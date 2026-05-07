use async_trait::async_trait;
use chrono::Utc;
use merkur_core::{
    ConsolidationLogEntry, ConsolidationReport, Edge, Memory, MemoryLevel, MerkurError,
    MerkurResult, NewEdge, NewMemory, ScoredMemory, Storage, StorageStats,
};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task;
use tracing::info;

use crate::InMemoryVectorIndex;
use crate::sqlite_helpers;

/// DDL is executed once at startup. `journal_mode = WAL` is database-level so a
/// single application is enough; `foreign_keys = ON` is per-connection and is
/// enforced via the pool's `with_init` hook in `sqlite_helpers::build_pool`.
const DDL: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id                     TEXT PRIMARY KEY,
    content                TEXT NOT NULL,
    abstract               TEXT DEFAULT '',
    category               TEXT DEFAULT 'general',
    weight                 REAL NOT NULL DEFAULT 1.0,
    level                  INTEGER NOT NULL DEFAULT 2,
    pending_consolidation  INTEGER NOT NULL DEFAULT 1,
    embedding              BLOB,
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
    errors              INTEGER NOT NULL DEFAULT 0
);
";

pub struct SqliteStorage {
    pool: Pool<SqliteConnectionManager>,
    vector_index: Arc<InMemoryVectorIndex>,
}

impl SqliteStorage {
    pub fn new(path: &str, embedding_dim: usize) -> MerkurResult<Self> {
        let pool = sqlite_helpers::build_pool(path, 10)?;

        let conn = pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute_batch(DDL)
            .map_err(|e| MerkurError::Storage(format!("Failed to init schema: {e}")))?;
        drop(conn);

        let vector_index = Arc::new(InMemoryVectorIndex::new(embedding_dim));

        let storage = Self { pool, vector_index };
        storage.load_vectors_from_db()?;
        info!(
            "SqliteStorage initialized, loaded {} vectors",
            storage.vector_index.len()
        );

        Ok(storage)
    }

    pub fn new_in_memory(embedding_dim: usize) -> MerkurResult<Self> {
        Self::new("file::memory:?cache=shared", embedding_dim)
    }

    fn load_vectors_from_db(&self) -> MerkurResult<()> {
        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let mut stmt = conn
            .prepare("SELECT id, embedding FROM memories WHERE embedding IS NOT NULL")
            .map_err(|e| MerkurError::Storage(format!("Failed to prepare statement: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, blob))
            })
            .map_err(|e| MerkurError::Storage(format!("Failed to query embeddings: {e}")))?;

        let mut vectors = Vec::new();
        for row in rows {
            let (id, blob) = row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
            let vec = bytes_to_vec_f32(&blob);
            vectors.push((id, vec));
        }
        self.vector_index.rebuild(vectors);
        Ok(())
    }
}

/// Wrap a synchronous rusqlite operation so it runs on Tokio's blocking pool.
///
/// All `Storage` methods take `&self` and need `Send + Sync` futures, so the
/// closure must be `'static`. We clone the pool (cheap — `r2d2::Pool` is `Arc`
/// inside) into the closure.
async fn run_blocking<F, T>(f: F) -> MerkurResult<T>
where
    F: FnOnce() -> MerkurResult<T> + Send + 'static,
    T: Send + 'static,
{
    task::spawn_blocking(f)
        .await
        .map_err(|e| MerkurError::Internal(format!("blocking task panicked: {e}")))?
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String> {
        let id = format!("mem_{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let metadata = serde_json::to_string(&mem.metadata)
            .map_err(|e| MerkurError::Storage(format!("Failed to serialize metadata: {e}")))?;
        let category = mem.category.clone().unwrap_or_else(|| "general".into());
        let embedding_blob = mem.embedding.as_ref().map(|v| vec_f32_to_bytes(v));
        let context = mem.context.clone();
        let content = mem.content.clone();

        let pool = self.pool.clone();
        let id_for_db = id.clone();
        run_blocking(move || -> MerkurResult<()> {
            let mut conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            // Atomic: memory + context tags must commit together.
            let tx = conn
                .transaction()
                .map_err(|e| MerkurError::Storage(format!("begin tx failed: {e}")))?;
            tx.execute(
                "INSERT INTO memories (id, content, category, weight, level, pending_consolidation, embedding, metadata, created_at, updated_at, accessed_at)
                 VALUES (?1, ?2, ?3, 1.0, 2, 1, ?4, ?5, ?6, ?6, ?6)",
                params![id_for_db, content, category, embedding_blob, metadata, now],
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

        // Vector index is updated only after the DB commit succeeds, so a
        // failed write never leaves a phantom vector behind.
        if let Some(embedding) = mem.embedding.clone() {
            self.vector_index.add(id.clone(), embedding);
        }

        Ok(id)
    }

    async fn update_memory(
        &self,
        id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> MerkurResult<()> {
        let embedding_blob = embedding.map(vec_f32_to_bytes);
        let id_owned = id.to_string();
        let content_owned = content.to_string();
        let pool = self.pool.clone();

        let affected = run_blocking(move || -> MerkurResult<usize> {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            conn.execute(
                "UPDATE memories SET content = ?1, embedding = ?2, pending_consolidation = 1, updated_at = ?3 WHERE id = ?4",
                params![content_owned, embedding_blob, Utc::now().to_rfc3339(), id_owned],
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to update memory: {e}")))
        })
        .await?;

        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }
        if let Some(vec) = embedding {
            self.vector_index.add(id.to_string(), vec.to_vec());
        } else {
            // Explicitly invalidate the in-memory vector when the caller cleared
            // the embedding column, keeping vector_index in sync with DB state.
            self.vector_index.remove(id);
        }
        Ok(())
    }

    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>> {
        let id_owned = id.to_string();
        let pool = self.pool.clone();

        run_blocking(move || -> MerkurResult<Option<Memory>> {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, content, abstract, category, weight, level, pending_consolidation, embedding, metadata, created_at, updated_at, accessed_at, access_count
                     FROM memories WHERE id = ?1",
                )
                .map_err(|e| MerkurError::Storage(format!("Failed to prepare statement: {e}")))?;

            let result = stmt.query_row(params![id_owned], |row| {
                let blob: Option<Vec<u8>> = row.get(7)?;
                let metadata_str: String = row.get(8)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, bool>(6)?,
                    blob.map(|b| bytes_to_vec_f32(&b)),
                    metadata_str,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)? as u64,
                ))
            });

            match result {
                Ok((
                    id,
                    content,
                    abstract_,
                    category,
                    weight,
                    level_i32,
                    pending,
                    embedding,
                    metadata_str,
                    created_at,
                    updated_at,
                    accessed_at,
                    access_count,
                )) => {
                    let level = MemoryLevel::from_i32(level_i32);
                    let metadata: HashMap<String, serde_json::Value> =
                        serde_json::from_str(&metadata_str).unwrap_or_default();
                    let context = sqlite_helpers::get_context_tags(&pool, &id)?;
                    Ok(Some(Memory {
                        id,
                        content,
                        abstract_,
                        category,
                        weight,
                        level,
                        pending_consolidation: pending,
                        embedding,
                        metadata,
                        context,
                        created_at: parse_dt(&created_at),
                        updated_at: parse_dt(&updated_at),
                        accessed_at: parse_dt(&accessed_at),
                        access_count,
                    }))
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(MerkurError::Storage(format!("Failed to query memory: {e}"))),
            }
        })
        .await
    }

    async fn delete_memory(&self, id: &str) -> MerkurResult<()> {
        let id_owned = id.to_string();
        let pool = self.pool.clone();

        let affected = run_blocking(move || -> MerkurResult<usize> {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            conn.execute("DELETE FROM memories WHERE id = ?1", params![id_owned])
                .map_err(|e| MerkurError::Storage(format!("Failed to delete memory: {e}")))
        })
        .await?;

        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }
        self.vector_index.remove(id);
        Ok(())
    }

    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>> {
        // Ask for more than `limit` so we can drop archived rows without
        // shrinking the result set below the caller's expectation.
        let oversample = limit.saturating_mul(2).max(limit);
        let scored_ids = self.vector_index.search(vec, oversample);
        if scored_ids.is_empty() {
            return Ok(Vec::new());
        }

        let pool = self.pool.clone();
        let ids: Vec<String> = scored_ids.iter().map(|(id, _)| id.clone()).collect();
        let scores: HashMap<String, f64> = scored_ids.into_iter().collect();
        let ids_for_query = ids.clone();

        let memories = run_blocking(move || -> MerkurResult<Vec<(String, String, Option<String>, String, f64, i32, String)>> {
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            let ids_json = serde_json::to_string(&ids_for_query)
                .map_err(|e| MerkurError::Storage(format!("Failed to encode ids: {e}")))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, content, abstract, category, weight, level, created_at
                     FROM memories
                     WHERE id IN (SELECT value FROM json_each(?1))
                       AND level >= 0",
                )
                .map_err(|e| MerkurError::Storage(format!("Failed to prepare batch query: {e}")))?;
            let rows = stmt
                .query_map(params![ids_json], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, i32>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .map_err(|e| MerkurError::Storage(format!("Batch query failed: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?);
            }
            Ok(out)
        })
        .await?;

        // Build a single batch context-tag fetch to avoid N round-trips.
        let pool2 = self.pool.clone();
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
                |(id, content, abstract_, category, weight, level_i32, created_at)| {
                    let level = MemoryLevel::from_i32(level_i32);
                    let score = scores.get(&id).copied().unwrap_or(0.0);
                    let context = ctx_map.get(&id).cloned().unwrap_or_default();
                    ScoredMemory {
                        id,
                        content,
                        abstract_,
                        score,
                        weight,
                        level,
                        category,
                        context,
                        created_at: parse_dt(&created_at),
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

        // Fire-and-forget access tracking. Errors are logged inside.
        let pool3 = self.pool.clone();
        let touched: Vec<String> = out.iter().map(|s| s.id.clone()).collect();
        tokio::spawn(async move {
            let _ =
                task::spawn_blocking(move || sqlite_helpers::update_access(&pool3, &touched)).await;
        });

        Ok(out)
    }

    async fn rebuild_vector_index(&self, all: &[(String, Vec<f32>)]) -> MerkurResult<()> {
        self.vector_index.rebuild(all.to_vec());
        Ok(())
    }

    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()> {
        let edge = edge.clone();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::insert_edge(&pool, &edge)).await
    }

    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<Edge>> {
        let id_owned = memory_id.to_string();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::get_edges(&pool, &id_owned)).await
    }

    async fn bfs_expand(
        &self,
        seed_ids: &[String],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        let seeds = seed_ids.to_vec();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::bfs_expand(&pool, &seeds, depth, degree_limit)).await
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
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::insert_context_tag(&pool, &mid, &k, &v)).await
    }

    async fn search_by_context(
        &self,
        filters: &HashMap<String, String>,
    ) -> MerkurResult<Vec<String>> {
        let f = filters.clone();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::search_by_context(&pool, &f)).await
    }

    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let pool = self.pool.clone();
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
        let pool = self.pool.clone();
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
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::mark_consolidated(&pool, &ids)).await
    }

    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()> {
        let id_owned = id.to_string();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::update_level(&pool, &id_owned, level)).await
    }

    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize> {
        let pool = self.pool.clone();
        let vector_index = self.vector_index.clone();
        run_blocking(move || -> MerkurResult<usize> {
            let threshold = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
            let conn = pool
                .get()
                .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
            let mut stmt = conn
                .prepare("SELECT id FROM memories WHERE level = -1 AND updated_at < ?1")
                .map_err(|e| {
                    MerkurError::Storage(format!("Failed to prepare delete query: {e}"))
                })?;
            let ids: Vec<String> = stmt
                .query_map(params![threshold], |row| row.get::<_, String>(0))
                .map_err(|e| MerkurError::Storage(format!("Failed to query archived: {e}")))?
                .filter_map(|r| r.ok())
                .collect();

            let count = conn
                .execute(
                    "DELETE FROM memories WHERE level = -1 AND updated_at < ?1",
                    params![threshold],
                )
                .map_err(|e| MerkurError::Storage(format!("Failed to delete archived: {e}")))?;

            for id in &ids {
                vector_index.remove(id);
            }
            Ok(count)
        })
        .await
    }

    async fn log_consolidation(
        &self,
        started_at: chrono::DateTime<chrono::Utc>,
        finished_at: chrono::DateTime<chrono::Utc>,
        report: &ConsolidationReport,
    ) -> MerkurResult<()> {
        let report = report.clone();
        let pool = self.pool.clone();
        run_blocking(move || {
            sqlite_helpers::log_consolidation(&pool, started_at, finished_at, &report)
        })
        .await
    }

    async fn get_consolidation_log(
        &self,
        limit: usize,
    ) -> MerkurResult<Vec<ConsolidationLogEntry>> {
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::get_consolidation_log(&pool, limit)).await
    }

    async fn stats(&self) -> MerkurResult<StorageStats> {
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::stats(&pool)).await
    }

    async fn memory_exists(&self, id: &str) -> MerkurResult<bool> {
        let id_owned = id.to_string();
        let pool = self.pool.clone();
        run_blocking(move || sqlite_helpers::memory_exists(&pool, &id_owned)).await
    }
}

fn parse_dt(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .unwrap_or_else(|_| Utc::now())
}

fn vec_f32_to_bytes(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_vec_f32(bytes: &[u8]) -> Vec<f32> {
    debug_assert!(
        bytes.len().is_multiple_of(4),
        "embedding blob length {} is not a multiple of 4",
        bytes.len()
    );
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}
