use chrono::{DateTime, Utc};
use merkur_core::{Consolidator, Embedder, Forgetter, Storage};
use std::sync::Arc;

use crate::config::Config;
use crate::rate_limit::GlobalLimiter;

#[derive(Clone)]
pub struct AppState {
    pub embedder: Arc<dyn Embedder>,
    pub storage: Arc<dyn Storage>,
    pub consolidator: Arc<dyn Consolidator>,
    pub forgetter: Arc<dyn Forgetter>,
    pub config: Arc<Config>,
    pub started_at: DateTime<Utc>,
    pub rate_limiter: Option<Arc<GlobalLimiter>>,
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
        let rate_limiter = if config.rate_limit.enabled {
            Some(crate::rate_limit::build_limiter(
                config.rate_limit.requests_per_second,
            ))
        } else {
            None
        };
        Self {
            embedder,
            storage,
            consolidator,
            forgetter,
            config,
            started_at,
            rate_limiter,
        }
    }
}
