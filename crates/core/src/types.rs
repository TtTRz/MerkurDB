use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub abstract_: Option<String>,
    pub category: String,
    pub weight: f64,
    pub level: MemoryLevel,
    pub pending_consolidation: bool,
    pub embedding: Option<Vec<f32>>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub context: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub accessed_at: DateTime<Utc>,
    pub access_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

    pub fn from_i32(v: i32) -> Self {
        match v {
            2 => Self::Full,
            1 => Self::Summary,
            0 => Self::Title,
            -1 => Self::Archived,
            _ => {
                tracing::warn!("Unknown memory level {v}, defaulting to Full");
                Self::Full
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeType {
    Auto,
    Manual,
}

#[derive(Debug, Clone)]
pub struct NewMemory {
    pub content: String,
    pub category: Option<String>,
    pub context: HashMap<String, String>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone)]
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
    pub abstract_: Option<String>,
    pub score: f64,
    pub weight: f64,
    pub level: MemoryLevel,
    pub category: String,
    pub context: HashMap<String, String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StorageStats {
    pub total_memories: usize,
    pub total_edges: usize,
    pub pending_consolidation: usize,
    pub by_level: HashMap<i32, usize>,
}

#[derive(Debug, Clone)]
pub struct ConsolidationReport {
    pub memories_processed: usize,
    pub edges_created: usize,
    pub errors: usize,
    pub new_abstracts: HashMap<String, String>,
    pub new_edges: Vec<NewEdge>,
}

impl ConsolidationReport {
    pub fn empty() -> Self {
        Self {
            memories_processed: 0,
            edges_created: 0,
            errors: 0,
            new_abstracts: HashMap::new(),
            new_edges: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationLogEntry {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub memories_processed: i64,
    pub edges_created: i64,
    pub errors: i64,
}

#[derive(Debug, Clone)]
pub enum LevelAction {
    Downgrade(MemoryLevel),
    Archive,
    Keep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Fast,
    Deep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteItem {
    pub content: String,
    pub context: Option<HashMap<String, String>>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    pub score_threshold: f64,
    pub context: Option<HashMap<String, String>>,
    pub depth: usize,
    pub include_graph: bool,
    pub offset: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            score_threshold: 0.3,
            context: None,
            depth: 2,
            include_graph: false,
            offset: 0,
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
