//! Offline inference engine — wraps the C++ GGML backend via FFI.
//!
//! When the model file is absent (e.g. on first install) it falls back
//! to a pure-Rust rule-based completer so the shell never breaks.

use anyhow::{bail, Result};
use lru::LruCache;
use std::num::NonZeroUsize;
use crate::{CompletionReq, CompletionRes, CorrectionReq, CorrectionRes, Source};

// ─── LRU n-gram cache ─────────────────────────────────────────────────────────

/// Maps hash(last 512 chars of context) → top-k token predictions.
/// Prevents repeated inference on identical prefixes.
/// Time O(1) amortised, Space O(capacity * k * avg_token_len).
struct Cache {
    inner: LruCache<u64, Vec<(String, f32)>>,
}

impl Cache {
    fn new(cap: usize) -> Self {
        Self { inner: LruCache::new(NonZeroUsize::new(cap).unwrap()) }
    }

    fn get(&mut self, key: u64) -> Option<&Vec<(String, f32)>> {
        self.inner.get(&key)
    }

    fn put(&mut self, key: u64, val: Vec<(String, f32)>) {
        self.inner.put(key, val);
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let start = s.len().saturating_sub(512);
    s[start..].hash(&mut h);
    std::hash::Hasher::finish(&h)
}

// ─── OfflineEngine ────────────────────────────────────────────────────────────

pub struct OfflineEngine {
    model_path: String,
    loaded:     bool,
    cache:      Cache,
    temperature: f32,
}

impl OfflineEngine {
    /// Create the engine. Verifies the model file exists (does not load yet).
    pub fn new(model_path: &str, _ctx_tokens: u32, _gpu_layers: i32) -> Result<Self> {
        let loaded = std::path::Path::new(model_path).exists();
        if !loaded {
            log::warn!(
                "[offline] Model not found at {model_path}. \
                 Falling back to rule-based completions. \
                 Install with: npkg install nexus-ai-models"
            );
        } else {
            log::info!("[offline] Model found at {model_path} — ready");
        }
        Ok(Self {
            model_path: model_path.to_owned(),
            loaded,
            cache: Cache::new(4096),
            temperature: 0.1,
        })
    }

    pub fn is_ready(&self) -> bool { self.loaded }

    // ── Completion ────────────────────────────────────────────────────────

    pub async fn complete(&mut self, req: &CompletionReq, n: usize) -> Result<Vec<CompletionRes>> {
        if !self.loaded {
            return Ok(self.rule_complete(req, n));
        }

        let key = hash_str(&req.prefix);
        if let Some(cached) = self.cache.get(key) {
            return Ok(cached.iter().take(n).map(|(t, s)| CompletionRes {
                text: t.clone(), score: *s,
                source: Source::Offline, latency_ms: 0,
            }).collect());
        }

        // Run inference in blocking thread pool so we don't block the async runtime
        let path    = self.model_path.clone();
        let prefix  = req.prefix.clone();
        let lang    = req.language.clone().unwrap_or_default();
        let max_tok = req.max_tokens as u32;
        let temp    = self.temperature;

        let text = tokio::task::spawn_blocking(move || {
            run_ggml(&path, &prefix, &lang, max_tok, temp)
        }).await??;

        let results = vec![(text.trim().to_owned(), 0.82f32)];
        self.cache.put(key, results.clone());

        Ok(results.into_iter().take(n).map(|(t, s)| CompletionRes {
            text: t, score: s, source: Source::Offline, latency_ms: 0,
        }).collect())
    }

    // ── Next-word logits ──────────────────────────────────────────────────

    pub async fn top_k_next(&mut self, context: &str, k: usize) -> Result<Vec<(String, f32)>> {
        if !self.loaded {
            return Ok(self.rule_next(context, k));
        }

        let key = hash_str(context);
        if let Some(cached) = self.cache.get(key) {
            return Ok(cached.clone());
        }

        let path = self.model_path.clone();
        let ctx  = context.to_owned();
        let result = tokio::task::spawn_blocking(move || {
            run_ggml_topk(&path, &ctx, k as u32)
        }).await??;

        self.cache.put(key, result.clone());
        Ok(result)
    }

    // ── Error correction ──────────────────────────────────────────────────

    pub async fn correct(&mut self, req: &CorrectionReq) -> Result<CorrectionRes> {
        if !self.loaded {
            bail!("Offline model not loaded — cannot perform code correction");
        }
        let path  = self.model_path.clone();
        let code  = req.code.clone();
        let lang  = req.language.clone();
        let error = req.error.clone().unwrap_or_default();

        let fixed = tokio::task::spawn_blocking(move || {
            run_ggml_fix(&path, &code, &lang, &error)
        }).await??;

        Ok(CorrectionRes {
            fixed_code:  fixed.clone(),
            explanation: String::from("Fixed by local model"),
        })
    }

    // ── Rule-based fallback ───────────────────────────────────────────────

    fn rule_complete(&self, req: &CompletionReq, n: usize) -> Vec<CompletionRes> {
        let line = req.line.trim_end();
        let candidates: &[(&str, f32)] = if line.ends_with("fn ") {
            &[("main() {\n    \n}", 0.70), ("new() -> Self { Self {} }", 0.65)]
        } else if line.contains("Vec") && line.ends_with("::") {
            &[("new()", 0.90), ("with_capacity(", 0.85), ("from(", 0.70)]
        } else if line.contains("HashMap") && line.ends_with("::") {
            &[("new()", 0.90), ("with_capacity(", 0.83)]
        } else if line.ends_with("for ") {
            &[("item in iter {\n    \n}", 0.72), ("i in 0..n {\n    \n}", 0.68)]
        } else if line.ends_with("if ") {
            &[("let Some(x) = val {\n    \n}", 0.75), ("condition {\n    \n}", 0.65)]
        } else {
            &[]
        };
        candidates.iter().take(n).map(|(t, s)| CompletionRes {
            text: t.to_string(), score: *s,
            source: Source::Offline, latency_ms: 0,
        }).collect()
    }

    fn rule_next(&self, context: &str, k: usize) -> Vec<(String, f32)> {
        let last = context.split_whitespace().last().unwrap_or("");
        let preds: &[(&str, f32)] = match last {
            "let"    => &[("mut", 0.55), ("x", 0.30), ("result", 0.25)],
            "fn"     => &[("main", 0.40), ("new", 0.35), ("init", 0.25)],
            "for"    => &[("item", 0.45), ("i", 0.40), ("chunk", 0.15)],
            "if"     => &[("let", 0.50), ("condition", 0.30), ("self", 0.20)],
            "return" => &[("Ok(", 0.50), ("Some(", 0.35), ("None", 0.15)],
            "use"    => &[("std::", 0.45), ("crate::", 0.35), ("super::", 0.20)],
            _        => &[("let", 0.35), ("fn", 0.25), ("return", 0.25), ("for", 0.15)],
        };
        preds.iter().take(k).map(|(w, s)| ((*w).to_owned(), *s)).collect()
    }
}

// ─── FFI stubs (replaced by real C++ calls when libnexus_ai.so is linked) ────

/// Run GGML inference to generate a completion.
fn run_ggml(
    _model_path: &str,
    _prefix:     &str,
    _lang:       &str,
    _max_tokens: u32,
    _temp:       f32,
) -> Result<String> {
    // In a real build this calls:
    //   nexus_generate(model, prompt, max_tokens, temp, 0.95, 40, buf, buf_len)
    // via the C FFI in ffi.rs.
    bail!("GGML backend not linked (set feature = 'offline' and provide libnexus_ai.so)")
}

fn run_ggml_topk(_model_path: &str, _ctx: &str, _k: u32) -> Result<Vec<(String, f32)>> {
    bail!("GGML backend not linked")
}

fn run_ggml_fix(_model_path: &str, _code: &str, _lang: &str, _err: &str) -> Result<String> {
    bail!("GGML backend not linked")
}
