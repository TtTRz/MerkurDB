mod app_state;
mod config;
mod handlers;
mod router;
mod scheduler;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use merkur_consolidators::NoopConsolidator;
use merkur_core::Consolidator;
use merkur_embedders::NoopEmbedder;
use merkur_forgetters::EbbinghausConfig;
use merkur_storage::SqliteStorage;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "merkur_server=info".into()),
        )
        .init();

    let config_path = std::env::args().nth(1).and_then(|arg| {
        if arg.starts_with("--config=") {
            Some(arg.trim_start_matches("--config=").to_string())
        } else if arg == "--config" {
            std::env::args().nth(2)
        } else {
            None
        }
    });

    let config = config::Config::load(config_path.as_deref()).expect("Failed to load config");

    let embedder: Arc<dyn merkur_core::Embedder> =
        match config.plugins.embedder.embedder_type.as_str() {
            #[cfg(feature = "ollama")]
            "ollama" => {
                let ollama = config
                    .plugins
                    .embedder
                    .ollama
                    .as_ref()
                    .expect("Missing [plugins.embedder.ollama] config");
                let base_url = ollama
                    .base_url
                    .as_deref()
                    .unwrap_or("http://localhost:11434");
                let model = ollama.model.as_deref().unwrap_or("all-minilm");
                info!("Using OllamaEmbedder: url={base_url}, model={model}");
                let e = merkur_embedders::OllamaEmbedder::new(base_url, model)
                    .await
                    .expect("Failed to initialize OllamaEmbedder");
                Arc::new(e)
            }
            #[cfg(feature = "openai")]
            "openai" => {
                let openai = config
                    .plugins
                    .embedder
                    .openai
                    .as_ref()
                    .expect("Missing [plugins.embedder.openai] config");
                let base_url = openai
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com");
                let api_key = openai.api_key.as_deref().expect("Missing openai.api_key");
                let model = openai.model.as_deref().unwrap_or("text-embedding-3-small");
                info!("Using OpenAIEmbedder: url={base_url}, model={model}");
                let e = merkur_embedders::OpenAIEmbedder::new(base_url, api_key, model)
                    .await
                    .expect("Failed to initialize OpenAIEmbedder");
                Arc::new(e)
            }
            "noop" => {
                let dim = config.embedding_dim();
                info!("Using NoopEmbedder with dim={dim}");
                Arc::new(NoopEmbedder::new(dim))
            }
            unknown => panic!("Unknown embedder type: {unknown}"),
        };

    fn expand_tilde(path: &str) -> String {
        if path.starts_with("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return format!("{}{}", home.to_string_lossy(), &path[1..]);
        }
        path.to_string()
    }

    let storage: Arc<dyn merkur_core::Storage> = match config.storage.storage_type.as_str() {
        #[cfg(feature = "lancedb")]
        "lancedb" => {
            let lancedb_cfg = config
                .storage
                .lancedb
                .as_ref()
                .expect("Missing [storage.lancedb] config");
            let lance_path = expand_tilde(&lancedb_cfg.lance_path);
            let sqlite_path = expand_tilde(&lancedb_cfg.sqlite_path);
            if let Some(parent) = std::path::Path::new(&sqlite_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            info!("Using LanceDbStorage: lance={lance_path}, sqlite={sqlite_path}");
            let s = merkur_storage::LanceDbStorage::new(&lance_path, &sqlite_path, embedder.dim())
                .await
                .expect("Failed to initialize LanceDbStorage");
            Arc::new(s)
        }
        _ => {
            // Default: SQLite with in-memory vector index
            let db_path = expand_tilde(&config.storage.sqlite.path);
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            info!("Using SqliteStorage: path={db_path}");
            Arc::new(
                SqliteStorage::new(&db_path, embedder.dim()).expect("Failed to initialize storage"),
            )
        }
    };

    let ebbinghaus_config = EbbinghausConfig {
        decay_factor: config.forgetting.decay_factor,
        half_life_seconds: config.forgetting.half_life_seconds,
        access_boost: config.forgetting.access_boost,
        threshold_to_l1: config.forgetting.threshold_to_l1,
        threshold_to_l0: config.forgetting.threshold_to_l0,
        threshold_archive: config.forgetting.threshold_archive,
    };

    let consolidator: Arc<dyn Consolidator> = Arc::new(NoopConsolidator);

    let forgetter: Arc<dyn merkur_core::Forgetter> = Arc::new(
        merkur_forgetters::EbbinghausForgetter::new(ebbinghaus_config),
    );

    let scheduler_storage: Arc<dyn merkur_core::Storage> = storage.clone();

    let sched = Arc::new(scheduler::Scheduler::new(
        scheduler_storage,
        consolidator.clone(),
        forgetter.clone(),
        std::time::Duration::from_secs(config.consolidation.interval_seconds),
        config.consolidation.batch_size,
        std::time::Duration::from_secs(config.forgetting.interval_seconds),
        config.forgetting.batch_size,
        config.forgetting.archive_days,
    ));

    let sched_handle = tokio::spawn(async move {
        sched.run().await;
    });

    let started_at = chrono::Utc::now();
    let state_storage: Arc<dyn merkur_core::Storage> = storage;
    let state = app_state::AppState::new(
        embedder,
        state_storage,
        consolidator,
        forgetter,
        config.clone(),
        started_at,
    );

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = router::create_router().with_state(state).layer(cors);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    info!("MerkurDB server listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Server error");

    info!("Server shutting down");
    sched_handle.abort();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install Ctrl+C handler");
    info!("Received shutdown signal");
}
