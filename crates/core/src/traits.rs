use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

use crate::{
    Adjudication, ConsolidationLogEntry, ConsolidationReport, LevelAction, Memory, MerkurResult,
    NewEdge, NewMemory, ScoredMemory, StorageStats,
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

    /// Judge one freshly written memory against its nearest neighbors
    /// (write governance, P1-7). The default verdict is `Add` — no
    /// adjudication happens without an LLM behind the trait.
    async fn adjudicate(
        &self,
        _pending: &Memory,
        _candidates: &[ScoredMemory],
    ) -> MerkurResult<Adjudication> {
        Ok(Adjudication::default())
    }
}

pub trait Forgetter: Send + Sync {
    fn compute_weight(&self, memory: &Memory, now: DateTime<Utc>) -> f64;

    fn decide(&self, memory: &Memory, now: DateTime<Utc>) -> LevelAction;
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String>;

    /// Insert with a write-time dedup short-circuit (P2-8).
    ///
    /// Before inserting, the implementation looks up the top-1 most similar
    /// memory **in the same namespace**. If its cosine similarity clears
    /// `threshold`, the write is a NOOP: no new row is created and the id of
    /// the existing memory is returned instead. This is the ADD/NOOP half of
    /// mem0's write governance; UPDATE/DELETE adjudication stays with the
    /// asynchronous Consolidator.
    ///
    /// Implementations may skip the check when the new memory carries no
    /// embedding (there is nothing to compare).
    async fn insert_memory_dedup(&self, mem: &NewMemory, threshold: f64)
    -> MerkurResult<String>;
    async fn update_memory(
        &self,
        id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> MerkurResult<()>;
    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>>;
    async fn delete_memory(&self, id: &str) -> MerkurResult<()>;

    /// Soft-invalidate a memory (P1-7 write governance): the row stays for
    /// audit with `invalid_at` set, but disappears from every retrieval
    /// channel until the retention purge hard-deletes it. `absorbed_into`
    /// records the surviving memory when the invalidation is the absorb half
    /// of an UPDATE adjudication. Idempotent: re-invalidating keeps the
    /// original timestamp.
    async fn invalidate_memory(&self, id: &str, absorbed_into: Option<&str>) -> MerkurResult<()>;

    /// Hard-delete memories invalidated more than `days` ago — the retention
    /// window for the soft-invalidation channel.
    async fn purge_invalidated_older_than(&self, days: i32) -> MerkurResult<usize>;

    /// Pure cosine channel over **every** bucket. Kept for the rare
    /// cross-namespace audit path; new callers should prefer
    /// [`Storage::vector_search_ns`].
    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>>;

    /// Cosine channel restricted to one bucket.
    ///
    /// Pure query: records no access. Serving points call
    /// [`Storage::record_access`] for the results they actually return.
    async fn vector_search_ns(
        &self,
        vec: &[f32],
        namespace: &str,
        limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>>;

    /// Record that these memories were served to a caller — the demand
    /// signal the forgetting curve and access-driven promotion consume.
    ///
    /// Retrieval methods ([`Storage::vector_search_ns`],
    /// [`Storage::text_search`], [`Storage::bfs_expand_ns`]) are pure queries
    /// and never record; governance probes (write-time dedup) ride the same
    /// pure path. Serving points — the REST search handler, context assembly,
    /// the MCP tools — call this for the items they actually return, so the
    /// signal stays symmetric across channels and free of probe noise.
    async fn record_access(&self, ids: &[String]) -> MerkurResult<()>;

    /// Batch-fetch raw embeddings by id. Ids without an embedding are absent
    /// from the map. Internal surface: consolidation adjudication needs the
    /// pending memories' vectors (the `Memory` read model deliberately omits
    /// the blob).
    async fn get_embeddings(
        &self,
        ids: &[String],
    ) -> MerkurResult<std::collections::HashMap<String, Vec<f32>>>;

    /// Full-text (BM25) channel for hybrid retrieval, best match first.
    ///
    /// Implementations tokenize with FTS5's trigram tokenizer, so CJK and
    /// unsegmented scripts are matched by substring. Queries shorter than
    /// three characters (after trimming) cannot produce any trigram and yield
    /// an empty result — the vector channel is expected to cover those.
    /// Returned scores are raw SQLite `bm25()` values where smaller means more
    /// relevant; only ordering is meaningful to callers.
    ///
    /// Restricted to one bucket.
    async fn text_search(
        &self,
        query: &str,
        namespace: &str,
        limit: usize,
    ) -> MerkurResult<Vec<(String, f64)>>;

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

    /// Graph traversal over **every** bucket (legacy, cross-bucket).
    async fn bfs_expand(
        &self,
        seed_ids: &[String],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>>;

    /// Graph traversal restricted to one bucket: edges whose endpoints live
    /// in other buckets are silently not followed.
    async fn bfs_expand_ns(
        &self,
        seed_ids: &[String],
        namespace: &str,
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

    /// Persist a Consolidator-assessed importance score. This is the **only**
    /// write path for importance — the public write API has no such field, so
    /// salience stays system-learned rather than client-reported.
    async fn update_importance(&self, id: &str, importance: f64) -> MerkurResult<()>;

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
