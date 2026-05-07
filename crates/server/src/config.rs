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
    pub auth: AuthConfig,
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
pub struct ConsolidationConfig {
    #[serde(default = "default_60")]
    pub interval_seconds: u64,
    #[serde(default = "default_10")]
    pub batch_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForgettingConfig {
    #[serde(default = "default_300")]
    pub interval_seconds: u64,
    #[serde(default = "default_100")]
    pub batch_size: usize,
    #[serde(default = "default_30")]
    pub archive_days: i32,
    #[serde(default = "default_0_9")]
    pub decay_factor: f64,
    #[serde(default = "default_86400")]
    pub half_life_seconds: f64,
    #[serde(default = "default_0_1")]
    pub access_boost: f64,
    #[serde(default = "default_0_3")]
    pub threshold_to_l1: f64,
    #[serde(default = "default_0_2")]
    pub threshold_to_l0: f64,
    #[serde(default = "default_0_1_a")]
    pub threshold_archive: f64,
}

fn default_60() -> u64 {
    60
}
fn default_300() -> u64 {
    300
}
fn default_10() -> usize {
    10
}
fn default_100() -> usize {
    100
}
fn default_30() -> i32 {
    30
}
fn default_86400() -> f64 {
    86400.0
}
fn default_0_9() -> f64 {
    0.9
}
fn default_0_1() -> f64 {
    0.1
}
fn default_0_2() -> f64 {
    0.2
}
fn default_0_3() -> f64 {
    0.3
}
fn default_0_1_a() -> f64 {
    0.1
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 60,
            batch_size: 10,
        }
    }
}

impl Default for ForgettingConfig {
    fn default() -> Self {
        Self {
            interval_seconds: 300,
            batch_size: 100,
            archive_days: 30,
            decay_factor: 0.9,
            half_life_seconds: 86400.0,
            access_boost: 0.1,
            threshold_to_l1: 0.3,
            threshold_to_l0: 0.2,
            threshold_archive: 0.1,
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
}
