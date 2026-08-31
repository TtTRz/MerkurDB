use merkur_core::{EdgeType, MemoryLevel, MerkurResult, NewEdge, NewMemory, Storage};
use merkur_storage::SqliteStorage;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("file:test_{id}?mode=memory&cache=shared")
}

fn new_test_storage(dim: usize) -> MerkurResult<SqliteStorage> {
    SqliteStorage::new(&temp_db_path(), dim)
}

fn new_test_memory(content: &str, embedding: Option<Vec<f32>>) -> NewMemory {
    NewMemory {
        content: content.to_string(),
        category: Some("general".to_string()),
        context: HashMap::from([("agent".to_string(), "test".to_string())]),
        metadata: HashMap::new(),
        embedding,
        namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
    }
}

#[tokio::test]
async fn test_insert_and_get() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory(
            "v8 GC is generational",
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        ))
        .await?;

    let mem = storage.get_memory(&id).await?.unwrap();
    assert_eq!(mem.content, "v8 GC is generational");
    assert_eq!(mem.level, MemoryLevel::Full);
    assert!(mem.pending_consolidation);
    Ok(())
}

#[tokio::test]
async fn test_vector_store_and_search() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;

    let id1 = storage
        .insert_memory(&new_test_memory("v8 GC", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let _id2 = storage
        .insert_memory(&new_test_memory(
            "Rust async",
            Some(vec![-1.0, 0.0, 0.0, 0.0]),
        ))
        .await?;

    let results = storage.vector_search(&[1.0, 0.0, 0.0, 0.0], 5).await?;
    assert!(!results.is_empty());
    assert_eq!(results[0].id, id1);
    if results.len() > 1 {
        assert!(results[0].score > results[1].score);
    }
    Ok(())
}

#[tokio::test]
async fn test_edge_and_bfs() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;

    let a = storage
        .insert_memory(&new_test_memory("A", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let b = storage
        .insert_memory(&new_test_memory("B", Some(vec![0.0, 1.0, 0.0, 0.0])))
        .await?;
    let c = storage
        .insert_memory(&new_test_memory("C", Some(vec![0.0, 0.0, 1.0, 0.0])))
        .await?;

    storage
        .insert_edge(&NewEdge {
            source_id: a.clone(),
            target_id: b.clone(),
            weight: Some(1.0),
            relation: None,
            edge_type: EdgeType::Auto,
        })
        .await?;
    storage
        .insert_edge(&NewEdge {
            source_id: b.clone(),
            target_id: c.clone(),
            weight: Some(0.5),
            relation: None,
            edge_type: EdgeType::Auto,
        })
        .await?;

    let expanded = storage.bfs_expand(std::slice::from_ref(&a), 2, 20).await?;
    let ids: Vec<_> = expanded.iter().map(|m| m.id.clone()).collect();
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));
    Ok(())
}

#[tokio::test]
async fn test_delete_cascades_edges_and_context() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;

    let a = storage
        .insert_memory(&new_test_memory("A", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let b = storage
        .insert_memory(&new_test_memory("B", Some(vec![0.0, 1.0, 0.0, 0.0])))
        .await?;
    storage.insert_context_tag(&a, "ns", "team").await?;
    storage
        .insert_edge(&NewEdge {
            source_id: a.clone(),
            target_id: b.clone(),
            weight: None,
            relation: None,
            edge_type: EdgeType::Manual,
        })
        .await?;

    storage.delete_memory(&a).await?;
    assert!(storage.get_memory(&a).await?.is_none());

    // Edges referencing the deleted memory must have been removed by FK CASCADE.
    let remaining = storage.get_edges(&b).await?;
    assert!(
        remaining
            .iter()
            .all(|e| e.source_id != a && e.target_id != a),
        "edges referencing deleted memory still present: {remaining:?}"
    );
    Ok(())
}

#[tokio::test]
async fn test_insert_edge_to_unknown_memory_fails() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let a = storage
        .insert_memory(&new_test_memory("A", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;

    // With foreign_keys=ON enforced on every connection, an edge pointing at a
    // non-existent target must be rejected by the engine.
    let res = storage
        .insert_edge(&NewEdge {
            source_id: a,
            target_id: "mem_does_not_exist".into(),
            weight: None,
            relation: None,
            edge_type: EdgeType::Manual,
        })
        .await;
    assert!(res.is_err(), "expected FK violation, got {res:?}");
    Ok(())
}

#[tokio::test]
async fn test_get_nonexistent() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let result = storage.get_memory("nonexistent").await?;
    assert!(result.is_none());
    Ok(())
}

#[tokio::test]
async fn test_stats() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;

    storage
        .insert_memory(&new_test_memory("test1", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    storage
        .insert_memory(&new_test_memory("test2", Some(vec![0.0, 1.0, 0.0, 0.0])))
        .await?;

    let stats = storage.stats().await?;
    assert_eq!(stats.total_memories, 2);
    assert_eq!(stats.pending_consolidation, 2);
    Ok(())
}

#[tokio::test]
async fn test_memory_exists() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory("hello", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    assert!(storage.memory_exists(&id).await?);
    assert!(!storage.memory_exists("mem_zzz").await?);
    Ok(())
}

#[tokio::test]
async fn test_memory_exists_batch() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id1 = storage
        .insert_memory(&new_test_memory("A", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let id2 = storage
        .insert_memory(&new_test_memory("B", Some(vec![0.0, 1.0, 0.0, 0.0])))
        .await?;

    let candidates = vec![id1.clone(), id2.clone(), "mem_nonexistent".to_string()];
    let existing = storage.memory_exists_batch(&candidates).await?;
    assert!(existing.contains(&id1));
    assert!(existing.contains(&id2));
    assert!(!existing.contains("mem_nonexistent"));
    assert_eq!(existing.len(), 2);
    Ok(())
}

#[tokio::test]
async fn test_get_edges_batch() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let a = storage
        .insert_memory(&new_test_memory("A", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    let b = storage
        .insert_memory(&new_test_memory("B", Some(vec![0.0, 1.0, 0.0, 0.0])))
        .await?;
    let c = storage
        .insert_memory(&new_test_memory("C", Some(vec![0.0, 0.0, 1.0, 0.0])))
        .await?;

    storage
        .insert_edge(&NewEdge {
            source_id: a.clone(),
            target_id: b.clone(),
            weight: Some(1.0),
            relation: None,
            edge_type: EdgeType::Auto,
        })
        .await?;
    storage
        .insert_edge(&NewEdge {
            source_id: b.clone(),
            target_id: c.clone(),
            weight: Some(0.5),
            relation: None,
            edge_type: EdgeType::Auto,
        })
        .await?;

    let batch = storage
        .get_edges_batch(&[a.clone(), b.clone(), c.clone()])
        .await?;
    // 'a' has outgoing edge to b
    assert!(!batch.get(&a).unwrap_or(&vec![]).is_empty());
    // 'b' has edges in both directions
    assert!(!batch.get(&b).unwrap_or(&vec![]).is_empty());
    Ok(())
}

#[tokio::test]
async fn test_update_abstract() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory(
            "deep content",
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        ))
        .await?;

    storage.update_abstract(&id, "summarized").await?;

    let mem = storage.get_memory(&id).await?.unwrap();
    assert_eq!(mem.abstract_.as_deref(), Some("summarized"));
    Ok(())
}

#[tokio::test]
async fn test_get_memory_no_embedding() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory(
            "test embedding exclusion",
            Some(vec![1.0, 2.0, 3.0, 4.0]),
        ))
        .await?;

    // get_memory should NOT return the embedding blob (HV7 optimization).
    let mem = storage.get_memory(&id).await?.unwrap();
    assert!(
        mem.embedding.is_none(),
        "get_memory should not return embedding"
    );
    Ok(())
}

#[tokio::test]
async fn test_norms_consistent_after_upsert_remove() -> MerkurResult<()> {
    use merkur_storage::InMemoryVectorIndex;

    let idx = InMemoryVectorIndex::new(3);
    idx.add("a".into(), vec![3.0, 4.0, 0.0]); // norm = 5
    idx.add("b".into(), vec![0.0, 0.0, 1.0]); // norm = 1
    idx.add("c".into(), vec![1.0, 1.0, 1.0]); // norm ≈ 1.732

    // Remove 'a' (swap-removes with 'c'). After removal, search must still
    // return correct cosine scores for 'b' and 'c'.
    idx.remove("a");
    assert_eq!(idx.len(), 2);

    let results = idx.search(&[0.0, 0.0, 1.0], 2);
    // 'b' should rank first (perfectly aligned)
    assert_eq!(results[0].0, "b");
    assert!((results[0].1 - 1.0).abs() < 1e-9);
    Ok(())
}

#[tokio::test]
async fn test_update_memory_nonexistent_returns_error() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let result = storage
        .update_memory(
            "mem_does_not_exist",
            "new content",
            Some(&[1.0, 0.0, 0.0, 0.0]),
        )
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, merkur_core::MerkurError::MemoryNotFound(_)),
        "expected MemoryNotFound, got {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Hybrid search: BM25 (FTS5 trigram) channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fts_tracks_insert_update_delete() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory(
            "postgres vacuum tuning guide",
            None,
        ))
        .await?;

    let hits = storage.text_search("vacuum tuning", merkur_core::DEFAULT_NAMESPACE, 5).await?;
    assert_eq!(hits.len(), 1, "inserted row must be BM25-searchable");
    assert_eq!(hits[0].0, id);

    // Rewriting content must reindex: old term gone, new term found.
    storage
        .update_memory(&id, "postgres replication setup", None)
        .await?;
    assert!(
        storage.text_search("vacuum", merkur_core::DEFAULT_NAMESPACE, 5).await?.is_empty(),
        "old term must leave the index after content update"
    );
    let hits = storage.text_search("replication setup", merkur_core::DEFAULT_NAMESPACE, 5).await?;
    assert_eq!(hits.len(), 1);

    // Deleting the memory removes it from the index too.
    storage.delete_memory(&id).await?;
    assert!(storage.text_search("replication", merkur_core::DEFAULT_NAMESPACE, 5).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_text_search_cjk_substring() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    storage
        .insert_memory(&new_test_memory(
            "用户喜欢用 Rust 写数据库内核",
            None,
        ))
        .await?;
    let hits = storage.text_search("喜欢用", merkur_core::DEFAULT_NAMESPACE, 5).await?;
    assert_eq!(hits.len(), 1, "trigram tokenizer must match CJK substrings");
    Ok(())
}

#[tokio::test]
async fn test_text_search_excludes_archived() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory("kubernetes node affinity rules", None))
        .await?;
    storage.update_level(&id, -1).await?;
    assert!(
        storage.text_search("affinity", merkur_core::DEFAULT_NAMESPACE, 5).await?.is_empty(),
        "archived memories must not surface via the BM25 channel"
    );
    Ok(())
}

#[tokio::test]
async fn test_text_search_short_query_yields_empty() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    storage
        .insert_memory(&new_test_memory(
            "ab cd ef something long enough",
            None,
        ))
        .await?;
    // Fewer than three characters: trigram cannot index any term, so the
    // channel short-circuits to empty (vector channel covers these queries).
    assert!(storage.text_search("ab", merkur_core::DEFAULT_NAMESPACE, 5).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn test_text_search_ranks_by_term_frequency() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let a = storage
        .insert_memory(&new_test_memory(
            "rust rust rust rust compiler internals deep dive",
            None,
        ))
        .await?;
    let b = storage
        .insert_memory(&new_test_memory(
            "a gentle intro to the rust programming language",
            None,
        ))
        .await?;
    let _c = storage
        .insert_memory(&new_test_memory("completely unrelated garbage content here", None))
        .await?;

    let hits = storage.text_search("rust", merkur_core::DEFAULT_NAMESPACE, 5).await?;
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert!(!ids.contains(&_c.as_str()), "non-matching doc must be absent");
    let ai = ids.iter().position(|i| *i == a.as_str()).unwrap();
    let bi = ids.iter().position(|i| *i == b.as_str()).unwrap();
    assert!(
        ai < bi,
        "higher term frequency should rank above incidental mention"
    );
    Ok(())
}

#[tokio::test]
async fn test_migration_backfills_preexisting_rows() -> MerkurResult<()> {

    // Build a hand-rolled "v1" database: pre-migration schema with rows and
    // the old version marker, no memories_fts, no triggers.
    //
    // Shared-cache in-memory databases die with their last connection, so a
    // long-lived "keeper" connection holds the database open across the
    // SqliteStorage::new upgrade below.
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("file:v1_{n}?mode=memory&cache=shared");
    let keeper = rusqlite::Connection::open(&path).unwrap();
    keeper
        .execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, abstract TEXT DEFAULT '',
                category TEXT DEFAULT 'general', weight REAL NOT NULL DEFAULT 1.0,
                level INTEGER NOT NULL DEFAULT 2, pending_consolidation INTEGER NOT NULL DEFAULT 1,
                embedding BLOB, metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, accessed_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE merkur_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO merkur_meta VALUES ('schema_version', '1');
            INSERT INTO memories (id, content, created_at, updated_at, accessed_at)
              VALUES ('mem_old1', 'redis cluster resharding notes', '2026-01-01T00:00:00+00:00',
                      '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();

    let storage = SqliteStorage::new(&path, 4)?;

    // Pre-existing row must be searchable through the BM25 channel.
    let hits = storage.text_search("resharding", merkur_core::DEFAULT_NAMESPACE, 5).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "mem_old1");

    // Triggers installed by v2 keep syncing from now on.
    storage
        .insert_memory(&new_test_memory("postgres replication setup", None))
        .await?;
    assert_eq!(storage.text_search("replication", merkur_core::DEFAULT_NAMESPACE, 5).await?.len(), 1);
    Ok(())
}


// ---------------------------------------------------------------------------
// Namespace isolation (P0-3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_namespace_defaults_to_default_bucket() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory("unspecified namespace memory", None))
        .await?;
    let m = storage.get_memory(&id).await?.unwrap();
    assert_eq!(m.namespace, "default");
    Ok(())
}

#[tokio::test]
async fn test_namespace_isolated_text_search() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    storage
        .insert_memory(&NewMemory {
            content: "shared vocabulary postgres indexing".into(),
            category: None,
            context: Default::default(),
            metadata: Default::default(),
            embedding: None,
            namespace: "alpha".into(),
        })
        .await?;
    storage
        .insert_memory(&NewMemory {
            content: "shared vocabulary postgres indexing".into(),
            category: None,
            context: Default::default(),
            metadata: Default::default(),
            embedding: None,
            namespace: "beta".into(),
        })
        .await?;

    let hits_alpha = storage.text_search("shared vocabulary", "alpha", 10).await?;
    assert_eq!(hits_alpha.len(), 1);
    let hits_beta = storage.text_search("shared vocabulary", "beta", 10).await?;
    assert_eq!(hits_beta.len(), 1);
    assert_ne!(hits_alpha[0].0, hits_beta[0].0, "same content in two buckets must stay separate");
    Ok(())
}

#[tokio::test]
async fn test_namespace_isolated_vector_search() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let _a = storage
        .insert_memory(&NewMemory {
            content: "alpha bucket memory".into(),
            category: None,
            context: Default::default(),
            metadata: Default::default(),
            embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            namespace: "alpha".into(),
        })
        .await?;
    let _b = storage
        .insert_memory(&NewMemory {
            content: "beta bucket memory".into(),
            category: None,
            context: Default::default(),
            metadata: Default::default(),
            embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
            namespace: "beta".into(),
        })
        .await?;

    let hits = storage.vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "alpha", 10).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, _a);
    Ok(())
}

#[tokio::test]
async fn test_namespace_isolated_bfs() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    // Two chains with identical topology, one per bucket. Edges are
    // cross-bucket on purpose: traversal must not leak into the other bucket.
    let mut a_ids = Vec::new();
    let mut b_ids = Vec::new();
    for i in 0..3 {
        a_ids.push(
            storage
                .insert_memory(&NewMemory {
                    content: format!("alpha chain node {i}"),
                    category: None,
                    context: Default::default(),
                    metadata: Default::default(),
                    embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
                    namespace: "alpha".into(),
                })
                .await?,
        );
        b_ids.push(
            storage
                .insert_memory(&NewMemory {
                    content: format!("beta chain node {i}"),
                    category: None,
                    context: Default::default(),
                    metadata: Default::default(),
                    embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
                    namespace: "beta".into(),
                })
                .await?,
        );
    }
    storage
        .insert_edge(&NewEdge {
            source_id: a_ids[0].clone(),
            target_id: b_ids[0].clone(),
            weight: Some(1.0),
            relation: None,
            edge_type: EdgeType::Auto,
        })
        .await?;

    let expanded = storage
        .bfs_expand_ns(&[a_ids[0].clone()], "alpha", 2, 20)
        .await?;
    assert!(
        expanded.iter().all(|m| m.namespace == "alpha"),
        "BFS leaked into beta: {expanded:?}"
    );
    assert!(
        expanded.iter().all(|m| m.id != b_ids[0]),
        "cross-bucket edge endpoint must not appear in alpha results"
    );
    Ok(())
}

#[tokio::test]
async fn test_namespace_migration_preserves_rows() -> MerkurResult<()> {
    // v2-era database: rows exist before the namespace column lands.
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("file:v2_{n}?mode=memory&cache=shared");
    let keeper = rusqlite::Connection::open(&path).unwrap();
    keeper
        .execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, abstract TEXT DEFAULT '',
                category TEXT DEFAULT 'general', weight REAL NOT NULL DEFAULT 1.0,
                level INTEGER NOT NULL DEFAULT 2, pending_consolidation INTEGER NOT NULL DEFAULT 1,
                embedding BLOB, metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, accessed_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE merkur_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO merkur_meta VALUES ('schema_version', '2');
            INSERT INTO memories (id, content, created_at, updated_at, accessed_at)
              VALUES ('mem_v2_1', 'pre-migration row about vacuum', '2026-01-01T00:00:00+00:00',
                      '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();

    let storage = SqliteStorage::new(&path, 4)?;
    let m = storage.get_memory("mem_v2_1").await?.unwrap();
    assert_eq!(m.namespace, "default", "v2 rows must be backfilled into the default bucket");
    Ok(())
}

// ---------------------------------------------------------------------------
// Importance (P1-5): system-learned salience, Consolidator-written
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_importance_defaults_to_neutral_prior() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory("unconsolidated memory", None))
        .await?;
    let m = storage.get_memory(&id).await?.unwrap();
    assert!(
        (m.importance - 0.5).abs() < 1e-9,
        "unconsolidated memories carry the neutral 0.5 prior, got {}",
        m.importance
    );
    Ok(())
}

#[tokio::test]
async fn test_update_importance_persists() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id = storage
        .insert_memory(&new_test_memory("important memory", None))
        .await?;
    storage.update_importance(&id, 0.92).await?;
    let m = storage.get_memory(&id).await?.unwrap();
    assert!((m.importance - 0.92).abs() < 1e-9);
    Ok(())
}

#[tokio::test]
async fn test_importance_migration_backfills_neutral() -> MerkurResult<()> {
    // v3-era database (has namespace, no importance).
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = format!("file:v3_{n}?mode=memory&cache=shared");
    let keeper = rusqlite::Connection::open(&path).unwrap();
    keeper
        .execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY, content TEXT NOT NULL, abstract TEXT DEFAULT '',
                category TEXT DEFAULT 'general', weight REAL NOT NULL DEFAULT 1.0,
                level INTEGER NOT NULL DEFAULT 2, pending_consolidation INTEGER NOT NULL DEFAULT 1,
                embedding BLOB, metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, accessed_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                namespace TEXT NOT NULL DEFAULT 'default'
            );
            CREATE TABLE merkur_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO merkur_meta VALUES ('schema_version', '3');
            INSERT INTO memories (id, content, created_at, updated_at, accessed_at)
              VALUES ('mem_v3_1', 'pre-importance row', '2026-01-01T00:00:00+00:00',
                      '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();

    let storage = SqliteStorage::new(&path, 4)?;
    let m = storage.get_memory("mem_v3_1").await?.unwrap();
    assert!(
        (m.importance - 0.5).abs() < 1e-9,
        "v3 rows must backfill to the neutral prior"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Write-time dedup (P2-8): top-1 similarity NOOP short-circuit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dedup_noop_returns_existing_id_for_near_duplicate() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    // Identical embedding => cosine 1.0 > threshold.
    let id1 = storage
        .insert_memory_dedup(
            &new_test_memory("write dedup probe", Some(vec![1.0, 0.0, 0.0, 0.0])),
            0.92,
        )
        .await?;
    let id2 = storage
        .insert_memory_dedup(
            &new_test_memory("write dedup probe", Some(vec![1.0, 0.0, 0.0, 0.0])),
            0.92,
        )
        .await?;
    assert_eq!(id1, id2, "near-duplicate must NOOP onto the existing row");
    Ok(())
}

#[tokio::test]
async fn test_dedup_inserts_when_below_threshold() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    let id1 = storage
        .insert_memory_dedup(
            &new_test_memory("alpha vector", Some(vec![1.0, 0.0, 0.0, 0.0])),
            0.92,
        )
        .await?;
    let id2 = storage
        .insert_memory_dedup(
            &new_test_memory("orthogonal vector", Some(vec![0.0, 1.0, 0.0, 0.0])),
            0.92,
        )
        .await?;
    assert_ne!(id1, id2, "dissimilar content must insert normally");
    Ok(())
}

#[tokio::test]
async fn test_dedup_scopes_to_same_namespace() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;
    // Same content + embedding in two buckets: dedup must NOT cross buckets.
    let mut a = new_test_memory("cross bucket dup", Some(vec![1.0, 0.0, 0.0, 0.0]));
    a.namespace = "alpha".into();
    let mut b = new_test_memory("cross bucket dup", Some(vec![1.0, 0.0, 0.0, 0.0]));
    b.namespace = "beta".into();
    let ida = storage.insert_memory_dedup(&a, 0.92).await?;
    let idb = storage.insert_memory_dedup(&b, 0.92).await?;
    assert_ne!(ida, idb, "dedup must not leak across namespaces");
    Ok(())
}