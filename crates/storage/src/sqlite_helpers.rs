use chrono::{DateTime, Utc};
use merkur_core::{
    ConsolidationLogEntry, ConsolidationReport, Edge, EdgeType, MemoryLevel, MerkurError,
    MerkurResult, NewEdge, ScoredMemory, StorageStats,
};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use std::collections::HashMap;
use tracing::warn;

/// Build a connection pool that enforces `foreign_keys=ON` on every connection.
///
/// SQLite's `foreign_keys` PRAGMA is per-connection, so DDL that merely sets it
/// once is insufficient for a pooled application — each new connection defaults
/// to OFF and `ON DELETE CASCADE` references silently become no-ops. Using an
/// init hook ensures every connection handed out by r2d2 has FKs enabled.
pub fn build_pool(path: &str, max_size: u32) -> MerkurResult<Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path).with_init(|conn| {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
    });
    Pool::builder()
        .max_size(max_size)
        .build(manager)
        .map_err(|e| MerkurError::Storage(format!("Failed to create connection pool: {e}")))
}

fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.into())
        .unwrap_or_else(|_| Utc::now())
}

/// Insert an edge into the SQLite edges table.
pub fn insert_edge(pool: &Pool<SqliteConnectionManager>, edge: &NewEdge) -> MerkurResult<()> {
    let now = Utc::now().to_rfc3339();
    let weight = edge.weight.unwrap_or(1.0);
    let relation = edge
        .relation
        .clone()
        .unwrap_or_else(|| "related".to_string());

    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    conn.execute(
        "INSERT OR IGNORE INTO edges (source_id, target_id, weight, relation, edge_type, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![edge.source_id, edge.target_id, weight, relation, edge.edge_type.as_db_str(), now],
    )
    .map_err(|e| MerkurError::Storage(format!("Failed to insert edge: {e}")))?;
    Ok(())
}

/// BFS expand from seed IDs using the edges table.
///
/// Seed IDs flow into SQL as a JSON array parameter (fully parameterized — no
/// string concatenation). Cycle detection uses a delimited path string
/// (`,id1,id2,` → substring match of `,id,`) so that IDs which are prefixes of
/// other IDs cannot cause false cycle hits.
pub fn bfs_expand(
    pool: &Pool<SqliteConnectionManager>,
    seed_ids: &[String],
    depth: usize,
    degree_limit: usize,
) -> MerkurResult<Vec<ScoredMemory>> {
    if seed_ids.is_empty() || depth == 0 {
        return Ok(Vec::new());
    }

    // Clamp to hard upper bounds to cap recursion cost.
    let depth = depth.min(merkur_core::limits::MAX_BFS_DEPTH);
    let degree_limit = degree_limit.min(merkur_core::limits::MAX_BFS_DEGREE);

    let seeds_json = serde_json::to_string(seed_ids)
        .map_err(|e| MerkurError::Storage(format!("Failed to encode seed ids: {e}")))?;

    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;

    // Path delimiters guarantee that LIKE '%,<id>,%' matches whole IDs only.
    let sql = "WITH RECURSIVE
            bfs(id, d, w, path) AS (
                SELECT value, 0, 1.0, ',' || value || ','
                FROM (SELECT DISTINCT value FROM json_each(?1))
                UNION
                SELECT
                    CASE WHEN e.source_id = bfs.id THEN e.target_id ELSE e.source_id END,
                    bfs.d + 1,
                    bfs.w * e.weight,
                    bfs.path || (CASE WHEN e.source_id = bfs.id THEN e.target_id ELSE e.source_id END) || ','
                FROM bfs
                JOIN edges e ON (
                    (e.edge_type = 'auto' AND (e.source_id = bfs.id OR e.target_id = bfs.id))
                    OR
                    (e.edge_type = 'manual' AND e.source_id = bfs.id)
                )
                WHERE bfs.d < ?2
                  AND bfs.path NOT LIKE '%,' || (CASE WHEN e.source_id = bfs.id THEN e.target_id ELSE e.source_id END) || ',%'
            )
        SELECT bfs.id, bfs.d, bfs.w, m.content, m.abstract, m.level, m.category, m.created_at
        FROM bfs
        JOIN memories m ON m.id = bfs.id
        WHERE bfs.d > 0 AND m.level >= 0
        ORDER BY bfs.d, bfs.w DESC
        LIMIT ?3";

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare BFS query: {e}")))?;
    let rows = stmt
        .query_map(
            params![seeds_json, depth as i64, degree_limit as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i32>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .map_err(|e| MerkurError::Storage(format!("BFS query failed: {e}")))?;

    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for row in rows {
        let (id, bfs_depth, weight, content, abstract_, level_i32, category, created_at) =
            row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
        if !seen.insert(id.clone()) {
            continue;
        }

        let level = MemoryLevel::from_i32(level_i32);
        let decay = 0.5_f64.powi(bfs_depth);
        let score = decay * weight;
        let created_at = parse_rfc3339(&created_at);
        let context = get_context_tags(pool, &id)?;

        results.push(ScoredMemory {
            id,
            content,
            abstract_,
            score,
            weight,
            level,
            category,
            context,
            created_at,
        });
    }

    Ok(results)
}

/// Insert a context tag.
pub fn insert_context_tag(
    pool: &Pool<SqliteConnectionManager>,
    memory_id: &str,
    key: &str,
    value: &str,
) -> MerkurResult<()> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    conn.execute(
        "INSERT INTO context_tags (memory_id, key, value) VALUES (?1, ?2, ?3)",
        params![memory_id, key, value],
    )
    .map_err(|e| MerkurError::Storage(format!("Failed to insert context tag: {e}")))?;
    Ok(())
}

/// Search memory IDs by context tag filters. Each key/value pair is bound with
/// placeholders; the WHERE clause shape is derived from the number of pairs.
pub fn search_by_context(
    pool: &Pool<SqliteConnectionManager>,
    filters: &HashMap<String, String>,
) -> MerkurResult<Vec<String>> {
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    let conditions: Vec<String> = filters
        .keys()
        .enumerate()
        .map(|(i, _)| format!("(key = ?{} AND value = ?{})", i * 2 + 1, i * 2 + 2))
        .collect();
    let sql = format!(
        "SELECT DISTINCT memory_id FROM context_tags WHERE {}",
        conditions.join(" OR ")
    );
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare context query: {e}")))?;

    let params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = filters
        .iter()
        .flat_map(|(k, v)| {
            vec![
                Box::new(k.clone()) as Box<dyn rusqlite::types::ToSql>,
                Box::new(v.clone()) as Box<dyn rusqlite::types::ToSql>,
            ]
        })
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|p| p.as_ref()).collect();

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
        .map_err(|e| MerkurError::Storage(format!("Context search failed: {e}")))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?);
    }
    Ok(results)
}

/// List pending consolidation memory IDs.
pub fn list_pending_ids(
    pool: &Pool<SqliteConnectionManager>,
    limit: usize,
) -> MerkurResult<Vec<String>> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare("SELECT id FROM memories WHERE pending_consolidation = 1 LIMIT ?1")
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare pending query: {e}")))?;
    Ok(stmt
        .query_map(params![limit as i64], |row| row.get::<_, String>(0))
        .map_err(|e| MerkurError::Storage(format!("Pending query failed: {e}")))?
        .filter_map(|r| r.ok())
        .collect())
}

/// List forgetting candidate memory IDs (oldest accessed first, non-archived).
pub fn list_forgetting_ids(
    pool: &Pool<SqliteConnectionManager>,
    limit: usize,
) -> MerkurResult<Vec<String>> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare("SELECT id FROM memories WHERE level >= 0 ORDER BY accessed_at ASC LIMIT ?1")
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare forgetting query: {e}")))?;
    Ok(stmt
        .query_map(params![limit as i64], |row| row.get::<_, String>(0))
        .map_err(|e| MerkurError::Storage(format!("Forgetting query failed: {e}")))?
        .filter_map(|r| r.ok())
        .collect())
}

/// Mark memories as consolidated, chunking to stay within SQLite's variable limit.
pub fn mark_consolidated(pool: &Pool<SqliteConnectionManager>, ids: &[String]) -> MerkurResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // Stay well below SQLite's default SQLITE_MAX_VARIABLE_NUMBER (999 on older
    // builds, 32766 on 3.32+). 500 is a safe, conservative chunk size.
    const CHUNK: usize = 500;
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    for chunk in ids.chunks(CHUNK) {
        let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "UPDATE memories SET pending_consolidation = 0 WHERE id IN ({})",
            placeholders.join(",")
        );
        let params_ref: Vec<&dyn rusqlite::types::ToSql> = chunk
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        conn.execute(&sql, params_ref.as_slice())
            .map_err(|e| MerkurError::Storage(format!("Failed to mark consolidated: {e}")))?;
    }
    Ok(())
}

/// Update memory level.
pub fn update_level(
    pool: &Pool<SqliteConnectionManager>,
    id: &str,
    level: i32,
) -> MerkurResult<()> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    conn.execute(
        "UPDATE memories SET level = ?1, updated_at = ?2 WHERE id = ?3",
        params![level, Utc::now().to_rfc3339(), id],
    )
    .map_err(|e| MerkurError::Storage(format!("Failed to update level: {e}")))?;
    Ok(())
}

/// Insert a consolidation log entry.
pub fn log_consolidation(
    pool: &Pool<SqliteConnectionManager>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    report: &ConsolidationReport,
) -> MerkurResult<()> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    conn.execute(
        "INSERT INTO consolidate_log (started_at, finished_at, memories_processed, edges_created, errors)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            started_at.to_rfc3339(),
            finished_at.to_rfc3339(),
            report.memories_processed as i64,
            report.edges_created as i64,
            report.errors as i64,
        ],
    )
    .map_err(|e| MerkurError::Storage(format!("Failed to log consolidation: {e}")))?;
    Ok(())
}

/// Get consolidation log entries.
pub fn get_consolidation_log(
    pool: &Pool<SqliteConnectionManager>,
    limit: usize,
) -> MerkurResult<Vec<ConsolidationLogEntry>> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, started_at, finished_at, memories_processed, edges_created, errors
             FROM consolidate_log ORDER BY id DESC LIMIT ?1",
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare log query: {e}")))?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            let started_at: String = row.get(1)?;
            let finished_at: Option<String> = row.get(2)?;
            Ok(ConsolidationLogEntry {
                id: row.get(0)?,
                started_at: parse_rfc3339(&started_at),
                finished_at: finished_at.as_deref().map(parse_rfc3339),
                memories_processed: row.get(3)?,
                edges_created: row.get(4)?,
                errors: row.get(5)?,
            })
        })
        .map_err(|e| MerkurError::Storage(format!("Log query failed: {e}")))?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?);
    }
    Ok(entries)
}

/// Get storage statistics.
pub fn stats(pool: &Pool<SqliteConnectionManager>) -> MerkurResult<StorageStats> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;

    let total_memories: usize = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
        .map_err(|e| MerkurError::Storage(format!("Stats query failed: {e}")))?;

    let total_edges: usize = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .map_err(|e| MerkurError::Storage(format!("Stats query failed: {e}")))?;

    let pending_consolidation: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE pending_consolidation = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| MerkurError::Storage(format!("Stats query failed: {e}")))?;

    let mut by_level = HashMap::new();
    let mut stmt = conn
        .prepare("SELECT level, COUNT(*) FROM memories GROUP BY level")
        .map_err(|e| MerkurError::Storage(format!("Stats query failed: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, usize>(1)?))
        })
        .map_err(|e| MerkurError::Storage(format!("Stats query failed: {e}")))?;
    for row in rows {
        let (level, count) = row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
        by_level.insert(level, count);
    }

    Ok(StorageStats {
        total_memories,
        total_edges,
        pending_consolidation,
        by_level,
    })
}

/// Get context tags for a memory.
pub fn get_context_tags(
    pool: &Pool<SqliteConnectionManager>,
    memory_id: &str,
) -> MerkurResult<HashMap<String, String>> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare("SELECT key, value FROM context_tags WHERE memory_id = ?1")
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare context query: {e}")))?;
    let rows = stmt
        .query_map(params![memory_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| MerkurError::Storage(format!("Context query failed: {e}")))?;

    let mut context = HashMap::new();
    for row in rows {
        let (key, value) = row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
        context.insert(key, value);
    }
    Ok(context)
}

/// Get edges for a memory (both as source and target).
pub fn get_edges(pool: &Pool<SqliteConnectionManager>, memory_id: &str) -> MerkurResult<Vec<Edge>> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, source_id, target_id, weight, relation, edge_type
             FROM edges WHERE source_id = ?1 OR target_id = ?1",
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare edges query: {e}")))?;
    let rows = stmt
        .query_map(params![memory_id], |row| {
            let edge_type_str: String = row.get(5)?;
            Ok(Edge {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                weight: row.get(3)?,
                relation: row.get(4)?,
                edge_type: EdgeType::from_db_str(&edge_type_str),
            })
        })
        .map_err(|e| MerkurError::Storage(format!("Edges query failed: {e}")))?;

    let mut edges = Vec::new();
    for row in rows {
        edges.push(row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?);
    }
    Ok(edges)
}

/// Batch-fetch edges for a set of memory IDs, returning (memory_id, edge) pairs
/// so the caller can group them. Uses a single query with `IN`.
pub fn get_edges_batch(
    pool: &Pool<SqliteConnectionManager>,
    memory_ids: &[String],
) -> MerkurResult<HashMap<String, Vec<Edge>>> {
    if memory_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let ids_json = serde_json::to_string(memory_ids)
        .map_err(|e| MerkurError::Storage(format!("Failed to encode ids: {e}")))?;
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, source_id, target_id, weight, relation, edge_type
             FROM edges
             WHERE source_id IN (SELECT value FROM json_each(?1))
                OR target_id IN (SELECT value FROM json_each(?1))",
        )
        .map_err(|e| MerkurError::Storage(format!("Failed to prepare edges query: {e}")))?;
    let rows = stmt
        .query_map(params![ids_json], |row| {
            let edge_type_str: String = row.get(5)?;
            Ok(Edge {
                id: row.get(0)?,
                source_id: row.get(1)?,
                target_id: row.get(2)?,
                weight: row.get(3)?,
                relation: row.get(4)?,
                edge_type: EdgeType::from_db_str(&edge_type_str),
            })
        })
        .map_err(|e| MerkurError::Storage(format!("Edges query failed: {e}")))?;

    let mut by_mem: HashMap<String, Vec<Edge>> = HashMap::new();
    let ids_set: std::collections::HashSet<&str> = memory_ids.iter().map(String::as_str).collect();
    for row in rows {
        let edge = row.map_err(|e| MerkurError::Storage(format!("Row error: {e}")))?;
        for mid in &[edge.source_id.as_str(), edge.target_id.as_str()] {
            if ids_set.contains(*mid) {
                by_mem
                    .entry((*mid).to_string())
                    .or_default()
                    .push(edge.clone());
            }
        }
    }
    Ok(by_mem)
}

/// Update access tracking for memories. Errors are logged and swallowed because
/// access bookkeeping must never fail a read, but at least they're observable.
pub fn update_access(pool: &Pool<SqliteConnectionManager>, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to get connection for update_access: {e}");
            return;
        }
    };
    let now = Utc::now().to_rfc3339();
    const CHUNK: usize = 500;
    for chunk in ids.chunks(CHUNK) {
        let placeholders: Vec<String> = (2..=(chunk.len() + 1)).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "UPDATE memories SET accessed_at = ?1, access_count = access_count + 1
             WHERE id IN ({})",
            placeholders.join(",")
        );
        let mut all_params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(chunk.len() + 1);
        all_params.push(&now);
        for id in chunk {
            all_params.push(id as &dyn rusqlite::types::ToSql);
        }
        if let Err(e) = conn.execute(&sql, all_params.as_slice()) {
            warn!("update_access failed: {e}");
            return;
        }
    }
}

/// Confirm a memory exists, returning `MemoryNotFound` otherwise. Useful for
/// validating foreign-key-like preconditions at the application level on top of
/// whatever the schema enforces.
pub fn memory_exists(pool: &Pool<SqliteConnectionManager>, id: &str) -> MerkurResult<bool> {
    let conn = pool
        .get()
        .map_err(|e| MerkurError::Storage(format!("Failed to get connection: {e}")))?;
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(1) FROM memories WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(|e| MerkurError::Storage(format!("memory_exists failed: {e}")))?;
    Ok(count > 0)
}
