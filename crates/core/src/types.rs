use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    #[serde(rename = "abstract")]
    pub abstract_: Option<String>,
    pub category: String,
    pub weight: f64,
    pub level: MemoryLevel,
    pub pending_consolidation: bool,
    // Embedding is an internal detail of the storage layer; never serialize it
    // in API responses, and default to None when absent from input.
    #[serde(default, skip_serializing)]
    pub embedding: Option<Vec<f32>>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub context: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub accessed_at: DateTime<Utc>,
    pub access_count: u64,
    /// Logical isolation bucket. `NewMemory` defaults to `"default"`; rows
    /// written before the namespace column existed are backfilled to it.
    /// This is a *logical* isolation — search/BFS/FTS filter by it — not a
    /// security boundary (any authenticated caller may claim any bucket).
    pub namespace: String,
    /// System-learned salience in [0, 1], written **only** by the Consolidator
    /// during consolidation. New memories carry the neutral 0.5 prior until a
    /// consolidator forms an opinion; clients cannot self-report it through
    /// the write path.
    pub importance: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryLevel {
    Full = 2,
    Summary = 1,
    Title = 0,
    Archived = -1,
}

impl MemoryLevel {
    pub fn to_i32(self) -> i32 {
        self as i32
    }

    /// Convert a stored i32 into a MemoryLevel.
    ///
    /// Unknown values fall back to `Archived` (rather than the previous behaviour
    /// of defaulting to `Full`), so corrupt rows are removed from retrieval rather
    /// than promoted to the highest retention tier. A warning is logged so the
    /// anomaly is still observable.
    pub fn from_i32(v: i32) -> Self {
        match v {
            2 => Self::Full,
            1 => Self::Summary,
            0 => Self::Title,
            -1 => Self::Archived,
            _ => {
                tracing::warn!(level = v, "Unknown memory level, coercing to Archived");
                Self::Archived
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: i64,
    pub source_id: String,
    pub target_id: String,
    pub weight: f64,
    pub relation: String,
    pub edge_type: EdgeType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeType {
    Auto,
    Manual,
}

impl EdgeType {
    pub fn as_db_str(self) -> &'static str {
        match self {
            EdgeType::Auto => "auto",
            EdgeType::Manual => "manual",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "manual" => EdgeType::Manual,
            _ => EdgeType::Auto,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemory {
    pub content: String,
    pub category: Option<String>,
    pub context: HashMap<String, String>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub embedding: Option<Vec<f32>>,
    /// Target bucket; an empty or whitespace-only value falls back to the
    /// `"default"` bucket at insert time.
    #[serde(default = "default_namespace")]
    pub namespace: String,
}

/// Bucket assigned when the caller does not name one.
pub const DEFAULT_NAMESPACE: &str = "default";

/// Salience assigned to memories no consolidator has assessed yet. Deliberately
/// the midpoint: unassessed must neither outrank nor sink beneath assessed rows.
pub const NEUTRAL_IMPORTANCE: f64 = 0.5;

fn default_namespace() -> String {
    DEFAULT_NAMESPACE.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEdge {
    pub source_id: String,
    pub target_id: String,
    pub weight: Option<f64>,
    pub relation: Option<String>,
    pub edge_type: EdgeType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredMemory {
    pub id: String,
    pub content: String,
    #[serde(rename = "abstract")]
    pub abstract_: Option<String>,
    pub score: f64,
    pub weight: f64,
    pub level: MemoryLevel,
    pub category: String,
    pub context: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    /// Bucket this hit came from; always populated so clients can render
    /// isolation visually without re-fetching the memory.
    pub namespace: String,
    /// Consolidator-assessed salience, surfaced so clients can render it
    /// alongside the composite score.
    pub importance: f64,
}

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_memories: usize,
    pub total_edges: usize,
    pub pending_consolidation: usize,
    pub by_level: HashMap<i32, usize>,
}

#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    pub memories_processed: usize,
    pub edges_created: usize,
    pub errors: usize,
    pub new_abstracts: HashMap<String, String>,
    pub new_edges: Vec<NewEdge>,
    /// Consolidator-assessed importance per memory id; absent entries keep
    /// their prior. Only successfully persisted entries are applied.
    pub new_importance: HashMap<String, f64>,
}

impl ConsolidationReport {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationLogEntry {
    pub id: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub memories_processed: i64,
    pub edges_created: i64,
    pub errors: i64,
}

#[derive(Debug, Clone)]
pub enum LevelAction {
    Downgrade(MemoryLevel),
    /// Access-driven promotion back up the ladder (e.g. Title -> Summary).
    /// Firing this requires demonstrated demand — a derived weight well above
    /// the downgrade bars plus a minimum access count — so a memory oscillates
    /// around no single boundary (hysteresis).
    Upgrade(MemoryLevel),
    Archive,
    Keep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// BM25 x vector retrieval fused via reciprocal rank fusion. The default,
    /// because a memory store's out-of-the-box path should always be its best
    /// retrieval.
    Hybrid,
    /// Pure cosine similarity over embeddings. Lowest latency escape hatch.
    Fast,
    /// Vector seeds expanded through the memory graph via BFS.
    Deep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteItem {
    pub content: String,
    #[serde(default)]
    pub context: Option<HashMap<String, String>>,
    #[serde(default)]
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub mode: SearchMode,
    pub limit: usize,
    pub score_threshold: f64,
    pub context: Option<HashMap<String, String>>,
    pub depth: usize,
    pub degree_limit: usize,
    pub include_graph: bool,
    pub offset: usize,
    pub level: Option<Vec<MemoryLevel>>,
    pub category: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: SearchMode::Hybrid,
            limit: 10,
            score_threshold: 0.3,
            context: None,
            depth: 2,
            degree_limit: 10,
            include_graph: false,
            offset: 0,
            level: None,
            category: None,
            from: None,
            to: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResponse {
    pub id: String,
    pub status: String,
    pub searchable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteBatchResponse {
    pub ids: Vec<String>,
    pub count: usize,
}

/// Hard limits on user-controllable search parameters to avoid DoS.
pub mod limits {
    pub const MAX_SEARCH_LIMIT: usize = 1000;
    pub const MAX_BFS_DEPTH: usize = 5;
    pub const MAX_BFS_DEGREE: usize = 100;
    pub const MAX_BATCH_ITEMS: usize = 500;
    pub const MAX_CONTENT_BYTES: usize = 64 * 1024;
    pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
}
