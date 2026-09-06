//! Noxisa AI Engine
//!
//! Three-stage completion pipeline:
//!   Stage 0 — Trie        < 1 ms   always available, no network
//!   Stage 1 — Offline LLM < 100 ms  local GGUF model, no network
//!   Stage 2 — Online API  < 500 ms  Claude/GPT-4o, needs internet
//!
//! The `AiEngine` struct orchestrates all three, choosing automatically.

pub mod offline;
pub mod online;
pub mod trie;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::Mutex;

pub use trie::CompletionTrie;
pub use offline::OfflineEngine;
pub use online::OnlineEngine;

// ─── Public request / response types ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompletionReq {
    /// Full file text up to cursor position
    pub prefix:     String,
    /// Current line up to cursor (for single-line suggestions)
    pub line:       String,
    /// Programming language hint ("rust", "python", "shell", …)
    pub language:   Option<String>,
    /// Max tokens to generate
    pub max_tokens: usize,
    /// How many completions to return
    pub n:          usize,
}

#[derive(Debug, Clone)]
pub struct CompletionRes {
    /// Text to insert after the cursor
    pub text:       String,
    /// Confidence 0.0 – 1.0
    pub score:      f32,
    /// Which stage produced this result
    pub source:     Source,
    /// Wall-clock latency in milliseconds
    pub latency_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source { Trie, Offline, Online }

#[derive(Debug, Clone)]
pub struct CorrectionReq {
    pub code:     String,
    pub language: String,
    /// Optional compiler/linter error message
    pub error:    Option<String>,
}

#[derive(Debug, Clone)]
pub struct CorrectionRes {
    pub fixed_code:  String,
    pub explanation: String,
}

// ─── Engine config ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    /// Prefer online model when internet is available
    pub prefer_online:    bool,
    /// Fall back to offline if online fails
    pub offline_fallback: bool,
    /// Always run trie first pass
    pub trie_first:       bool,
    /// API key for online providers (Anthropic / OpenAI)
    pub api_key:          Option<String>,
    /// Path to local GGUF model file
    pub model_path:       String,
    /// Context window for local model (tokens)
    pub ctx_tokens:       u32,
    /// GPU layers to offload (-1 = auto, 0 = CPU only)
    pub gpu_layers:       i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefer_online:    true,
            offline_fallback: true,
            trie_first:       true,
            api_key:          std::env::var("NEXUS_AI_KEY").ok(),
            model_path:       "/usr/share/nexus-os/models/phi3-mini-q4.gguf".into(),
            ctx_tokens:       2048,
            gpu_layers:       -1,
        }
    }
}

// ─── AI Engine ────────────────────────────────────────────────────────────────

pub struct AiEngine {
    trie:      Arc<RwLock<CompletionTrie>>,
    offline:   Arc<Mutex<OfflineEngine>>,
    online:    Arc<OnlineEngine>,
    online_ok: Arc<AtomicBool>,
    config:    Config,
}

impl AiEngine {
    /// Create and initialise the engine.
    /// Loads offline model eagerly; probes connectivity in the background.
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        let trie = Arc::new(RwLock::new(CompletionTrie::new()));
        trie.write().load_builtins();

        let offline  = Arc::new(Mutex::new(
            OfflineEngine::new(&config.model_path, config.ctx_tokens, config.gpu_layers)?
        ));
        let online   = Arc::new(OnlineEngine::new(config.api_key.clone()));
        let online_ok = Arc::new(AtomicBool::new(false));

        // Background connectivity probe every 15 s
        {
            let ok  = online_ok.clone();
            let eng = online.clone();
            tokio::spawn(async move {
                loop {
                    let reachable = eng.ping().await;
                    ok.store(reachable, Ordering::Relaxed);
                    tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
                }
            });
        }

        Ok(Arc::new(Self { trie, offline, online, online_ok, config }))
    }

    /// Main completion entry point. Returns up to `req.n` suggestions sorted by score.
    pub async fn complete(&self, req: &CompletionReq) -> Result<Vec<CompletionRes>> {
        let mut results: Vec<CompletionRes> = Vec::new();

        // ── Stage 0: Trie (instant, always) ──────────────────────────────
        if self.config.trie_first {
            let t0   = std::time::Instant::now();
            let hits = self.trie.read().complete(&req.line, req.n);
            let ms   = t0.elapsed().as_millis() as u32;
            results.extend(hits.into_iter().map(|(text, score)| CompletionRes {
                text, score, source: Source::Trie, latency_ms: ms,
            }));
            if results.len() >= req.n {
                results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
                return Ok(results);
            }
        }

        // ── Stage 1 / 2: LLM ────────────────────────────────────────────
        let remaining = req.n.saturating_sub(results.len());
        let use_online = self.config.prefer_online && self.online_ok.load(Ordering::Relaxed);

        if use_online {
            match self.online.complete(req, remaining).await {
                Ok(mut r) => results.append(&mut r),
                Err(e) => {
                    log::warn!("Online completion failed: {e}");
                    if self.config.offline_fallback {
                        let mut r = self.offline.lock().await.complete(req, remaining).await?;
                        results.append(&mut r);
                    }
                }
            }
        } else {
            let mut r = self.offline.lock().await.complete(req, remaining).await?;
            results.append(&mut r);
        }

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        results.truncate(req.n);
        Ok(results)
    }

    /// Get top-k next-word predictions — used for ghost text.
    pub async fn next_word(&self, context: &str, k: usize) -> Vec<(String, f32)> {
        // Trie first
        let trie_res = self.trie.read().complete(context, k);
        if !trie_res.is_empty() { return trie_res; }

        // Offline model token logits
        self.offline.lock().await
            .top_k_next(context, k).await
            .unwrap_or_default()
    }

    /// Fix code errors (Gemini-in-Colab style).
    pub async fn correct(&self, req: &CorrectionReq) -> Result<CorrectionRes> {
        if self.config.prefer_online && self.online_ok.load(Ordering::Relaxed) {
            match self.online.correct(req).await {
                Ok(r) => return Ok(r),
                Err(e) => log::warn!("Online correction failed: {e}"),
            }
        }
        self.offline.lock().await.correct(req).await
    }

    /// Learn a new identifier from the user's current file.
    /// Called on every save or keystroke.
    pub fn learn(&self, identifier: &str) {
        self.trie.write().learn(identifier);
    }

    /// Returns true if the online model is currently reachable.
    pub fn is_online(&self) -> bool {
        self.online_ok.load(Ordering::Relaxed)
    }
}
