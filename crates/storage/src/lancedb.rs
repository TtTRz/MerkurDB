use arrow_array::types::Float32Type;
use arrow_array::{Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use async_trait::async_trait;
use chrono::Utc;
use lancedb::index::Index;
use lancedb::index::vector::IvfPqIndexBuilder;
use merkur_core::{
    ConsolidationLogEntry, ConsolidationReport, Edge, Memory, MemoryLevel, MemoryListFilter,
    MerkurError, MerkurResult, NewEdge, NewMemory, ScoredMemory, Storage, StorageStats,
};
use merkur_core::{escape_fts_query, is_bm25_viable};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task;
use tracing::{info, warn};

use crate::sqlite_helpers;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id                     TEXT PRIMARY KEY,
    content                TEXT NOT NULL,
    abstract               TEXT DEFAULT '',
    category               TEXT DEFAULT 'general',
    weight                 REAL NOT NULL DEFAULT 1.0,
    level                  INTEGER NOT NULL DEFAULT 2,
    pending_consolidation  INTEGER NOT NULL DEFAULT 1,
    metadata               TEXT NOT NULL DEFAULT '{}',
    created_at             TEXT NOT NULL,
    updated_at             TEXT NOT NULL,
    accessed_at            TEXT NOT NULL,
    access_count           INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_mem_pending  ON memories(pending_consolidation);
CREATE INDEX IF NOT EXISTS idx_mem_level    ON memories(level);
CREATE INDEX IF NOT EXISTS idx_mem_accessed ON memories(accessed_at);
CREATE INDEX IF NOT EXISTS idx_mem_category ON memories(category);

CREATE TABLE IF NOT EXISTS edges (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    weight       REAL NOT NULL DEFAULT 1.0,
    relation     TEXT NOT NULL DEFAULT 'related',
    edge_type    TEXT NOT NULL DEFAULT 'auto' CHECK(edge_type IN ('auto','manual')),
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    UNIQUE(source_id, target_id, relation)
);

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id);

CREATE TABLE IF NOT EXISTS context_tags (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ctx_memory ON context_tags(memory_id);
CREATE INDEX IF NOT EXISTS idx_ctx_kv     ON context_tags(key, value);

CREATE TABLE IF NOT EXISTS consolidate_log (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at          TEXT NOT NULL,
    finished_at         TEXT,
    memories_processed  INTEGER NOT NULL DEFAULT 0,
    edges_created       INTEGER NOT NULL DEFAULT 0,
    errors              INTEGER NOT NULL DEFAULT 0,
    absorptions         INTEGER NOT NULL DEFAULT 0,
    invalidations       INTEGER NOT NULL DEFAULT 0
);
";

const VECTOR_TABLE: &str = "vectors";

/// Minimum rows before auto-building a vector index.
const INDEX_THRESHOLD: usize = 256;
const GC_BATCH_SIZE: usize = 128;
const GC_GRACE_SECONDS: i64 = 300;

pub struct LanceDbStorage {
    db: lancedb::Connection,
    sqlite_pool: Pool<SqliteConnectionManager>,
    table_name: String,
    dim: usize,
    index_building: Arc<AtomicBool>,
}

async fn run_blocking<F, T>(f: F) -> MerkurResult<T>
where
    F: FnOnce() -> MerkurResult<T> + Send + 'static,
    T: Send + 'static,
{
    task::spawn_blocking(f)
        .await
        .map_err(|e| MerkurError::Internal(format!("blocking task panicked: {e}")))?
}

/// Inspect every vector index: native selection may choose a mismatched first index.
async fn vector_index_metrics(
    table: &lancedb::Table,
) -> MerkurResult<Vec<Option<lancedb::DistanceType>>> {
    let mut metrics = Vec::new();
    for index in table
        .list_indices()
        .await
        .map_err(|e| MerkurError::Storage(e.to_string()))?
    {
        if index.columns.iter().any(|c| c == "vector") {
            metrics.push(
                table
                    .index_stats(&index.name)
                    .await
                    .map_err(|e| MerkurError::Storage(e.to_string()))?
                    .and_then(|s| s.distance_type),
            );
        }
    }
    Ok(metrics)
}

/// Preserve SQLite's zero-vector semantics without retaining all embeddings.
/// Native cosine KNN omits zero-norm rows; these can outrank negative matches.
async fn exact_cosine_candidates(
    table: &lancedb::Table,
    vec: &[f32],
    limit: usize,
) -> MerkurResult<HashMap<String, f64>> {
    use futures::TryStreamExt;
    use lancedb::query::{ExecutableQuery, QueryBase, Select};
    let norm = crate::vector_index::l2_norm(vec);
    let zero = norm == 0.0;
    let mut query = table.query().only_if("vector IS NOT NULL").select(if zero {
        Select::columns(&["id"])
    } else {
        Select::columns(&["id", "vector"])
    });
    if zero {
        query = query.limit(limit);
    }
    let mut stream = query
        .execute()
        .await
        .map_err(|e| MerkurError::Storage(e.to_string()))?;
    let mut best = Vec::new();
    while let Some(batch) = stream
        .try_next()
        .await
        .map_err(|e| MerkurError::Storage(e.to_string()))?
    {
        let ids = batch
            .column_by_name("id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| MerkurError::Storage("Invalid vector key column".into()))?;
        let vectors = if zero {
            None
        } else {
            Some(
                batch
                    .column_by_name("vector")
                    .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>())
                    .ok_or_else(|| MerkurError::Storage("Invalid vector column".into()))?,
            )
        };
        for i in 0..batch.num_rows() {
            let score = if let Some(vectors) = vectors {
                let values = vectors.value(i);
                let values = values
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .ok_or_else(|| MerkurError::Storage("Invalid embedding type".into()))?;
                let values = values.values().as_ref();
                crate::vector_index::cosine_similarity(
                    vec,
                    values,
                    norm,
                    crate::vector_index::l2_norm(values),
                )
            } else {
                0.0
            };
            if !score.is_finite() {
                return Err(MerkurError::Storage(
                    "Non-finite stored cosine similarity".into(),
                ));
            }
            best.push((ids.value(i).to_string(), score.clamp(-1.0, 1.0)));
        }
        best.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        best.truncate(limit);
    }
    Ok(best.into_iter().collect())
}

/// Cosine distance is `1 - similarity`, independent of vector magnitude.
async fn lance_candidates(
    table: &lancedb::Table,
    vec: &[f32],
    limit: usize,
) -> MerkurResult<HashMap<String, f64>> {
    use futures::TryStreamExt;
    use lancedb::query::{ExecutableQuery, QueryBase};
    let norm = crate::vector_index::l2_norm(vec);
    if !norm.is_finite() {
        return Err(MerkurError::BadRequest(
            "Query vector must be finite".into(),
        ));
    }
    if norm == 0.0 {
        return exact_cosine_candidates(table, vec, limit).await;
    }
    let metrics = vector_index_metrics(table).await?;
    let mut query = table
        .query()
        .nearest_to(vec)
        .map_err(|e| MerkurError::Storage(format!("Failed to create query: {e}")))?
        .distance_type(lancedb::DistanceType::Cosine)
        .refine_factor(1)
        .limit(limit);
    if metrics.is_empty()
        || metrics
            .iter()
            .any(|metric| *metric != Some(lancedb::DistanceType::Cosine))
    {
        query = query.bypass_vector_index();
    }
    let results: Vec<_> = query
        .execute()
        .await
        .map_err(|e| MerkurError::Storage(format!("Vector search failed: {e}")))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| MerkurError::Storage(format!("Failed to collect results: {e}")))?;

    let mut id_score: HashMap<String, f64> = HashMap::new();
    for batch in results {
        let id_col = batch
            .column_by_name("id")
            .ok_or_else(|| MerkurError::Storage("Missing id column".into()))?;
        let id_array = id_col
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| MerkurError::Storage("Invalid id column type".into()))?;
        let distance_col = batch
            .column_by_name("_distance")
            .ok_or_else(|| MerkurError::Storage("Missing _distance column".into()))?;
        let dist_array = distance_col
            .as_any()
            .downcast_ref::<Float32Array>()
            .ok_or_else(|| MerkurError::Storage("Invalid distance type".into()))?;
        for i in 0..batch.num_rows() {
            let id = id_array.value(i).to_string();
            let d = f64::from(dist_array.value(i));
            let score = (1.0 - d).clamp(-1.0, 1.0);
            id_score.insert(id, score);
        }
    }
    let expected = limit.min(
        table
            .count_rows(None)
            .await
            .map_err(|e| MerkurError::Storage(e.to_string()))?,
    );
    if id_score.len() < expected
        || id_score
            .values()
            .any(|score| !score.is_finite() || *score < 0.0)
    {
        return exact_cosine_candidates(table, vec, limit).await;
    }
    Ok(id_score)
}

async fn ensure_cosine_index(table: &lancedb::Table) -> MerkurResult<()> {
    if table
        .count_rows(None)
        .await
        .map_err(|e| MerkurError::Storage(e.to_string()))?
        < INDEX_THRESHOLD
    {
        return Ok(());
    }
    if vector_index_metrics(table)
        .await?
        .contains(&Some(lancedb::DistanceType::Cosine))
    {
        return Ok(());
    }
    table
        .create_index(
            &["vector"],
            Index::IvfPq(IvfPqIndexBuilder::default().distance_type(lancedb::DistanceType::Cosine)),
        )
        .execute()
        .await
        .map_err(|e| MerkurError::Storage(format!("create cosine index: {e}")))?;
    info!("LanceDB cosine vector index built");
    Ok(())
}

fn consume_vector_stage(tx: &rusqlite::Transaction<'_>, key: &str) -> MerkurResult<()> {
    if key.is_empty() {
        return Ok(());
    }
    let consumed = tx
        .execute(
            "DELETE FROM lance_vector_staging WHERE vector_id = ?1 AND state = 'pending'",
            params![key],
        )
        .map_err(|e| MerkurError::Storage(format!("consume vector stage: {e}")))?;
    if consumed != 1 {
        return Err(MerkurError::Storage(
            "Vector publication expired; retry this write".into(),
        ));
    }
    Ok(())
}

impl LanceDbStorage {
    pub async fn new(lance_path: &str, sqlite_path: &str, dim: usize) -> MerkurResult<Self> {
        let db = lancedb::connect(lance_path)
            .read_consistency_interval(std::time::Duration::ZERO)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to connect to LanceDB: {e}")))?;

        let sqlite_pool = sqlite_helpers::build_pool(sqlite_path, 10)?;

        let conn = sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute_batch(DDL)
            .map_err(|e| MerkurError::Storage(format!("Failed to init SQLite schema: {e}")))?;
        drop(conn);

        // Both backends share this SQLite file for rows/FTS; skip this and the
        // table never gains the migrated columns (namespace, importance, ...).
        crate::migration::migrate(&sqlite_pool)?;

        let storage = Self {
            db,
            sqlite_pool,
            table_name: VECTOR_TABLE.to_string(),
            dim,
            index_building: Arc::new(AtomicBool::new(false)),
        };

        storage.ensure_vector_table().await?;
        storage.spawn_index_builder();
        info!("LanceDbStorage initialized: lance={lance_path}, sqlite={sqlite_path}, dim={dim}");

        Ok(storage)
    }

    fn vector_schema(&self) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    self.dim as i32,
                ),
                true,
            ),
        ]))
    }

    async fn ensure_vector_table(&self) -> MerkurResult<()> {
        let table_names = self
            .db
            .table_names()
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to list tables: {e}")))?;

        if table_names.iter().any(|t| t == VECTOR_TABLE) {
            return Ok(());
        }

        let schema = self.vector_schema();
        let empty_batch = RecordBatch::new_empty(schema.clone());
        self.db
            .create_table(VECTOR_TABLE, empty_batch)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to create vector table: {e}")))?;

        Ok(())
    }

    fn spawn_index_builder(&self) {
        if self.index_building.swap(true, Ordering::AcqRel) {
            return;
        }
        let db = self.db.clone();
        let table_name = self.table_name.clone();
        let flag = self.index_building.clone();

        tokio::spawn(async move {
            let result = async {
                let table = db
                    .open_table(&table_name)
                    .execute()
                    .await
                    .map_err(|e| format!("open table: {e}"))?;
                ensure_cosine_index(&table).await.map_err(|e| e.to_string())
            }
            .await;

            if let Err(e) = result {
                warn!("LanceDB index build failed: {e}");
            }
            flag.store(false, Ordering::Release);
        });
    }

    async fn get_table(&self) -> MerkurResult<lancedb::Table> {
        self.db
            .open_table(&self.table_name)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to open vector table: {e}")))
    }

    fn vec_to_arrow(vec: &[f32], dim: usize) -> FixedSizeListArray {
        FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            vec![Some(vec.iter().map(|&x| Some(x)).collect::<Vec<_>>())],
            dim as i32,
        )
    }

    /// Published keys are immutable; SQL alone controls their visibility.
    async fn stage_vector(&self, embedding: Option<&[f32]>) -> MerkurResult<String> {
        let Some(embedding) = embedding else {
            return Ok(String::new());
        };
        if embedding.len() != self.dim {
            return Err(MerkurError::BadRequest(
                "Embedding dimension mismatch".into(),
            ));
        }
        let key = format!("mem_{}", uuid::Uuid::new_v4());
        let batch = RecordBatch::try_new(
            self.vector_schema(),
            vec![
                Arc::new(StringArray::from(vec![key.clone()])),
                Arc::new(Self::vec_to_arrow(embedding, self.dim)),
            ],
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to stage vector batch: {e}")))?;
        let pool = self.sqlite_pool.clone();
        let staged_key = key.clone();
        run_blocking(move || {
            let conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
            conn.execute("INSERT INTO lance_vector_staging(vector_id, created_at, state) VALUES (?1, ?2, 'pending')",
                params![staged_key, Utc::now().to_rfc3339()])
                .map_err(|e| MerkurError::Storage(format!("register vector stage: {e}")))?;
            Ok(())
        }).await?;
        self.get_table()
            .await?
            .add(vec![batch])
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to stage vector: {e}")))?;
        Ok(key)
    }

    async fn publish_vector_update(
        &self,
        id: &str,
        content: &str,
        vector_key: String,
    ) -> MerkurResult<()> {
        let id = id.to_string();
        let content = content.to_string();
        let pool = self.sqlite_pool.clone();
        let obsolete = run_blocking(move || {
            let mut conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT COALESCE(lance_vector_id, id) FROM memories WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let Some(old) = old else {
                return Err(MerkurError::MemoryNotFound(id));
            };
            consume_vector_stage(&tx, &vector_key)?;
            tx.execute(
                "UPDATE memories SET content = ?1, lance_vector_id = ?2, pending_consolidation = 1,
                updated_at = ?3 WHERE id = ?4",
                params![content, vector_key, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to publish update: {e}")))?;
            tx.commit()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            Ok(old)
        })
        .await?;
        if let Err(e) = self.reclaim_vector_keys(&[obsolete]).await {
            warn!(error = %e, "Retired vector cleanup failed after publication");
        }
        Ok(())
    }

    /// Include invalidated rows: an audit row may share a key with a survivor.
    async fn vector_references(&self, ids: &[String]) -> MerkurResult<Vec<(String, String, bool)>> {
        let pool = self.sqlite_pool.clone();
        let ids = serde_json::to_string(ids).map_err(|e| MerkurError::Storage(e.to_string()))?;
        run_blocking(move || {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, COALESCE(lance_vector_id, id), lance_vector_id IS NULL
                FROM memories WHERE id IN (SELECT value FROM json_each(?1))
                AND COALESCE(lance_vector_id, id) != ''",
                )
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            stmt.query_map(params![ids], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| MerkurError::Storage(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| MerkurError::Storage(e.to_string()))
        })
        .await
    }

    /// Only retire previously published keys. A general orphan sweep could
    /// race a staged vector whose SQL publication has not happened yet.
    async fn reclaim_vector_keys(&self, keys: &[String]) -> MerkurResult<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let pool = self.sqlite_pool.clone();
        let keys = keys.to_vec();
        let unreferenced = run_blocking(move || {
            let conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
            let mut unused = Vec::new();
            for key in keys {
                if key.is_empty() { continue; }
                let referenced: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE COALESCE(lance_vector_id, id) = ?1)",
                    params![key], |r| r.get(0),
                ).map_err(|e| MerkurError::Storage(e.to_string()))?;
                if !referenced { unused.push(key); }
            }
            Ok(unused)
        }).await?;
        if !unreferenced.is_empty() {
            let keys = unreferenced
                .iter()
                .map(|key| Self::quote_id_strict(key))
                .collect::<MerkurResult<Vec<_>>>()?
                .join(",");
            self.get_table()
                .await?
                .delete(&format!("id IN ({keys})"))
                .await
                .map_err(|e| MerkurError::Storage(format!("Failed to reclaim vectors: {e}")))?;
        }
        Ok(())
    }

    /// Claim under the same SQL write lock used by publishers. Grace is an
    /// availability policy; revoking the capability is the safety boundary.
    async fn claim_gc_keys(
        &self,
        keys: &[String],
        cutoff: &str,
        limit: usize,
    ) -> MerkurResult<Vec<String>> {
        let mut unique = std::collections::HashSet::new();
        let keys: Vec<_> = keys
            .iter()
            .filter(|key| Self::quote_id_strict(key).is_ok() && unique.insert(key.as_str()))
            .cloned()
            .collect();
        let cutoff = cutoff.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || {
            let mut conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let mut claimed = Vec::new();
            for key in keys {
                if claimed.len() >= limit { break; }
                let referenced: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE COALESCE(lance_vector_id, id) = ?1)",
                    params![key], |r| r.get(0),
                ).map_err(|e| MerkurError::Storage(e.to_string()))?;
                if referenced { continue; }
                let stage: Option<(String, String)> = tx.query_row(
                    "SELECT state, created_at FROM lance_vector_staging WHERE vector_id = ?1", params![key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional().map_err(|e| MerkurError::Storage(e.to_string()))?;
                if stage.is_some_and(|(state, created)| state == "pending" && created >= cutoff) { continue; }
                tx.execute("INSERT INTO lance_vector_staging(vector_id, created_at, state) VALUES (?1, ?2, 'collecting')
                    ON CONFLICT(vector_id) DO UPDATE SET state = 'collecting'", params![key, Utc::now().to_rfc3339()])
                    .map_err(|e| MerkurError::Storage(e.to_string()))?;
                claimed.push(key);
            }
            tx.commit().map_err(|e| MerkurError::Storage(e.to_string()))?;
            Ok(claimed)
        }).await
    }

    async fn delete_gc_keys(&self, keys: &[String]) -> MerkurResult<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        let quoted = keys
            .iter()
            .map(|key| Self::quote_id_strict(key))
            .collect::<MerkurResult<Vec<_>>>()?
            .join(",");
        self.get_table()
            .await?
            .delete(&format!("id IN ({quoted})"))
            .await
            .map_err(|e| MerkurError::Storage(format!("delete orphan vectors: {e}")))?;
        let pool = self.sqlite_pool.clone();
        let encoded =
            serde_json::to_string(keys).map_err(|e| MerkurError::Storage(e.to_string()))?;
        run_blocking(move || {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            conn.execute(
                "DELETE FROM lance_vector_staging WHERE state = 'collecting'
                AND vector_id IN (SELECT value FROM json_each(?1))",
                params![encoded],
            )
            .map_err(|e| MerkurError::Storage(e.to_string()))?;
            Ok(())
        })
        .await?;
        // Deleted capabilities cannot be revived by a delayed publisher.
        Ok(keys.len())
    }

    async fn delete_expired(&self, days: i32, invalidated: bool) -> MerkurResult<usize> {
        let pool = self.sqlite_pool.clone();
        let keys = run_blocking(move || {
            let conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
            let threshold = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
            let sql = if invalidated {
                "DELETE FROM memories WHERE invalid_at IS NOT NULL AND invalid_at < ?1 RETURNING COALESCE(lance_vector_id, id)"
            } else {
                "DELETE FROM memories WHERE level = -1 AND updated_at < ?1 RETURNING COALESCE(lance_vector_id, id)"
            };
            let mut stmt = conn.prepare(sql).map_err(|e| MerkurError::Storage(e.to_string()))?;
            stmt.query_map(params![threshold], |r| r.get::<_, String>(0))
                .map_err(|e| MerkurError::Storage(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| MerkurError::Storage(e.to_string()))
        }).await?;
        self.reclaim_vector_keys(&keys).await?;
        Ok(keys.len())
    }

    /// Quote a memory id for inclusion in a LanceDB SQL filter. We refuse
    /// anything that doesn't match our generated `mem_<uuid>` shape — IDs are
    /// always server-generated, so any external value is malicious or buggy.
    fn quote_id_strict(id: &str) -> MerkurResult<String> {
        if id
            .strip_prefix("mem_")
            .and_then(|suffix| uuid::Uuid::parse_str(suffix).ok())
            .is_none()
        {
            return Err(MerkurError::BadRequest("Invalid vector key".into()));
        }
        Ok(format!("'{id}'"))
    }
}
#[async_trait]
impl Storage for LanceDbStorage {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String> {
        let id = format!("mem_{}", uuid::Uuid::new_v4());
        let vector_key = self.stage_vector(mem.embedding.as_deref()).await?;
        let now = Utc::now().to_rfc3339();
        let metadata = serde_json::to_string(&mem.metadata)
            .map_err(|e| MerkurError::Storage(format!("Failed to serialize metadata: {e}")))?;
        let category = mem.category.clone().unwrap_or_else(|| "general".into());
        let context = mem.context.clone();
        let content = mem.content.clone();
        let namespace = if mem.namespace.trim().is_empty() {
            merkur_core::DEFAULT_NAMESPACE.to_string()
        } else {
            mem.namespace.clone()
        };
        let id_for_db = id.clone();
        let pool = self.sqlite_pool.clone();

        run_blocking(move || -> MerkurResult<()> {
            let mut conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| MerkurError::Storage(format!("begin tx failed: {e}")))?;
            consume_vector_stage(&tx, &vector_key)?;
            tx.execute(
                "INSERT INTO memories (id, content, category, weight, level, pending_consolidation, metadata, created_at, updated_at, accessed_at, namespace, valid_at, lance_vector_id)
                 VALUES (?1, ?2, ?3, 1.0, 2, 1, ?4, ?5, ?5, ?5, ?6, ?5, ?7)",
                params![id_for_db, content, category, metadata, now, namespace, vector_key],
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to insert memory: {e}")))?;
            for (k, v) in &context {
                tx.execute(
                    "INSERT INTO context_tags (memory_id, key, value) VALUES (?1, ?2, ?3)",
                    params![id_for_db, k, v],
                )
                .map_err(|e| MerkurError::Storage(format!("Failed to insert context tag: {e}")))?;
            }
            tx.commit()
                .map_err(|e| MerkurError::Storage(format!("commit failed: {e}")))?;
            Ok(())
        })
        .await?;

        self.spawn_index_builder();

        Ok(id)
    }

    async fn insert_memory_dedup(&self, mem: &NewMemory, threshold: f64) -> MerkurResult<String> {
        if let Some(embedding) = mem.embedding.as_deref() {
            // Best-effort governance probe: a probe failure must not fail the
            // write — fall back to a plain insert.
            let top = match self.vector_search_ns(embedding, &mem.namespace, 1).await {
                Ok(hits) => hits.into_iter().next().map(|h| (h.id, h.score)),
                Err(e) => {
                    tracing::warn!(error = %e, "dedup probe failed; inserting plainly");
                    None
                }
            };
            if let Some(existing_id) = sqlite_helpers::dedup_verdict(top, threshold) {
                tracing::debug!(
                    existing_id = existing_id.as_str(),
                    threshold,
                    "write-time dedup: NOOP onto existing memory"
                );
                return Ok(existing_id);
            }
        }
        self.insert_memory(mem).await
    }

    async fn update_memory(
        &self,
        id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> MerkurResult<()> {
        let vector_key = self.stage_vector(embedding).await?;
        self.publish_vector_update(id, content, vector_key).await
    }

    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>> {
        let id_owned = id.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::get_memory_row(&pool, &id_owned)).await
    }

    async fn list_memories(&self, filter: &MemoryListFilter) -> MerkurResult<(Vec<Memory>, usize)> {
        let filter = filter.clone();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::list_memories_filtered(&pool, &filter)).await
    }

    async fn delete_memory(&self, id: &str) -> MerkurResult<()> {
        let id = id.to_string();
        let pool = self.sqlite_pool.clone();
        let obsolete = run_blocking(move || {
            let mut conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let key: Option<String> = tx
                .query_row(
                    "SELECT COALESCE(lance_vector_id, id) FROM memories WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            let Some(key) = key else {
                return Err(MerkurError::MemoryNotFound(id));
            };
            tx.execute("DELETE FROM memories WHERE id = ?1", params![id])
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            tx.commit()
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            Ok(key)
        })
        .await?;
        self.reclaim_vector_keys(&[obsolete]).await
    }

    async fn text_search(
        &self,
        query: &str,
        namespace: &str,
        limit: usize,
    ) -> MerkurResult<Vec<(String, f64)>> {
        // The LanceDB backend keeps memory contents in the shared SQLite
        // database (the FTS5 index and its triggers live there too), so the
        // BM25 channel rides the same helper as the SQLite-only backend.
        if !is_bm25_viable(query) {
            return Ok(Vec::new());
        }
        let expr = escape_fts_query(query);
        let namespace = namespace.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::text_search_bm25(&pool, &expr, &namespace, limit))
            .await
    }

    async fn vector_search_ns(
        &self,
        vec: &[f32],
        namespace: &str,
        limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        let table = self.get_table().await?;

        // Same iterative-deepening contract as the SQLite backend: candidates
        // are scored globally in the vector table and filtered by bucket in
        // the shared SQLite database, so a fixed oversample can starve a
        // small bucket.
        let mut fetch = limit.saturating_mul(2).max(limit).max(1);
        let (memories, id_score) = loop {
            let id_score = lance_candidates(&table, vec, fetch).await?;
            let exhausted = id_score.len() < fetch;
            let ids: Vec<String> = id_score.keys().cloned().collect();
            let pool = self.sqlite_pool.clone();
            let namespace = namespace.to_string();
            let scores = id_score.clone();
            let (memories, memory_scores) = run_blocking(move || {
                let conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
                let keys = serde_json::to_string(&ids).map_err(|e| MerkurError::Storage(e.to_string()))?;
                let mut stmt = conn.prepare(
                    "SELECT id, content, abstract, category, weight, level, created_at, namespace, importance,
                        content_revision, COALESCE(lance_vector_id, id) FROM memories
                     WHERE COALESCE(lance_vector_id, id) IN (SELECT value FROM json_each(?1))
                        AND namespace = ?2 AND level >= 0 AND invalid_at IS NULL")
                    .map_err(|e| MerkurError::Storage(e.to_string()))?;
                let rows = stmt.query_map(params![keys, namespace], |r| {
                    let row: crate::sqlite::MemoryRow = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                        r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?);
                    Ok((row, r.get::<_, String>(10)?))
                }).map_err(|e| MerkurError::Storage(e.to_string()))?;
                let mut memories = Vec::new();
                let mut memory_scores = HashMap::new();
                for row in rows {
                    let (row, key) = row.map_err(|e| MerkurError::Storage(e.to_string()))?;
                    memory_scores.insert(row.0.clone(), scores[&key]);
                    memories.push(row);
                }
                Ok((memories, memory_scores))
            }).await?;
            if memories.len() >= limit || exhausted {
                break (memories, memory_scores);
            }
            fetch = fetch.saturating_mul(2);
        };

        let pool2 = self.sqlite_pool.clone();
        let id_set: Vec<String> = memories.iter().map(|m| m.0.clone()).collect();
        let ctx_map = run_blocking(
            move || -> MerkurResult<HashMap<String, HashMap<String, String>>> {
                let conn = pool2
                    .get()
                    .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
                let ids_json = serde_json::to_string(&id_set)
                    .map_err(|e| MerkurError::Storage(format!("Failed to encode ids: {e}")))?;
                let mut stmt = conn
                    .prepare(
                        "SELECT memory_id, key, value FROM context_tags
                     WHERE memory_id IN (SELECT value FROM json_each(?1))",
                    )
                    .map_err(|e| {
                        MerkurError::Storage(format!("Failed to prepare ctx batch: {e}"))
                    })?;
                let rows = stmt
                    .query_map(params![ids_json], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })
                    .map_err(|e| MerkurError::Storage(format!("Ctx batch failed: {e}")))?;
                let mut by_id: HashMap<String, HashMap<String, String>> = HashMap::new();
                for r in rows {
                    let (mid, k, v) =
                        r.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
                    by_id.entry(mid).or_default().insert(k, v);
                }
                Ok(by_id)
            },
        )
        .await?;

        let mut out: Vec<ScoredMemory> = memories
            .into_iter()
            .map(
                |(
                    id,
                    content,
                    abstract_,
                    category,
                    weight,
                    level_i32,
                    created_at,
                    namespace,
                    importance,
                    content_revision,
                )| {
                    let level = MemoryLevel::from_i32(level_i32);
                    let score = id_score.get(&id).copied().unwrap_or(0.0);
                    let context = ctx_map.get(&id).cloned().unwrap_or_default();
                    ScoredMemory {
                        id,
                        content,
                        content_revision,
                        abstract_,
                        score,
                        weight,
                        level,
                        category,
                        context,
                        created_at: sqlite_helpers::parse_rfc3339(&created_at),
                        namespace,
                        importance,
                    }
                },
            )
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.truncate(limit);

        Ok(out)
    }

    async fn record_access(&self, ids: &[String]) -> MerkurResult<()> {
        let pool = self.sqlite_pool.clone();
        let ids = ids.to_vec();
        run_blocking(move || sqlite_helpers::update_access(&pool, &ids)).await
    }

    async fn get_embeddings(&self, ids: &[String]) -> MerkurResult<HashMap<String, Vec<f32>>> {
        use futures::TryStreamExt;
        use lancedb::query::{ExecutableQuery, QueryBase};
        let references = self.vector_references(ids).await?;
        if references.is_empty() {
            return Ok(HashMap::new());
        }
        let keys = references
            .iter()
            .map(|(_, key, _)| Self::quote_id_strict(key))
            .collect::<MerkurResult<Vec<_>>>()?
            .join(",");
        let batches = self
            .get_table()
            .await?
            .query()
            .only_if(format!("id IN ({keys})"))
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to query vectors: {e}")))?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| MerkurError::Storage(e.to_string()))?;
        let mut by_key = HashMap::new();
        for batch in batches {
            let keys = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| MerkurError::Storage("Invalid vector key column".into()))?;
            let vectors = batch
                .column_by_name("vector")
                .and_then(|c| c.as_any().downcast_ref::<FixedSizeListArray>())
                .ok_or_else(|| MerkurError::Storage("Invalid vector column".into()))?;
            for i in 0..batch.num_rows() {
                let array = vectors.value(i);
                let vector = array
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .ok_or_else(|| MerkurError::Storage("Invalid embedding type".into()))?;
                if vector.len() != self.dim
                    || by_key
                        .insert(keys.value(i).to_string(), vector.values().to_vec())
                        .is_some()
                {
                    return Err(MerkurError::Storage(
                        "Invalid or duplicate published vector".into(),
                    ));
                }
            }
        }
        let mut out = HashMap::new();
        for (id, key, legacy) in references {
            if let Some(vector) = by_key.get(&key) {
                out.insert(id, vector.clone());
            } else if !legacy {
                return Err(MerkurError::Storage("Published vector is missing".into()));
            }
        }
        Ok(out)
    }
    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>> {
        // Legacy cross-bucket audit path — same rationale as the SQLite
        // backend: delegate to the default bucket until an audit endpoint
        // exists.
        self.vector_search_ns(vec, merkur_core::DEFAULT_NAMESPACE, limit)
            .await
    }

    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()> {
        let edge = edge.clone();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::insert_edge(&pool, &edge)).await
    }

    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<Edge>> {
        let id_owned = memory_id.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::get_edges(&pool, &id_owned)).await
    }

    async fn get_edges_batch(
        &self,
        memory_ids: &[String],
    ) -> MerkurResult<HashMap<String, Vec<Edge>>> {
        let ids = memory_ids.to_vec();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::get_edges_batch(&pool, &ids)).await
    }

    async fn bfs_expand(
        &self,
        seeds: &[(String, f64)],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        // Legacy cross-bucket traversal.
        let seeds = seeds.to_vec();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::bfs_expand(&pool, &seeds, None, depth, degree_limit))
            .await
    }

    async fn bfs_expand_ns(
        &self,
        seeds: &[(String, f64)],
        namespace: &str,
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        let seeds = seeds.to_vec();
        let namespace = namespace.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || {
            sqlite_helpers::bfs_expand(&pool, &seeds, Some(&namespace), depth, degree_limit)
        })
        .await
    }

    async fn insert_context_tag(
        &self,
        memory_id: &str,
        key: &str,
        value: &str,
    ) -> MerkurResult<()> {
        let mid = memory_id.to_string();
        let k = key.to_string();
        let v = value.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::insert_context_tag(&pool, &mid, &k, &v)).await
    }

    async fn search_by_context(
        &self,
        filters: &HashMap<String, String>,
    ) -> MerkurResult<Vec<String>> {
        let f = filters.clone();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::search_by_context(&pool, &f)).await
    }

    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let pool = self.sqlite_pool.clone();
        let ids = run_blocking(move || sqlite_helpers::list_pending_ids(&pool, limit)).await?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn list_for_forgetting(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let pool = self.sqlite_pool.clone();
        let ids = run_blocking(move || sqlite_helpers::list_forgetting_ids(&pool, limit)).await?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn mark_consolidated(&self, ids: &[String]) -> MerkurResult<()> {
        let ids = ids.to_vec();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::mark_consolidated(&pool, &ids)).await
    }

    async fn mark_consolidated_if_unchanged(
        &self,
        snapshots: &[merkur_core::MemorySnapshot],
    ) -> MerkurResult<()> {
        let snapshots = snapshots.to_vec();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::mark_consolidated_if_unchanged(&pool, &snapshots))
            .await
    }

    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()> {
        let id_owned = id.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::update_level(&pool, &id_owned, level)).await
    }

    async fn update_abstract(&self, id: &str, abstract_: &str) -> MerkurResult<()> {
        let id_owned = id.to_string();
        let abstract_owned = abstract_.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::update_abstract(&pool, &id_owned, &abstract_owned))
            .await
    }

    async fn update_importance(&self, id: &str, importance: f64) -> MerkurResult<()> {
        let id_owned = id.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::update_importance(&pool, &id_owned, importance)).await
    }

    async fn apply_adjudication(
        &self,
        write: &merkur_core::AdjudicationWrite,
    ) -> MerkurResult<bool> {
        if let merkur_core::AdjudicationWrite::Absorb { pending, target } = write {
            if pending.id == target.id || pending.namespace != target.namespace {
                return Ok(false);
            }
            let Some(source) = self.get_memory(&pending.id).await? else {
                return Ok(false);
            };
            if !merkur_core::MemorySnapshot::from(&source).eq(pending)
                || source.invalid_at.is_some()
                || source.level.to_i32() < 0
                || !source.pending_consolidation
            {
                return Ok(false);
            }
            // Legacy NULL references also represent optional, absent vectors.
            // Verify existence in Lance; SQL rechecks both revisions at commit.
            if !self
                .get_embeddings(std::slice::from_ref(&pending.id))
                .await?
                .contains_key(&pending.id)
            {
                return Err(MerkurError::Storage(
                    "adjudication source has no vector".into(),
                ));
            }
        }
        let obsolete = if let merkur_core::AdjudicationWrite::Absorb { target, .. } = write {
            self.vector_references(std::slice::from_ref(&target.id))
                .await?
                .into_iter()
                .map(|(_, key, _)| key)
                .collect()
        } else {
            Vec::new()
        };
        let pool = self.sqlite_pool.clone();
        let write = write.clone();
        let applied =
            run_blocking(move || sqlite_helpers::apply_adjudication(&pool, &write, true)).await?;
        if applied && let Err(e) = self.reclaim_vector_keys(&obsolete).await {
            warn!(error = %e, "Retired vector cleanup failed after absorption");
        }
        Ok(applied)
    }

    async fn invalidate_memory(&self, id: &str, absorbed_into: Option<&str>) -> MerkurResult<()> {
        // Rows live in the shared SQLite database; the Lance side keeps the
        // vector, which every retrieval path filters via the SQL post-filter
        // until the retention purge removes it.
        let id = id.to_string();
        let absorbed = absorbed_into.map(str::to_string);
        let pool = self.sqlite_pool.clone();
        run_blocking(move || {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            let now = Utc::now().to_rfc3339();
            // Idempotent: the first invalidation wins, keeping the original
            // timestamp and absorb pointer for audit.
            match absorbed {
                Some(target) => conn.execute(
                    "UPDATE memories SET invalid_at = ?1, updated_at = ?1,
                        metadata = json_set(metadata, '$.absorbed_into', ?2)
                     WHERE id = ?3 AND invalid_at IS NULL",
                    params![now, target, id],
                ),
                None => conn.execute(
                    "UPDATE memories SET invalid_at = ?1, updated_at = ?1
                     WHERE id = ?2 AND invalid_at IS NULL",
                    params![now, id],
                ),
            }
            .map_err(|e| MerkurError::Storage(format!("Failed to invalidate memory: {e}")))?;
            Ok(())
        })
        .await
    }

    async fn cleanup_orphan_vectors(&self) -> MerkurResult<usize> {
        use futures::TryStreamExt;
        use lancedb::query::{ExecutableQuery, QueryBase, Select};
        let cutoff = (Utc::now() - chrono::Duration::seconds(GC_GRACE_SECONDS)).to_rfc3339();
        let pool = self.sqlite_pool.clone();
        let before = cutoff.clone();
        let queued = run_blocking(move || {
            let conn = pool.get().map_err(|e| MerkurError::Storage(e.to_string()))?;
            let mut stmt = conn.prepare("SELECT vector_id FROM lance_vector_staging
                WHERE state = 'collecting' OR created_at < ?1 ORDER BY created_at, vector_id LIMIT ?2")
                .map_err(|e| MerkurError::Storage(e.to_string()))?;
            stmt.query_map(params![before, GC_BATCH_SIZE], |r| r.get::<_, String>(0))
                .map_err(|e| MerkurError::Storage(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| MerkurError::Storage(e.to_string()))
        }).await?;
        let claimed = self.claim_gc_keys(&queued, &cutoff, GC_BATCH_SIZE).await?;
        let mut processed = self.delete_gc_keys(&claimed).await?;
        let mut completed: std::collections::HashSet<_> = claimed.into_iter().collect();
        if processed >= GC_BATCH_SIZE {
            return Ok(processed);
        }
        let table = self.get_table().await?;
        let mut stream = table
            .query()
            .select(Select::columns(&["id"]))
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("scan orphan keys: {e}")))?;
        let mut candidates = Vec::new();
        while let Some(batch) = stream
            .try_next()
            .await
            .map_err(|e| MerkurError::Storage(e.to_string()))?
        {
            let ids = batch
                .column_by_name("id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .ok_or_else(|| MerkurError::Storage("Invalid vector key column".into()))?;
            for i in 0..batch.num_rows() {
                let key = ids.value(i);
                if completed.contains(key) || Self::quote_id_strict(key).is_err() {
                    continue;
                }
                candidates.push(key.to_string());
                if candidates.len() == GC_BATCH_SIZE {
                    let claimed = self
                        .claim_gc_keys(&candidates, &cutoff, GC_BATCH_SIZE - processed)
                        .await?;
                    processed += self.delete_gc_keys(&claimed).await?;
                    completed.extend(claimed);
                    candidates.clear();
                    if processed >= GC_BATCH_SIZE {
                        return Ok(processed);
                    }
                }
            }
        }
        let claimed = self
            .claim_gc_keys(&candidates, &cutoff, GC_BATCH_SIZE - processed)
            .await?;
        processed += self.delete_gc_keys(&claimed).await?;
        Ok(processed)
    }

    async fn purge_invalidated_older_than(&self, days: i32) -> MerkurResult<usize> {
        self.delete_expired(days, true).await
    }

    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize> {
        self.delete_expired(days, false).await
    }

    async fn log_consolidation(
        &self,
        started_at: chrono::DateTime<chrono::Utc>,
        finished_at: chrono::DateTime<chrono::Utc>,
        report: &ConsolidationReport,
    ) -> MerkurResult<()> {
        let report = report.clone();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || {
            sqlite_helpers::log_consolidation(&pool, started_at, finished_at, &report)
        })
        .await
    }

    async fn get_consolidation_log(
        &self,
        limit: usize,
    ) -> MerkurResult<Vec<ConsolidationLogEntry>> {
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::get_consolidation_log(&pool, limit)).await
    }

    async fn stats(&self) -> MerkurResult<StorageStats> {
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::stats(&pool)).await
    }

    async fn memory_exists(&self, id: &str) -> MerkurResult<bool> {
        let id_owned = id.to_string();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::memory_exists(&pool, &id_owned)).await
    }

    async fn memory_exists_batch(
        &self,
        ids: &[String],
    ) -> MerkurResult<std::collections::HashSet<String>> {
        let ids = ids.to_vec();
        let pool = self.sqlite_pool.clone();
        run_blocking(move || sqlite_helpers::memory_exists_batch(&pool, &ids)).await
    }
}

#[cfg(test)]
mod maintenance_tests {
    use super::*;

    #[tokio::test]
    async fn legacy_l2_indexes_are_bypassed_then_replaced_without_retraining() -> MerkurResult<()> {
        let dir = std::env::temp_dir().join(format!("merkur_cos_index_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sqlite = dir.join("meta.db").to_string_lossy().to_string();
        let lance = dir.join("lance").to_string_lossy().to_string();
        let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while storage.index_building.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Hold automatic maintenance while constructing a real legacy fixture.
        storage.index_building.store(true, Ordering::Release);
        for (content, vector) in [
            ("aligned", vec![10.0, 0.0, 0.0, 0.0]),
            (
                "angled",
                vec![0.85, (1.0_f32 - 0.85 * 0.85).sqrt(), 0.0, 0.0],
            ),
            ("third", vec![0.6, 0.8, 0.0, 0.0]),
        ] {
            storage
                .insert_memory(&NewMemory {
                    content: content.into(),
                    category: None,
                    metadata: HashMap::new(),
                    context: HashMap::new(),
                    namespace: "alpha".into(),
                    embedding: Some(vector),
                })
                .await?;
        }
        let keys: Vec<_> = (0..512)
            .map(|_| format!("mem_{}", uuid::Uuid::new_v4()))
            .collect();
        let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            (0..512).map(|i| {
                Some(vec![
                    Some((i % 31 + 1) as f32),
                    Some((i * 17 % 97 + 10) as f32),
                    Some((i * 7 % 53 + 1) as f32),
                    Some(1.0),
                ])
            }),
            4,
        );
        let table = storage.get_table().await?;
        let batch = RecordBatch::try_new(
            storage.vector_schema(),
            vec![Arc::new(StringArray::from(keys)), Arc::new(vectors)],
        )
        .unwrap();
        table.add(vec![batch]).execute().await.unwrap();
        table
            .create_index(
                &["vector"],
                Index::IvfPq(
                    IvfPqIndexBuilder::default()
                        .distance_type(lancedb::DistanceType::L2)
                        .num_partitions(1)
                        .num_sub_vectors(1),
                ),
            )
            .execute()
            .await
            .unwrap();
        assert_eq!(
            vector_index_metrics(&table).await?,
            vec![Some(lancedb::DistanceType::L2)]
        );
        let hits = storage
            .vector_search_ns(&[7.0, 0.0, 0.0, 0.0], "alpha", 3)
            .await?;
        for (content, score) in [("aligned", 1.0), ("angled", 0.85), ("third", 0.6)] {
            assert!(
                (hits.iter().find(|h| h.content == content).unwrap().score - score).abs() < 1e-5
            );
        }
        ensure_cosine_index(&table).await?;
        assert_eq!(
            vector_index_metrics(&table).await?,
            vec![Some(lancedb::DistanceType::Cosine)]
        );
        let hits = storage
            .vector_search_ns(&[7.0, 0.0, 0.0, 0.0], "alpha", 3)
            .await?;
        for (content, score) in [("aligned", 1.0), ("angled", 0.85), ("third", 0.6)] {
            assert!(
                (hits.iter().find(|h| h.content == content).unwrap().score - score).abs() < 1e-5,
                "indexed score for {content} must be refined to raw cosine"
            );
        }
        storage
            .insert_memory(&NewMemory {
                content: "unindexed tail".into(),
                category: None,
                metadata: HashMap::new(),
                context: HashMap::new(),
                namespace: "tail".into(),
                embedding: Some(vec![3.0, 0.0, 0.0, 0.0]),
            })
            .await?;
        let tail = storage
            .vector_search_ns(&[7.0, 0.0, 0.0, 0.0], "tail", 1)
            .await?;
        assert_eq!(
            tail[0].content, "unindexed tail",
            "new appends must remain searchable before index maintenance"
        );
        assert!((tail[0].score - 1.0).abs() < 1e-5);
        let version = table.version().await.unwrap();
        ensure_cosine_index(&table).await?;
        assert_eq!(
            table.version().await.unwrap(),
            version,
            "new appends must not retrain a matching index"
        );
        drop(table);
        drop(storage);
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }

    #[tokio::test]
    async fn gc_streams_legacy_orphans_in_bounded_batches() -> MerkurResult<()> {
        let dir = std::env::temp_dir().join(format!("merkur_gc_batch_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sqlite = dir.join("meta.db").to_string_lossy().to_string();
        let lance = dir.join("lance").to_string_lossy().to_string();
        let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
        let target = storage
            .insert_memory(&NewMemory {
                content: "survivor".into(),
                category: None,
                context: HashMap::new(),
                metadata: HashMap::new(),
                namespace: "default".into(),
                embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            })
            .await?;
        let keys: Vec<_> = (0..129)
            .map(|_| format!("mem_{}", uuid::Uuid::new_v4()))
            .collect();
        let vectors = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            vec![Some(vec![Some(1.0), Some(0.0), Some(0.0), Some(0.0)]); 129],
            4,
        );
        let batch = RecordBatch::try_new(
            storage.vector_schema(),
            vec![Arc::new(StringArray::from(keys)), Arc::new(vectors)],
        )
        .unwrap();
        let table = storage.get_table().await?;
        table.add(vec![batch]).execute().await.unwrap();
        assert_eq!(storage.cleanup_orphan_vectors().await?, 128);
        assert_eq!(storage.cleanup_orphan_vectors().await?, 1);
        assert_eq!(storage.cleanup_orphan_vectors().await?, 0);
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
        assert_eq!(
            storage
                .get_embeddings(std::slice::from_ref(&target))
                .await?
                .len(),
            1
        );
        drop(table);
        drop(storage);
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }

    #[tokio::test]
    async fn gc_fences_expired_publication_and_preserves_fresh_stages() -> MerkurResult<()> {
        let dir = std::env::temp_dir().join(format!("merkur_gc_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sqlite = dir.join("meta.db").to_string_lossy().to_string();
        let lance = dir.join("lance").to_string_lossy().to_string();
        let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
        let target = storage
            .insert_memory(&NewMemory {
                content: "original".into(),
                category: None,
                context: HashMap::new(),
                metadata: HashMap::new(),
                namespace: "default".into(),
                embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            })
            .await?;
        let fresh = storage.stage_vector(Some(&[0.0, 1.0, 0.0, 0.0])).await?;
        assert_eq!(
            storage.cleanup_orphan_vectors().await?,
            0,
            "a fresh unpublished stage must survive GC"
        );
        storage
            .publish_vector_update(&target, "fresh", fresh)
            .await?;

        let expired = storage.stage_vector(Some(&[0.0, 0.0, 1.0, 0.0])).await?;
        let interrupted = storage.stage_vector(Some(&[0.0, 0.0, 0.0, 1.0])).await?;
        let conn = rusqlite::Connection::open(&sqlite).unwrap();
        conn.execute("UPDATE lance_vector_staging SET created_at = '2000-01-01T00:00:00+00:00' WHERE vector_id = ?1", params![expired]).unwrap();
        conn.execute(
            "UPDATE lance_vector_staging SET state = 'collecting' WHERE vector_id = ?1",
            params![interrupted],
        )
        .unwrap();
        let restarted = LanceDbStorage::new(&lance, &sqlite, 4).await?;
        assert_eq!(
            restarted.cleanup_orphan_vectors().await?,
            2,
            "retry an interrupted collection after restart"
        );
        assert!(
            storage
                .publish_vector_update(&target, "expired", expired)
                .await
                .is_err()
        );
        assert!(
            storage
                .publish_vector_update(&target, "interrupted", interrupted)
                .await
                .is_err()
        );
        assert_eq!(storage.get_memory(&target).await?.unwrap().content, "fresh");
        assert_eq!(
            storage
                .get_embeddings(std::slice::from_ref(&target))
                .await?[&target],
            vec![0.0, 1.0, 0.0, 0.0]
        );
        assert_eq!(storage.cleanup_orphan_vectors().await?, 0);
        drop(conn);
        drop(restarted);
        drop(storage);
        std::fs::remove_dir_all(dir).ok();
        Ok(())
    }
}
