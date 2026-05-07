use arrow_array::types::Float32Type;
use arrow_array::{FixedSizeListArray, Float32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
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
use tracing::{info, warn};

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

const VECTOR_TABLE: &str = "vectors";

pub struct LanceDbStorage {
    db: lancedb::Connection,
    sqlite_pool: Pool<SqliteConnectionManager>,
    table_name: String,
    dim: usize,
}

impl LanceDbStorage {
    pub async fn new(lance_path: &str, sqlite_path: &str, dim: usize) -> MerkurResult<Self> {
        let db = lancedb::connect(lance_path)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to connect to LanceDB: {e}")))?;

        let manager = SqliteConnectionManager::file(sqlite_path);
        let sqlite_pool = Pool::builder()
            .max_size(10)
            .build(manager)
            .map_err(|e| MerkurError::Storage(format!("Failed to create SQLite pool: {e}")))?;

        let conn = sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute_batch(DDL)
            .map_err(|e| MerkurError::Storage(format!("Failed to init SQLite schema: {e}")))?;

        let storage = Self {
            db,
            sqlite_pool,
            table_name: VECTOR_TABLE.to_string(),
            dim,
        };

        storage.ensure_vector_table().await?;
        info!("LanceDbStorage initialized: lance={lance_path}, sqlite={sqlite_path}, dim={dim}");

        Ok(storage)
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

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new(
                "vector",
                DataType::FixedSizeList(
                    Arc::new(Field::new("item", DataType::Float32, true)),
                    self.dim as i32,
                ),
                true,
            ),
        ]));

        let empty_batch = RecordBatch::new_empty(schema.clone());
        self.db
            .create_table(VECTOR_TABLE, empty_batch)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to create vector table: {e}")))?;

        if let Ok(table) = self.db.open_table(VECTOR_TABLE).execute().await {
            use lancedb::index::Index;
            if let Err(e) = table.create_index(&["vector"], Index::Auto).execute().await {
                warn!("Failed to create LanceDB vector index: {e}. Search may be slower.");
            }
        }

        Ok(())
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

    fn sanitize_id(id: &str) -> &str {
        debug_assert!(
            id.starts_with("mem_")
                && id
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_'),
            "Unexpected memory ID format: {id}"
        );
        id
    }

    fn sqlite(&self) -> &Pool<SqliteConnectionManager> {
        &self.sqlite_pool
    }
}

#[async_trait]
impl Storage for LanceDbStorage {
    async fn insert_memory(&self, mem: &NewMemory) -> MerkurResult<String> {
        let id = format!("mem_{}", uuid::Uuid::new_v4());
        let now = Utc::now().to_rfc3339();
        let metadata = serde_json::to_string(&mem.metadata)
            .map_err(|e| MerkurError::Storage(format!("Failed to serialize metadata: {e}")))?;
        let category = mem.category.clone().unwrap_or_else(|| "general".into());

        let conn = self
            .sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        conn.execute(
            "INSERT INTO memories (id, content, category, weight, level, pending_consolidation, metadata, created_at, updated_at, accessed_at)
             VALUES (?1, ?2, ?3, 1.0, 2, 1, ?4, ?5, ?5, ?5)",
            params![id, mem.content, category, metadata, now],
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to insert memory: {e}")))?;

        if let Some(ref embedding) = mem.embedding {
            let schema = Arc::new(Schema::new(vec![
                Field::new("id", DataType::Utf8, false),
                Field::new(
                    "vector",
                    DataType::FixedSizeList(
                        Arc::new(Field::new("item", DataType::Float32, true)),
                        self.dim as i32,
                    ),
                    true,
                ),
            ]));

            let id_array = Arc::new(StringArray::from(vec![id.clone()]));
            let vector_array = Arc::new(Self::vec_to_arrow(embedding, self.dim));
            let batch = RecordBatch::try_new(schema, vec![id_array, vector_array])
                .map_err(|e| MerkurError::Storage(format!("Failed to create record batch: {e}")))?;

            let table = self.get_table().await?;
            table
                .add(vec![batch])
                .execute()
                .await
                .map_err(|e| MerkurError::Storage(format!("Failed to insert vector: {e}")))?;
        }

        for (key, value) in &mem.context {
            sqlite_helpers::insert_context_tag(self.sqlite(), &id, key, value)?;
        }

        Ok(id)
    }

    async fn update_memory(
        &self,
        id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> MerkurResult<()> {
        let conn = self
            .sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let affected = conn
            .execute(
                "UPDATE memories SET content = ?1, pending_consolidation = 1, updated_at = ?2 WHERE id = ?3",
                params![content, Utc::now().to_rfc3339(), id],
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to update memory: {e}")))?;
        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }
        if let Some(vec) = embedding {
            let schema = Arc::new(arrow_schema::Schema::new(vec![
                arrow_schema::Field::new("id", arrow_schema::DataType::Utf8, false),
                arrow_schema::Field::new(
                    "vector",
                    arrow_schema::DataType::FixedSizeList(
                        Arc::new(arrow_schema::Field::new(
                            "item",
                            arrow_schema::DataType::Float32,
                            true,
                        )),
                        self.dim as i32,
                    ),
                    true,
                ),
            ]));
            let id_array = Arc::new(arrow_array::StringArray::from(vec![id.to_string()]));
            let vector_array = Arc::new(Self::vec_to_arrow(vec, self.dim));
            let batch = arrow_array::RecordBatch::try_new(schema, vec![id_array, vector_array])
                .map_err(|e| MerkurError::Storage(format!("Failed to create record batch: {e}")))?;

            // Delete old vector, then insert new
            let table = self.get_table().await?;
            let safe_id = Self::sanitize_id(id);
            let _ = table.delete(&format!("id = '{safe_id}'")).await;
            table
                .add(vec![batch])
                .execute()
                .await
                .map_err(|e| MerkurError::Storage(format!("Failed to update vector: {e}")))?;
        }
        Ok(())
    }

    async fn get_memory(&self, id: &str) -> MerkurResult<Option<Memory>> {
        let conn = self
            .sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, content, abstract, category, weight, level, pending_consolidation, metadata, created_at, updated_at, accessed_at, access_count
                 FROM memories WHERE id = ?1",
            )
            .map_err(|e| MerkurError::Storage(format!("Failed to prepare statement: {e}")))?;

        let result = stmt
            .query_row(params![id], |row| {
                let metadata_str: String = row.get(7)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, bool>(6)?,
                    metadata_str,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)? as u64,
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
                metadata_str,
                created_at,
                updated_at,
                accessed_at,
                access_count,
            )) => {
                let level = MemoryLevel::from_i32(level_i32);
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

                let context = sqlite_helpers::get_context_tags(&self.sqlite_pool, &id)?;
                sqlite_helpers::update_access(&self.sqlite_pool, &id);

                Ok(Some(Memory {
                    id,
                    content,
                    abstract_,
                    category,
                    weight,
                    level,
                    pending_consolidation: pending,
                    embedding: None, // Vector stored in LanceDB
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
            .sqlite_pool
            .get()
            .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
        let affected = conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])
            .map_err(|e| MerkurError::Storage(format!("Failed to delete memory: {e}")))?;
        if affected == 0 {
            return Err(MerkurError::MemoryNotFound(id.to_string()));
        }

        let table = self.get_table().await?;
        let safe_id = Self::sanitize_id(id);
        let _ = table
            .delete(&format!("id = '{safe_id}'"))
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to delete vector: {e}")))?;
        Ok(())
    }

    async fn vector_search(&self, vec: &[f32], limit: usize) -> MerkurResult<Vec<ScoredMemory>> {
        use futures::TryStreamExt;
        use lancedb::query::{ExecutableQuery, QueryBase};

        let table = self.get_table().await?;

        let results: Vec<_> = table
            .query()
            .nearest_to(vec)
            .map_err(|e| MerkurError::Storage(format!("Failed to create query: {e}")))?
            .limit(limit)
            .execute()
            .await
            .map_err(|e| MerkurError::Storage(format!("Vector search failed: {e}")))?
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| MerkurError::Storage(format!("Failed to collect results: {e}")))?;

        let mut scored = Vec::with_capacity(results.len());
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
                let distance = dist_array.value(i);
                // Convert distance to similarity score, clamp to [0, 1]
                let score = (1.0 - distance as f64 / 2.0).max(0.0);

                if let Some(memory) = self.get_memory(&id).await?
                    && memory.level != MemoryLevel::Archived
                {
                    scored.push(ScoredMemory {
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
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(scored)
    }

    async fn rebuild_vector_index(&self, _all: &[(String, Vec<f32>)]) -> MerkurResult<()> {
        Ok(())
    }

    // ── Delegated to sqlite_helpers ──

    async fn insert_edge(&self, edge: &NewEdge) -> MerkurResult<()> {
        sqlite_helpers::insert_edge(self.sqlite(), edge)
    }

    async fn get_edges(&self, memory_id: &str) -> MerkurResult<Vec<Edge>> {
        sqlite_helpers::get_edges(self.sqlite(), memory_id)
    }

    async fn bfs_expand(
        &self,
        seed_ids: &[String],
        depth: usize,
        degree_limit: usize,
    ) -> MerkurResult<Vec<ScoredMemory>> {
        sqlite_helpers::bfs_expand(self.sqlite(), seed_ids, depth, degree_limit)
    }

    async fn insert_context_tag(
        &self,
        memory_id: &str,
        key: &str,
        value: &str,
    ) -> MerkurResult<()> {
        sqlite_helpers::insert_context_tag(self.sqlite(), memory_id, key, value)
    }

    async fn search_by_context(
        &self,
        filters: &HashMap<String, String>,
    ) -> MerkurResult<Vec<String>> {
        sqlite_helpers::search_by_context(self.sqlite(), filters)
    }

    async fn list_pending(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let ids = sqlite_helpers::list_pending_ids(self.sqlite(), limit)?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn list_for_forgetting(&self, limit: usize) -> MerkurResult<Vec<Memory>> {
        let ids = sqlite_helpers::list_forgetting_ids(self.sqlite(), limit)?;
        let mut memories = Vec::new();
        for id in ids {
            if let Some(mem) = self.get_memory(&id).await? {
                memories.push(mem);
            }
        }
        Ok(memories)
    }

    async fn mark_consolidated(&self, ids: &[String]) -> MerkurResult<()> {
        sqlite_helpers::mark_consolidated(self.sqlite(), ids)
    }

    async fn update_level(&self, id: &str, level: i32) -> MerkurResult<()> {
        sqlite_helpers::update_level(self.sqlite(), id, level)
    }

    async fn delete_archived_older_than(&self, days: i32) -> MerkurResult<usize> {
        let threshold = (Utc::now() - chrono::Duration::days(days as i64)).to_rfc3339();
        let (ids, count) = {
            let conn = self
                .sqlite_pool
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

            (ids, count)
        };

        if !ids.is_empty() {
            let table = self.get_table().await?;
            let id_list = ids
                .iter()
                .map(|id| format!("'{}'", Self::sanitize_id(id)))
                .collect::<Vec<_>>()
                .join(",");
            let _ = table
                .delete(&format!("id IN ({id_list})"))
                .await
                .map_err(|e| MerkurError::Storage(format!("Failed to delete vectors: {e}")))?;
        }

        Ok(count)
    }

    async fn log_consolidation(
        &self,
        started_at: chrono::DateTime<chrono::Utc>,
        finished_at: chrono::DateTime<chrono::Utc>,
        report: &ConsolidationReport,
    ) -> MerkurResult<()> {
        sqlite_helpers::log_consolidation(self.sqlite(), started_at, finished_at, report)
    }

    async fn get_consolidation_log(
        &self,
        limit: usize,
    ) -> MerkurResult<Vec<ConsolidationLogEntry>> {
        sqlite_helpers::get_consolidation_log(self.sqlite(), limit)
    }

    async fn stats(&self) -> MerkurResult<StorageStats> {
        sqlite_helpers::stats(self.sqlite())
    }
}
