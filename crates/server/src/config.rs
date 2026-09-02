use figment::Figment;
use figment::providers::{Env, Format, Yaml};
use merkur_core::{MerkurError, MerkurResult};
use serde::Deserialize;

/// Top-level configuration. The default order of precedence (highest first) is:
///
///     command-line --config YAML  >  MERKUR_* env vars  >  embedded defaults
///
/// `MERKUR_*` env vars use **double underscore** as the level separator
/// (e.g. `MERKUR_FORGETTING__HALF_LIFE_SECONDS=86400`) so that single
/// underscores inside field names like `half_life_seconds` are preserved.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    #[serde(default)]
    pub forgetting: ForgettingConfig,
    #[serde(default)]
    pub write: WriteConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Comma-separated list of allowed CORS origins, or `*` to allow all (the
    /// latter is rejected by `validate()` unless `dev_mode` is also set).
    #[serde(default)]
    pub cors_allow_origin: Option<String>,
    #[serde(default)]
    pub dev_mode: bool,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    1934
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    #[serde(rename = "type")]
    pub storage_type: String,
    pub sqlite: SqliteConfig,
    #[cfg_attr(not(feature = "lancedb"), allow(dead_code))]
    pub lancedb: Option<LanceDbConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqliteConfig {
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "lancedb"), allow(dead_code))]
pub struct LanceDbConfig {
    pub lance_path: String,
    pub sqlite_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginsConfig {
    pub embedder: EmbedderConfig,
    #[serde(default)]
    pub consolidator: ConsolidatorConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedderConfig {
    #[serde(rename = "type")]
    pub embedder_type: String,
    #[cfg_attr(not(feature = "ollama"), allow(dead_code))]
    pub ollama: Option<OllamaConfig>,
    #[cfg_attr(not(feature = "openai"), allow(dead_code))]
    pub openai: Option<OpenAIConfig>,
    pub noop: Option<NoopConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConsolidatorConfig {
    /// One of: "noop" (default) or "llm".
    #[serde(rename = "type", default = "default_consolidator")]
    pub consolidator_type: String,
    pub llm: Option<LlmConsolidatorConfig>,
}

fn default_consolidator() -> String {
    "noop".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConsolidatorConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_llm_backend")]
    pub backend: String,
    /// Bearer token for hosted providers (DeepSeek, OpenRouter). Prefer the
    /// MERKUR_PLUGINS__CONSOLIDATOR__LLM__API_KEY env var over the file.
    pub api_key: Option<String>,
    /// Per-call timeout seconds (default 120). Reasoning models on large
    /// batches can need much longer.
    pub timeout_seconds: Option<u64>,
}

fn default_llm_backend() -> String {
    "ollama".to_string()
}

impl LlmConsolidatorConfig {
    pub fn backend(&self) -> merkur_consolidators::LlmBackend {
        match self.backend.as_str() {
            "openai" => merkur_consolidators::LlmBackend::OpenAI,
            _ => merkur_consolidators::LlmBackend::Ollama,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "ollama"), allow(dead_code))]
pub struct OllamaConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(not(feature = "openai"), allow(dead_code))]
pub struct OpenAIConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub dimensions: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NoopConfig {
    pub dim: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RetrievalConfig {
    pub fast_default_limit: Option<usize>,
    pub score_threshold: Option<f64>,
    pub default_depth: Option<usize>,
    pub default_degree_limit: Option<usize>,
    pub fusion: Option<FusionConfig>,
}

/// Hybrid-retrieval fusion tuning (P1-5). Every field is optional and falls
/// back to the pre-P1-5 behavior (symmetric channels, k=60, 0.5/0.2/0.3).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FusionConfig {
    pub rrf_k: Option<f64>,
    pub bm25_weight: Option<f64>,
    pub vector_weight: Option<f64>,
    pub score_search: Option<f64>,
    pub score_weight: Option<f64>,
    pub score_importance: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    pub level: Option<String>,
    /// "text" (default) or "json".
    pub format: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthConfig {
    /// API tokens accepted as `Authorization: Bearer <token>`. If empty, the
    /// service refuses to start in non-dev mode unless `disabled` is true.
    #[serde(default)]
    pub tokens: Vec<String>,
    /// Set to `true` to explicitly run without authentication. Combine with
    /// `server.dev_mode = true` to bind 0.0.0.0 or use `*` CORS.
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_rps")]
    pub requests_per_second: u32,
    #[serde(default)]
    pub enabled: bool,
}

fn default_rps() -> u32 {
    100
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_second: default_rps(),
            enabled: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConsolidationConfig {
    #[serde(default = "default_consolidation_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_consolidation_batch")]
    pub batch_size: usize,
    /// Write governance (P1-7): LLM UPDATE/DELETE verdicts execute only when
    /// the pair's cosine similarity clears this floor — the second signal of
    /// the dual-signal rule. 0.6 is deliberately loose (below the 0.92 dedup
    /// bar); the LLM verdict carries the semantic weight.
    #[serde(default = "default_adjudication_floor")]
    pub adjudication_floor: f64,
    /// Nearest-neighbor candidates the adjudicator judges per pending memory.
    #[serde(default = "default_adjudication_candidates")]
    pub adjudication_candidates: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForgettingConfig {
    #[serde(default = "default_forgetting_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_forgetting_batch")]
    pub batch_size: usize,
    #[serde(default = "default_archive_days")]
    pub archive_days: i32,
    /// Days an invalidated (soft-deleted) memory is kept for audit before the
    /// forgetting tick hard-deletes it (P1-7, Q7).
    #[serde(default = "default_purge_invalidated_days")]
    pub purge_invalidated_days: i32,
    #[serde(default = "default_decay_factor")]
    pub decay_factor: f64,
    #[serde(default = "default_half_life_seconds")]
    pub half_life_seconds: f64,
    #[serde(default = "default_access_boost")]
    pub access_boost: f64,
    #[serde(default = "default_threshold_to_l1")]
    pub threshold_to_l1: f64,
    #[serde(default = "default_threshold_to_l0")]
    pub threshold_to_l0: f64,
    #[serde(default = "default_threshold_archive")]
    pub threshold_archive: f64,
    #[serde(default = "default_threshold_upgrade")]
    pub threshold_upgrade: f64,
    #[serde(default = "default_upgrade_min_access_count")]
    pub upgrade_min_access_count: u64,
}

/// Write-time dedup (P2-8).
#[derive(Debug, Clone, Deserialize)]
pub struct WriteConfig {
    /// Top-1 cosine similarity above which a write NOOPs onto the existing
    /// memory. `0.92` mirrors mem0's published threshold.
    #[serde(default = "default_dedup_threshold")]
    pub dedup_threshold: f64,
    /// Master switch; disabling restores plain insert behavior.
    #[serde(default = "default_dedup_enabled")]
    pub dedup_enabled: bool,
    /// Reserved write-governance mode switch. The P1-7 decision: UPDATE/DELETE
    /// adjudication runs in the async Consolidator only, keeping the
    /// synchronous write path LLM-free. Only "async" is accepted today; the
    /// field exists so a future "sync" mode does not break config compat.
    #[serde(default = "default_adjudication_mode")]
    pub adjudication: String,
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self {
            dedup_threshold: 0.92,
            dedup_enabled: true,
            adjudication: "async".into(),
        }
    }
}

fn default_dedup_threshold() -> f64 {
    0.92
}
fn default_dedup_enabled() -> bool {
    true
}
fn default_adjudication_mode() -> String {
    "async".into()
}

// Serde requires free functions for field-level defaults. Keep them named for
// the semantic they encode, not the literal they return — magic-number-style
// names like `default_0_1` read as line noise in diffs.
fn default_consolidation_interval() -> u64 {
    60
}
fn default_consolidation_batch() -> usize {
    10
}
fn default_adjudication_floor() -> f64 {
    0.6
}
fn default_adjudication_candidates() -> usize {
    5
}
fn default_forgetting_interval() -> u64 {
    300
}
fn default_forgetting_batch() -> usize {
    100
}
fn default_archive_days() -> i32 {
    30
}
fn default_purge_invalidated_days() -> i32 {
    30
}
fn default_decay_factor() -> f64 {
    0.9
}
fn default_half_life_seconds() -> f64 {
    86_400.0
}
fn default_access_boost() -> f64 {
    0.1
}
fn default_threshold_to_l1() -> f64 {
    0.3
}
fn default_threshold_to_l0() -> f64 {
    0.2
}
fn default_threshold_archive() -> f64 {
    0.1
}
fn default_threshold_upgrade() -> f64 {
    0.6
}
fn default_upgrade_min_access_count() -> u64 {
    3
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 60,
            batch_size: 10,
            adjudication_floor: 0.6,
            adjudication_candidates: 5,
        }
    }
}

impl Default for ForgettingConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 300,
            batch_size: 100,
            archive_days: 30,
            purge_invalidated_days: 30,
            decay_factor: 0.9,
            half_life_seconds: 86400.0,
            access_boost: 0.1,
            threshold_to_l1: 0.3,
            threshold_to_l0: 0.2,
            threshold_archive: 0.1,
            threshold_upgrade: 0.6,
            upgrade_min_access_count: 3,
        }
    }
}

const BUILT_IN_DEFAULTS: &str = r#"
server:
  host: "127.0.0.1"
  port: 1934
  dev_mode: false
storage:
  type: "sqlite"
  sqlite:
    path: "~/.merkur/data/merkur.db"
plugins:
  embedder:
    type: "noop"
    noop:
      dim: 384
  consolidator:
    type: "noop"
retrieval:
  fast_default_limit: 10
  score_threshold: 0.3
  default_depth: 2
  default_degree_limit: 10
logging:
  level: "info"
  format: "text"
auth:
  tokens: []
  disabled: false
"#;

impl Config {
    /// Load configuration with precedence: defaults < env < yaml.
    pub fn load(config_path: Option<&str>) -> MerkurResult<Self> {
        let mut fig = Figment::new()
            .merge(Yaml::string(BUILT_IN_DEFAULTS))
            .merge(Env::prefixed("MERKUR_").split("__"));
        if let Some(p) = config_path {
            fig = fig.merge(Yaml::file(p));
        }
        let cfg: Config = fig
            .extract()
            .map_err(|e| MerkurError::Config(format!("failed to load config: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate semantic constraints not expressed by the type system.
    pub fn validate(&self) -> MerkurResult<()> {
        if self.server.port == 0 {
            return Err(MerkurError::Config(
                "server.port=0 is only valid in tests".into(),
            ));
        }
        if self.forgetting.half_life_seconds <= 0.0 {
            return Err(MerkurError::Config(
                "forgetting.half_life_seconds must be > 0".into(),
            ));
        }
        if self.forgetting.archive_days < 0 {
            return Err(MerkurError::Config(
                "forgetting.archive_days must be >= 0".into(),
            ));
        }
        if self.forgetting.purge_invalidated_days < 0 {
            return Err(MerkurError::Config(
                "forgetting.purge_invalidated_days must be >= 0".into(),
            ));
        }
        if let Some(dim) = self.plugins.embedder.noop.as_ref().and_then(|n| n.dim)
            && dim == 0
        {
            return Err(MerkurError::Config(
                "plugins.embedder.noop.dim must be > 0".into(),
            ));
        }
        if let Some(t) = &self.retrieval.score_threshold
            && (*t < -1.0 || *t > 1.0)
        {
            return Err(MerkurError::Config(
                "retrieval.score_threshold must be in [-1, 1]".into(),
            ));
        }
        if self.write.dedup_threshold <= 0.0 || self.write.dedup_threshold > 1.0 {
            // 0.0 would NOOP nearly every embedded write onto an unrelated
            // memory; >1.0 silently disables dedup.
            return Err(MerkurError::Config(
                "write.dedup_threshold must be in (0, 1]".into(),
            ));
        }
        if self.write.adjudication != "async" {
            return Err(MerkurError::Config(
                "write.adjudication is reserved and only \"async\" is supported today".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.consolidation.adjudication_floor) {
            return Err(MerkurError::Config(
                "consolidation.adjudication_floor must be in [0, 1]".into(),
            ));
        }
        if let Some(f) = &self.retrieval.fusion {
            if let Some(k) = f.rrf_k
                && k <= 0.0
            {
                return Err(MerkurError::Config(
                    "retrieval.fusion.rrf_k must be > 0".into(),
                ));
            }
            let bw = f.bm25_weight.unwrap_or(1.0);
            let vw = f.vector_weight.unwrap_or(1.0);
            if bw < 0.0 || vw < 0.0 || (bw == 0.0 && vw == 0.0) {
                return Err(MerkurError::Config(
                    "retrieval.fusion channel weights must be >= 0 and not both zero".into(),
                ));
            }
            for (name, v) in [
                ("score_search", f.score_search),
                ("score_weight", f.score_weight),
                ("score_importance", f.score_importance),
            ] {
                if let Some(v) = v
                    && v < 0.0
                {
                    return Err(MerkurError::Config(format!(
                        "retrieval.fusion.{name} must be >= 0"
                    )));
                }
            }
        }
        if self.forgetting.threshold_upgrade <= self.forgetting.threshold_to_l1 {
            return Err(MerkurError::Config(format!(
                "forgetting.threshold_upgrade ({}) must exceed threshold_to_l1 ({}) — otherwise memories oscillate across the boundary on every tick",
                self.forgetting.threshold_upgrade, self.forgetting.threshold_to_l1
            )));
        }
        // Production safety: refuse to start with `*` CORS and no tokens unless
        // dev_mode is explicitly enabled.
        let cors_is_wildcard = matches!(
            self.server.cors_allow_origin.as_deref(),
            Some("*") | Some("Any") | Some("any")
        );
        let no_auth = self.auth.tokens.is_empty() && !self.auth.disabled;
        if !self.server.dev_mode && (cors_is_wildcard || no_auth) {
            return Err(MerkurError::Config(
                "Refusing to start: configure auth.tokens, restrict cors_allow_origin, or set server.dev_mode=true".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn test_config() -> Self {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 1934
  dev_mode: true
storage:
  type: "sqlite"
  sqlite:
    path: "file::memory:?cache=shared"
plugins:
  embedder:
    type: "noop"
    noop:
      dim: 16
  consolidator:
    type: "noop"
auth:
  disabled: true
"#;
        Figment::new()
            .merge(Yaml::string(BUILT_IN_DEFAULTS))
            .merge(Yaml::string(yaml))
            .extract()
            .expect("Failed to load test config")
    }

    pub fn embedding_dim_hint(&self) -> usize {
        self.plugins
            .embedder
            .noop
            .as_ref()
            .and_then(|n| n.dim)
            .unwrap_or(384)
    }

    pub fn fast_limit(&self) -> usize {
        self.retrieval.fast_default_limit.unwrap_or(10)
    }

    pub fn score_threshold(&self) -> f64 {
        self.retrieval.score_threshold.unwrap_or(0.3)
    }

    pub fn default_depth(&self) -> usize {
        self.retrieval.default_depth.unwrap_or(2)
    }

    pub fn default_degree_limit(&self) -> usize {
        self.retrieval.default_degree_limit.unwrap_or(10)
    }

    pub fn fusion_params(&self) -> merkur_core::FusionParams {
        let mut p = merkur_core::FusionParams::default();
        if let Some(f) = &self.retrieval.fusion {
            p.rrf_k = f.rrf_k.unwrap_or(p.rrf_k);
            p.bm25_weight = f.bm25_weight.unwrap_or(p.bm25_weight);
            p.vector_weight = f.vector_weight.unwrap_or(p.vector_weight);
            p.score.search = f.score_search.unwrap_or(p.score.search);
            p.score.weight = f.score_weight.unwrap_or(p.score.weight);
            p.score.importance = f.score_importance.unwrap_or(p.score.importance);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validates() {
        assert!(Config::test_config().validate().is_ok());
    }

    // ── retrieval.fusion (P1-5) ──

    #[test]
    fn fusion_params_default_to_legacy_behavior() {
        let p = Config::test_config().fusion_params();
        assert_eq!(p.rrf_k, 60.0);
        assert_eq!(p.bm25_weight, 1.0);
        assert_eq!(p.vector_weight, 1.0);
        assert_eq!(p.score.search, 0.5);
        assert_eq!(p.score.weight, 0.2);
        assert_eq!(p.score.importance, 0.3);
    }

    #[test]
    fn fusion_params_pick_up_config_overrides() {
        let mut cfg = Config::test_config();
        cfg.retrieval.fusion = Some(FusionConfig {
            rrf_k: Some(20.0),
            bm25_weight: Some(1.5),
            vector_weight: None,
            score_search: Some(0.8),
            score_weight: Some(0.1),
            score_importance: Some(0.1),
        });
        let p = cfg.fusion_params();
        assert_eq!(p.rrf_k, 20.0);
        assert_eq!(p.bm25_weight, 1.5);
        assert_eq!(p.vector_weight, 1.0); // untouched field falls back
        assert_eq!(p.score.search, 0.8);
    }

    #[test]
    fn fusion_validation_rejects_degenerate_configs() {
        let mut cfg = Config::test_config();
        cfg.retrieval.fusion = Some(FusionConfig {
            rrf_k: Some(0.0),
            ..Default::default()
        });
        assert!(cfg.validate().is_err());

        let mut cfg = Config::test_config();
        cfg.retrieval.fusion = Some(FusionConfig {
            bm25_weight: Some(0.0),
            vector_weight: Some(0.0),
            ..Default::default()
        });
        assert!(cfg.validate().is_err());

        let mut cfg = Config::test_config();
        cfg.retrieval.fusion = Some(FusionConfig {
            score_search: Some(-0.1),
            ..Default::default()
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_dedup_threshold() {
        // 0.0 would NOOP nearly every embedded write onto an unrelated
        // memory — silent write loss with a 201 in hand.
        let mut cfg = Config::test_config();
        cfg.write.dedup_threshold = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_dedup_threshold_above_one() {
        let mut cfg = Config::test_config();
        cfg.write.dedup_threshold = 1.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn purge_invalidated_days_defaults_to_30() {
        assert_eq!(Config::test_config().forgetting.purge_invalidated_days, 30);
    }

    #[test]
    fn adjudication_defaults_and_reservation() {
        let cfg = Config::test_config();
        assert_eq!(cfg.consolidation.adjudication_floor, 0.6);
        assert_eq!(cfg.consolidation.adjudication_candidates, 5);
        assert_eq!(cfg.write.adjudication, "async");

        // The reserved mode switch rejects anything but "async" today.
        let mut cfg = Config::test_config();
        cfg.write.adjudication = "sync".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_upgrade_threshold_below_downgrade() {
        // An upgrade bar under the L1 downgrade bar oscillates a memory
        // between levels on every forgetting tick — the exact flip-flop the
        // hysteresis design exists to prevent.
        let mut cfg = Config::test_config();
        cfg.forgetting.threshold_upgrade = 0.25; // < default threshold_to_l1 0.3
        assert!(cfg.validate().is_err());
    }
}
