mod app_state;
mod auth;
mod config;
mod error;
mod handlers;
mod router;
mod scheduler;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::{Context, Result};
use merkur_consolidators::{LlmConsolidator, NoopConsolidator};
use merkur_core::Consolidator;
use merkur_embedders::NoopEmbedder;
use merkur_forgetters::EbbinghausConfig;
use merkur_storage::SqliteStorage;
use tokio::sync::watch;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = parse_config_path();

    // Load config first with a fallback stderr writer so errors are at least
    // visible. Tracing is not installed until after we know the desired level
    // and format, guaranteeing a single subscriber registration over the
    // lifetime of the process.
    let cfg = match config::Config::load(config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e:#}");
            std::process::exit(1);
        }
    };

    init_tracing(&cfg.logging);

    info!(
        "MerkurDB v{} starting (host={}, port={})",
        env!("CARGO_PKG_VERSION"),
        cfg.server.host,
        cfg.server.port
    );

    let cfg = Arc::new(cfg);
    let embedder = build_embedder(&cfg).await?;
    let storage = build_storage(&cfg, embedder.dim()).await?;
    let consolidator = build_consolidator(&cfg)?;
    let forgetter = Arc::new(merkur_forgetters::EbbinghausForgetter::new(
        EbbinghausConfig {
            decay_factor: cfg.forgetting.decay_factor,
            half_life_seconds: cfg.forgetting.half_life_seconds,
            access_boost: cfg.forgetting.access_boost,
            threshold_to_l1: cfg.forgetting.threshold_to_l1,
            threshold_to_l0: cfg.forgetting.threshold_to_l0,
            threshold_archive: cfg.forgetting.threshold_archive,
        },
    ));

    // Build the scheduler with a shutdown signal.
    let (sched_shutdown_tx, sched_shutdown_rx) = watch::channel(false);
    let sched = Arc::new(scheduler::Scheduler::new(
        storage.clone(),
        consolidator.clone(),
        forgetter.clone(),
        std::time::Duration::from_secs(cfg.consolidation.interval_seconds),
        cfg.consolidation.batch_size,
        std::time::Duration::from_secs(cfg.forgetting.interval_seconds),
        cfg.forgetting.batch_size,
        cfg.forgetting.archive_days,
    ));
    let sched_handle = tokio::spawn({
        let sched = sched.clone();
        async move { sched.run(sched_shutdown_rx).await }
    });

    let started_at = chrono::Utc::now();
    let state = app_state::AppState::new(
        embedder,
        storage,
        consolidator,
        forgetter,
        cfg.clone(),
        started_at,
    );

    let cors = build_cors(&cfg.server.cors_allow_origin, cfg.server.dev_mode);
    let app = router::create_router(state).layer(cors);

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind {addr}"))?;
    info!("MerkurDB server listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Server error")?;

    info!("Server stopped accepting new connections; signalling scheduler");
    let _ = sched_shutdown_tx.send(true);
    if let Err(e) = sched_handle.await {
        error!("Scheduler join error: {e}");
    }
    info!("Shutdown complete");
    Ok(())
}

fn parse_config_path() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--config=") {
            return Some(rest.to_string());
        } else if arg == "--config" {
            return args.next();
        }
    }
    None
}

/// Initialize the tracing subscriber exactly once, honouring the operator's
/// explicit `RUST_LOG` if present and otherwise deriving the directive from
/// `config.logging`.
///
/// This function is deliberately side-effectful at startup only: tracing has a
/// single global subscriber per process, so any log record emitted before this
/// call is lost. Callers must invoke it after config has been parsed but before
/// any work that would `info!`/`warn!`/`error!`.
fn init_tracing(log: &config::LoggingConfig) {
    use tracing_subscriber::EnvFilter;

    let level = log.level.as_deref().unwrap_or("info");
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(format!("merkur_server={level},{level}"))
            .unwrap_or_else(|_| EnvFilter::new("merkur_server=info,info"))
    });

    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    let result = match log.format.as_deref() {
        Some("json") => builder.json().try_init(),
        _ => builder.try_init(),
    };
    if let Err(e) = result {
        // Re-initialization during tests is expected; in a real process this
        // indicates someone else installed a subscriber first (unusual for a
        // binary crate).
        eprintln!("tracing subscriber already installed: {e}");
    }
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return format!("{}/{}", home.to_string_lossy(), rest);
    }
    path.to_string()
}

async fn build_embedder(cfg: &config::Config) -> Result<Arc<dyn merkur_core::Embedder>> {
    match cfg.plugins.embedder.embedder_type.as_str() {
        #[cfg(feature = "ollama")]
        "ollama" => {
            let oc = cfg
                .plugins
                .embedder
                .ollama
                .as_ref()
                .context("Missing [plugins.embedder.ollama] config")?;
            let base = oc.base_url.as_deref().unwrap_or("http://localhost:11434");
            let model = oc.model.as_deref().unwrap_or("all-minilm");
            info!("Using OllamaEmbedder: url={base}, model={model}");
            let e = merkur_embedders::OllamaEmbedder::new(base, model)
                .await
                .context("Failed to initialize OllamaEmbedder")?;
            Ok(Arc::new(e))
        }
        #[cfg(feature = "openai")]
        "openai" => {
            let oc = cfg
                .plugins
                .embedder
                .openai
                .as_ref()
                .context("Missing [plugins.embedder.openai] config")?;
            let base = oc.base_url.as_deref().unwrap_or("https://api.openai.com");
            let api_key = oc.api_key.as_deref().context("Missing openai.api_key")?;
            let model = oc.model.as_deref().unwrap_or("text-embedding-3-small");
            info!("Using OpenAIEmbedder: url={base}, model={model}");
            let e = merkur_embedders::OpenAIEmbedder::new_with_dimensions(
                base,
                api_key,
                model,
                oc.dimensions,
            )
            .await
            .context("Failed to initialize OpenAIEmbedder")?;
            Ok(Arc::new(e))
        }
        "noop" => {
            let dim = cfg.embedding_dim_hint();
            info!("Using NoopEmbedder with dim={dim}");
            Ok(Arc::new(NoopEmbedder::new(dim)))
        }
        unknown => Err(anyhow::anyhow!("Unknown embedder type: {unknown}")),
    }
}

async fn build_storage(cfg: &config::Config, dim: usize) -> Result<Arc<dyn merkur_core::Storage>> {
    match cfg.storage.storage_type.as_str() {
        #[cfg(feature = "lancedb")]
        "lancedb" => {
            let lc = cfg
                .storage
                .lancedb
                .as_ref()
                .context("Missing [storage.lancedb] config")?;
            let lance_path = expand_tilde(&lc.lance_path);
            let sqlite_path = expand_tilde(&lc.sqlite_path);
            if let Some(parent) = std::path::Path::new(&sqlite_path).parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                warn!("Failed to create sqlite parent dir: {e}");
            }
            info!("Using LanceDbStorage: lance={lance_path}, sqlite={sqlite_path}");
            let s = merkur_storage::LanceDbStorage::new(&lance_path, &sqlite_path, dim)
                .await
                .context("Failed to initialize LanceDbStorage")?;
            Ok(Arc::new(s))
        }
        _ => {
            let db_path = expand_tilde(&cfg.storage.sqlite.path);
            if let Some(parent) = std::path::Path::new(&db_path).parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                warn!("Failed to create sqlite parent dir: {e}");
            }
            info!("Using SqliteStorage: path={db_path}");
            let s =
                SqliteStorage::new(&db_path, dim).context("Failed to initialize SqliteStorage")?;
            Ok(Arc::new(s))
        }
    }
}

fn build_consolidator(cfg: &config::Config) -> Result<Arc<dyn Consolidator>> {
    match cfg.plugins.consolidator.consolidator_type.as_str() {
        "llm" => {
            let lc = cfg
                .plugins
                .consolidator
                .llm
                .as_ref()
                .context("Missing [plugins.consolidator.llm] config")?;
            info!(
                "Using LlmConsolidator: base_url={}, model={}",
                lc.base_url, lc.model
            );
            Ok(Arc::new(LlmConsolidator::new(
                lc.base_url.clone(),
                lc.model.clone(),
            )?))
        }
        _ => {
            info!("Using NoopConsolidator");
            Ok(Arc::new(NoopConsolidator))
        }
    }
}

fn build_cors(allow_origin: &Option<String>, dev_mode: bool) -> CorsLayer {
    use axum::http::HeaderValue;
    let layer = CorsLayer::new();
    match allow_origin.as_deref() {
        Some("*") | Some("Any") | Some("any") if dev_mode => {
            warn!("CORS wildcard enabled because dev_mode=true; do not use in production");
            layer
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
        }
        Some(list) => {
            let parsed: Vec<HeaderValue> = list
                .split(',')
                .filter_map(|s| HeaderValue::from_str(s.trim()).ok())
                .collect();
            layer
                .allow_origin(AllowOrigin::list(parsed))
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
        }
        None => layer,
    }
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        error!("Ctrl+C handler install failed: {e}");
    }
    info!("Received shutdown signal");
}
