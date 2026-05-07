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
