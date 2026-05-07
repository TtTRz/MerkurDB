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
use tracing::info;

use crate::InMemoryVectorIndex;
use crate::sqlite_helpers;

const DDL: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;

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
    vector_index: InMemoryVectorIndex,
}

impl SqliteStorage {
    pub fn new(path: &str, embedding_dim: usize) -> MerkurResult<Self> {
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(10)
            .build(manager)
            .map_err(|e| MerkurError::Storage(format!("Failed to create connection pool: {e}")))?;

        let conn = pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute_batch(DDL)
            .map_err(|e| MerkurError::Storage(format!("Failed to init schema: {e}")))?;

        let vector_index = InMemoryVectorIndex::new(embedding_dim);

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

#[async_trait]
impl Storage for SqliteStorage {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String> {
        let id = format!("mem_{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let metadata = serde_json::to_string(&mem.metadata)
            .map_err(|e| MerkurError::Storage(format!("Failed to serialize metadata: {e}")))?;
        let category = mem.category.clone().unwrap_or_else(|| "general".into());
        let embedding_blob = mem.embedding.as_ref().map(|v| vec_f32_to_bytes(v));

        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute(
            "INSERT INTO memories (id, content, category, weight, level, pending_consolidation, embedding, metadata, created_at, updated_at, accessed_at)
             VALUES (?1, ?2, ?3, 1.0, 2, 1, ?4, ?5, ?6, ?6, ?6)",
            params![id, mem.content, category, embedding_blob, metadata, now],
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to insert memory: {e}")))?;

        if let Some(ref embedding) = mem.embedding {
            self.vector_index.add(id.clone(), embedding.clone());
        }

        for (key, value) in &mem.context {
            sqlite_helpers::insert_context_tag(&self.pool, &id, key, value)?;
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
        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let affected = conn
            .execute(
                "UPDATE memories SET content = ?1, embedding = ?2, pending_consolidation = 1, updated_at = ?3 WHERE id = ?4",
                params![content, embedding_blob, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to update memory: {e}")))?;
        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }
        if let Some(vec) = embedding {
            self.vector_index.add(id.to_string(), vec.to_vec());
        }
        Ok(())
    }

    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>> {
        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, content, abstract, category, weight, level, pending_consolidation, embedding, metadata, created_at, updated_at, accessed_at, access_count
                 FROM memories WHERE id = ?1",
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to prepare statement: {e}")))?;

        let result = stmt
            .query_row(params![id], |row| {
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
            })
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => MerkurError::MemoryNotFound(id.to_string()),
                other => MerkurError::Storage(format!("Failed to query memory: {other}")),
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
                sqlite_helpers::update_access(&self.pool, &id);

                let metadata: HashMap<String, serde_json::Value> =
                    serde_json::from_str(&metadata_str).unwrap_or_default();
                let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.into())
                    .unwrap_or_else(|_| Utc::now());
                let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)
                    .map(|dt| dt.into())
                    .unwrap_or_else(|_| Utc::now());
                let accessed_at = chrono::DateTime::parse_from_rfc3339(&accessed_at)
                    .map(|dt| dt.into())
                    .unwrap_or_else(|_| Utc::now());

                let context = self.get_context_tags(&id)?;

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
                    created_at,
                    updated_at,
                    accessed_at,
                    access_count,
                }))
            }
            Err(MerkurError::MemoryNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete_memory(&self, id: &str) -> MerkurResult<()> {
        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let affected = conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])
            .map_err(|e| MerkurError::Storage(format!("Failed to delete memory: {e}")))?;
        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }
        self.vector_index.remove(id);
        Ok(())
    }

    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>> {
        let scored_ids = self.vector_index.search(vec, limit);
        let mut results = Vec::with_capacity(scored_ids.len());
        for (id, score) in scored_ids {
            if let Some(memory) = self.get_memory(&id).await?
                && memory.level != MemoryLevel::Archived
            {
                results.push(ScoredMemory {
                    id: memory.id,
                    content: memory.content,
                    abstract_: memory.abstract_,
                    score,
                    weight: memory.weight,
                    level: memory.level,
                    category: memory.category,
                    context: memory.context,
                    created_at: memory.created_at,
                });
            }
        }
        Ok(results)
    }

    async fn rebuild_vector_index(&self, all: &[(String, Vec<f32>)]) -> MerkurResult<()> {
        self.vector_index.rebuild(all.to_vec());
        Ok(())
    }

    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()> {
        sqlite_helpers::insert_edge(&self.pool, edge)
    }

    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<Edge>> {
        sqlite_helpers::get_edges(&self.pool, memory_id)
    }

    async fn bfs_expand(
        &self,
        seed_ids: &[String],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        sqlite_helpers::bfs_expand(&self.pool, seed_ids, depth, degree_limit)
    }

    async fn insert_context_tag(
        &self,
        memory_id: &str,
        key: &str,
        value: &str,
    ) -> MerkurResult<()> {
        sqlite_helpers::insert_context_tag(&self.pool, memory_id, key, value)
    }

    async fn search_by_context(
        &self,
        filters: &HashMap<String, String>,
    ) -> MerkurResult<Vec<String>> {
        sqlite_helpers::search_by_context(&self.pool, filters)
    }

    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let ids = sqlite_helpers::list_pending_ids(&self.pool, limit)?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn list_for_forgetting(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let ids = sqlite_helpers::list_forgetting_ids(&self.pool, limit)?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn mark_consolidated(&self, ids: &[String]) -> MerkurResult<()> {
        sqlite_helpers::mark_consolidated(&self.pool, ids)
    }

    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()> {
        sqlite_helpers::update_level(&self.pool, id, level)
    }

    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize> {
        let threshold = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let conn = self
            .pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;

        // Collect IDs before deletion to clean up vector index
        let mut stmt = conn
            .prepare("SELECT id FROM memories WHERE level = -1 AND updated_at < ?1")
            .map_err(|e| MerkurError::Storage(format!("Failed to prepare delete query: {e}")))?;
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
            self.vector_index.remove(id);
        }

        Ok(count)
    }

    async fn log_consolidation(
        &self,
        started_at: chrono::DateTime<chrono::Utc>,
        finished_at: chrono::DateTime<chrono::Utc>,
        report: &ConsolidationReport,
    ) -> MerkurResult<()> {
        sqlite_helpers::log_consolidation(&self.pool, started_at, finished_at, report)
    }

    async fn get_consolidation_log(
        &self,
        limit: usize,
    ) -> MerkurResult<Vec<ConsolidationLogEntry>> {
        sqlite_helpers::get_consolidation_log(&self.pool, limit)
    }

    async fn stats(&self) -> MerkurResult<StorageStats> {
        sqlite_helpers::stats(&self.pool)
    }
}

impl SqliteStorage {
    fn get_context_tags(&self, memory_id: &str) -> MerkurResult<HashMap<String, String>> {
        sqlite_helpers::get_context_tags(&self.pool, memory_id)
    }
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
