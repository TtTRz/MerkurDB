use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

use crate::{
    ConsolidationLogEntry, ConsolidationReport, LevelAction, Memory, MerkurResult, NewEdge,
    NewMemory, ScoredMemory, StorageStats,
};

#[async_trait]
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;

    async fn encode_batch(&self, texts: &[String]) -> MerkurResult<Vec<Vec<f32>>>;

    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>>;
}

#[async_trait]
pub trait Consolidator: Send + Sync {
    async fn consolidate(&self, memories: &[Memory]) -> MerkurResult<ConsolidationReport>;
}

pub trait Forgetter: Send + Sync {
    fn compute_weight(&self, memory: &Memory, now: DateTime<Utc>) -> f64;

    fn decide(&self, memory: &Memory, now: DateTime<Utc>) -> LevelAction;
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String>;
    async fn update_memory(
        &self,
        id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> MerkurResult<()>;
    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>>;
    async fn delete_memory(&self, id: &str) -> MerkurResult<()>;

    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>>;
    async fn rebuild_vector_index(&self, all: &[(String, Vec<f32>)]) -> MerkurResult<()>;

    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()>;
    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<crate::Edge>>;

    /// Batch variant of [`Storage::get_edges`]. Returns a map keyed by the
    /// supplied memory ids; an id with no edges maps to an empty Vec or is
    /// absent from the map.
    ///
    /// Implementations should issue a single query (e.g. via SQLite's
    /// `json_each(?1)`) so that callers iterating over a neighborhood do not
    /// pay the round-trip cost of N independent SELECTs.
    async fn get_edges_batch(
        &self,
        memory_ids: &[String],
    ) -> MerkurResult<HashMap<String, Vec<crate::Edge>>>;

    async fn bfs_expand(
        &self,
        seed_ids: &[String],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>>;

    async fn insert_context_tag(&self, memory_id: &str, key: &str, value: &str)
    -> MerkurResult<()>;
    async fn search_by_context(
        &self,
        filters: &HashMap<String, String>,
    ) -> MerkurResult<Vec<String>>;

    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>>;
    async fn list_for_forgetting(&self, limit: usize) -> MerkurResult<Vec<Memory>>;
    async fn mark_consolidated(&self, ids: &[String]) -> MerkurResult<()>;
    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()>;

    /// Set the post-consolidation abstract on a memory. Writes the
    /// `memories.abstract` column directly so that `Memory.abstract_` reflects
    /// the LLM-generated summary, rather than tunnelling it through the
    /// `context_tags` side-table.
    async fn update_abstract(&self, id: &str, abstract_: &str) -> MerkurResult<()>;

    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize>;

    async fn log_consolidation(
        &self,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        report: &ConsolidationReport,
    ) -> MerkurResult<()>;

    async fn get_consolidation_log(&self, limit: usize)
    -> MerkurResult<Vec<ConsolidationLogEntry>>;

    async fn stats(&self) -> MerkurResult<StorageStats>;

    /// Whether a memory with the given id exists. Used to validate edge
    /// endpoints at the application layer regardless of whether the underlying
    /// engine enforces foreign keys.
    async fn memory_exists(&self, id: &str) -> MerkurResult<bool>;

    /// Batch variant of [`Storage::memory_exists`]. Returns the subset of the
    /// supplied ids that actually exist. Implementations should issue a single
    /// query so that batch endpoints (e.g. `/v1/relate-batch`) avoid 2N
    /// round-trips for source/target validation.
    async fn memory_exists_batch(&self, ids: &[String]) -> MerkurResult<HashSet<String>>;
}
