use chrono::{DateTime, Utc};
use merkur_core::{Consolidator, Embedder, Forgetter, Storage};
use std::sync::Arc;

use crate::config::Config;

/// Shared application state. Wrapped in cheap clones — `Arc<dyn ...>` and
/// `Arc<Config>` make this O(1) on each request.
#[derive(Clone)]
pub struct AppState {
    pub embedder: Arc<dyn Embedder>,
    pub storage: Arc<dyn Storage>,
    pub consolidator: Arc<dyn Consolidator>,
    pub forgetter: Arc<dyn Forgetter>,
    pub config: Arc<Config>,
    pub started_at: DateTime<Utc>,
}

impl AppState {
    pub fn new(
        embedder: Arc<dyn Embedder>,
        storage: Arc<dyn Storage>,
        consolidator: Arc<dyn Consolidator>,
        forgetter: Arc<dyn Forgetter>,
        config: Arc<Config>,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            embedder,
            storage,
            consolidator,
            forgetter,
            config,
            started_at,
        }
    }
}
