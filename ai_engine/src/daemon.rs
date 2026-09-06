//! AI Daemon — long-running process serving the kernel AI IPC channel
//!
//! Other processes (shell, editor, compositor) request completions via
//! the named IPC channel "ai.completion" instead of loading the model themselves.

use ai_engine::{AiEngine, Config, CompletionReq};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    log::info!("[aidaemon] Starting AI daemon");

    let config = Config {
        api_key:    std::env::var("NEXUS_AI_KEY").ok(),
        model_path: std::env::var("NEXUS_MODEL_PATH")
            .unwrap_or_else(|_| "/usr/share/nexus-os/models/phi3-mini-q4.gguf".into()),
        ..Config::default()
    };

    let engine = AiEngine::new(config).await?;
    log::info!("[aidaemon] Engine ready. Online: {}", engine.is_online());

    // TODO: listen on IPC socket and dispatch requests
    // For now, run a simple demo
    let req = CompletionReq {
        prefix:     "fn main() {\n    let x = Vec::".into(),
        line:       "    let x = Vec::".into(),
        language:   Some("rust".into()),
        max_tokens: 64,
        n:          5,
    };

    let results = engine.complete(&req).await?;
    for r in &results {
        log::info!("[aidaemon] Completion ({:?}, {:.2}): {:?}", r.source, r.score, r.text);
    }

    // In production: select! on IPC channel + shutdown signal
    tokio::signal::ctrl_c().await?;
    log::info!("[aidaemon] Shutting down");
    Ok(())
}
