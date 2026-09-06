//! Online inference engine — remote API client
//!
//! Provider chain (tried in order):
//!   1. Anthropic Claude (best reasoning + code)
//!   2. OpenAI GPT-4o   (fallback)
//!   3. Local Ollama    (local HTTP server — bridge to offline models)
//!
//! Circuit breaker: after 3 consecutive failures the provider is
//! disabled for 30 seconds before being retried.

use anyhow::{bail, Result};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use reqwest::Client;
use crate::{CompletionReq, CompletionRes, CorrectionReq, CorrectionRes, Source};

// ─── Provider ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Provider { Anthropic, OpenAI, Ollama }

impl Provider {
    fn api_base(&self) -> &str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::OpenAI    => "https://api.openai.com",
            Self::Ollama    => "http://localhost:11434",
        }
    }
    fn model(&self) -> &str {
        match self {
            Self::Anthropic => "claude-opus-4-6",
            Self::OpenAI    => "gpt-4o",
            Self::Ollama    => "codellama",
        }
    }
}

// ─── Circuit breaker ──────────────────────────────────────────────────────────

struct CircuitBreaker {
    failures: u32,
    limit:    u32,
    open_until: Option<Instant>,
}

impl CircuitBreaker {
    fn new() -> Self { Self { failures: 0, limit: 3, open_until: None } }

    fn is_open(&mut self) -> bool {
        if let Some(until) = self.open_until {
            if Instant::now() >= until {
                // Half-open: allow one probe
                self.open_until = None;
                self.failures   = 0;
                false
            } else {
                true
            }
        } else {
            false
        }
    }

    fn success(&mut self) { self.failures = 0; self.open_until = None; }

    fn failure(&mut self) {
        self.failures += 1;
        if self.failures >= self.limit {
            self.open_until = Some(Instant::now() + Duration::from_secs(30));
        }
    }
}

// ─── OnlineEngine ─────────────────────────────────────────────────────────────

pub struct OnlineEngine {
    api_key:  Option<String>,
    client:   Client,
    breakers: Mutex<Vec<(Provider, CircuitBreaker)>>,
}

impl OnlineEngine {
    pub fn new(api_key: Option<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        let breakers = vec![
            (Provider::Anthropic, CircuitBreaker::new()),
            (Provider::OpenAI,    CircuitBreaker::new()),
            (Provider::Ollama,    CircuitBreaker::new()),
        ];

        Self { api_key, client, breakers: Mutex::new(breakers) }
    }

    /// Quick connectivity check. Returns true if any provider responds.
    pub async fn ping(&self) -> bool {
        // Try a HEAD request to the Anthropic API health endpoint
        match self.client
            .head("https://api.anthropic.com")
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(r) => r.status().as_u16() < 500,
            Err(_) => {
                // Try Ollama (local, doesn't need internet)
                self.client
                    .get("http://localhost:11434/api/tags")
                    .timeout(Duration::from_secs(1))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }
        }
    }

    // ── Completion ────────────────────────────────────────────────────────

    pub async fn complete(&self, req: &CompletionReq, n: usize) -> Result<Vec<CompletionRes>> {
        let providers = [Provider::Anthropic, Provider::OpenAI, Provider::Ollama];

        for provider in &providers {
            if self.breaker_open(provider) { continue; }

            match self.call_complete(provider, req, n).await {
                Ok(r) => {
                    self.breaker_success(provider);
                    return Ok(r);
                }
                Err(e) => {
                    log::warn!("[online] {:?} failed: {e}", provider);
                    self.breaker_failure(provider);
                }
            }
        }
        bail!("All online providers unavailable")
    }

    async fn call_complete(
        &self,
        provider: &Provider,
        req:      &CompletionReq,
        _n:       usize,
    ) -> Result<Vec<CompletionRes>> {
        let key = self.api_key.as_deref().unwrap_or("");
        let lang = req.language.as_deref().unwrap_or("text");
        let ctx  = &req.prefix[req.prefix.len().saturating_sub(2000)..];

        let (url, body, auth_header) = match provider {
            Provider::Anthropic => {
                let url  = format!("{}/v1/messages", provider.api_base());
                let body = serde_json::json!({
                    "model": provider.model(),
                    "max_tokens": req.max_tokens,
                    "system": format!(
                        "You are a code completion assistant. Complete the {lang} code. \
                         Return ONLY the completion text, no explanation, no markdown."
                    ),
                    "messages": [{"role":"user","content": format!("```{lang}\n{ctx}\n```")}]
                });
                (url, body, ("x-api-key", key.to_owned()))
            }
            Provider::OpenAI => {
                let url  = format!("{}/v1/chat/completions", provider.api_base());
                let body = serde_json::json!({
                    "model": provider.model(),
                    "max_tokens": req.max_tokens,
                    "messages": [
                        {"role":"system","content": format!("Complete {lang} code. Return completion only.")},
                        {"role":"user",  "content": format!("```{lang}\n{ctx}\n```")}
                    ]
                });
                (url, body, ("Authorization", format!("Bearer {key}")))
            }
            Provider::Ollama => {
                let url  = format!("{}/api/generate", provider.api_base());
                let body = serde_json::json!({
                    "model":  provider.model(),
                    "prompt": format!("Complete this {lang} code:\n{ctx}"),
                    "stream": false
                });
                (url, body, ("Content-Type", "application/json".to_owned()))
            }
        };

        let resp: serde_json::Value = self.client
            .post(&url)
            .header(auth_header.0, auth_header.1)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let text = extract_text(&resp, provider);
        Ok(vec![CompletionRes {
            text: text.trim().to_owned(),
            score: 0.92,
            source: Source::Online,
            latency_ms: 0,
        }])
    }

    // ── Correction ────────────────────────────────────────────────────────

    pub async fn correct(&self, req: &CorrectionReq) -> Result<CorrectionRes> {
        let providers = [Provider::Anthropic, Provider::OpenAI];
        for provider in &providers {
            if self.breaker_open(provider) { continue; }
            match self.call_correct(provider, req).await {
                Ok(r) => { self.breaker_success(provider); return Ok(r); }
                Err(e) => { log::warn!("[online] correct {:?}: {e}", provider); self.breaker_failure(provider); }
            }
        }
        bail!("All online providers unavailable for correction")
    }

    async fn call_correct(&self, provider: &Provider, req: &CorrectionReq) -> Result<CorrectionRes> {
        let key  = self.api_key.as_deref().unwrap_or("");
        let err  = req.error.as_deref().unwrap_or("unknown error");
        let prompt = format!(
            "Fix this {} code.\nError: {}\n\nCode:\n```{}\n{}\n```\n\
             Reply with ONLY a JSON object: {{\"fixed\":<string>,\"explanation\":<string>}}",
            req.language, err, req.language, req.code
        );

        let (url, body, auth) = match provider {
            Provider::Anthropic => (
                format!("{}/v1/messages", provider.api_base()),
                serde_json::json!({
                    "model": provider.model(), "max_tokens": 1024,
                    "messages": [{"role":"user","content": prompt}]
                }),
                ("x-api-key", key.to_owned()),
            ),
            _ => (
                format!("{}/v1/chat/completions", provider.api_base()),
                serde_json::json!({
                    "model": provider.model(), "max_tokens": 1024,
                    "messages":[{"role":"user","content": prompt}]
                }),
                ("Authorization", format!("Bearer {key}")),
            ),
        };

        let resp: serde_json::Value = self.client
            .post(&url)
            .header(auth.0, auth.1)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send().await?
            .error_for_status()?.json().await?;

        let raw  = extract_text(&resp, provider);
        let json: serde_json::Value = serde_json::from_str(raw.trim())?;
        Ok(CorrectionRes {
            fixed_code:  json["fixed"].as_str().unwrap_or(&req.code).to_owned(),
            explanation: json["explanation"].as_str().unwrap_or("").to_owned(),
        })
    }

    // ── Circuit breaker helpers ───────────────────────────────────────────

    fn breaker_open(&self, p: &Provider) -> bool {
        self.breakers.lock().unwrap()
            .iter_mut().find(|(prov, _)| prov == p)
            .map_or(false, |(_, b)| b.is_open())
    }
    fn breaker_success(&self, p: &Provider) {
        if let Some((_, b)) = self.breakers.lock().unwrap()
            .iter_mut().find(|(prov, _)| prov == p) { b.success(); }
    }
    fn breaker_failure(&self, p: &Provider) {
        if let Some((_, b)) = self.breakers.lock().unwrap()
            .iter_mut().find(|(prov, _)| prov == p) { b.failure(); }
    }
}

// ─── Response extraction ──────────────────────────────────────────────────────

fn extract_text(resp: &serde_json::Value, provider: &Provider) -> String {
    match provider {
        Provider::Anthropic =>
            resp["content"][0]["text"].as_str().unwrap_or("").to_owned(),
        Provider::OpenAI =>
            resp["choices"][0]["message"]["content"].as_str().unwrap_or("").to_owned(),
        Provider::Ollama =>
            resp["response"].as_str().unwrap_or("").to_owned(),
    }
}
