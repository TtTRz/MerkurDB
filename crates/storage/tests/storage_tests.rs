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
    let id2 = storage
        .insert_memory(&new_test_memory(
            "Rust async",
            Some(vec![-1.0, 0.0, 0.0, 0.0]),
        ))
        .await?;

    let results = storage.vector_search(&[1.0, 0.0, 0.0, 0.0], 5).await?;
    assert!(!results.is_empty());
    assert!(results[0].id == id1);
    assert!(results[0].score > results[1].score);
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

    let expanded = storage.bfs_expand(&[a.clone()], 2, 20).await?;
    let ids: Vec<_> = expanded.iter().map(|m| m.id.clone()).collect();
    assert!(ids.contains(&b));
    assert!(ids.contains(&c));
    Ok(())
}

#[tokio::test]
async fn test_delete_cascades() -> MerkurResult<()> {
    let storage = new_test_storage(4)?;

    let id = storage
        .insert_memory(&new_test_memory("test", Some(vec![1.0, 0.0, 0.0, 0.0])))
        .await?;
    storage.insert_context_tag(&id, "agent", "test").await?;
    storage.delete_memory(&id).await?;
    assert!(storage.get_memory(&id).await?.is_none());
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
