use figment::Figment;
use figment::providers::{Env, Format, Yaml};
use serde::Deserialize;

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub plugins: PluginsConfig,
    pub retrieval: RetrievalConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
    #[serde(default)]
    pub forgetting: ForgettingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    #[serde(rename = "type")]
    pub storage_type: String,
    pub sqlite: SqliteConfig,
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
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct EmbedderConfig {
    #[serde(rename = "type")]
    pub embedder_type: String,
    pub ollama: Option<OllamaConfig>,
    pub openai: Option<OpenAIConfig>,
    pub noop: Option<NoopConfig>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OllamaConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NoopConfig {
    pub dim: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RetrievalConfig {
    pub fast_default_limit: Option<usize>,
    pub score_threshold: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub format: Option<String>,
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

impl Config {
    pub fn load(config_path: Option<&str>) -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Env::prefixed("MERKUR_").split("_"))
            .merge(
                config_path
                    .map(Yaml::file)
                    .unwrap_or_else(|| Yaml::string("")),
            )
            .extract()
            .map_err(Box::new)
    }

    #[cfg(test)]
    pub fn test_config() -> Self {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 0
storage:
  type: "sqlite"
  sqlite:
    path: "file::memory:?cache=shared"
plugins:
  embedder:
    type: "noop"
    noop:
      dim: 16
retrieval: {}
logging: {}
"#;
        Figment::new()
            .merge(Yaml::string(yaml))
            .extract()
            .expect("Failed to load test config")
    }

    pub fn embedding_dim(&self) -> usize {
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
}
