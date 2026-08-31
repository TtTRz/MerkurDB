#![cfg(feature = "lancedb")]

use merkur_core::{MerkurResult, NewMemory, Storage};
use merkur_storage::LanceDbStorage;
use std::collections::HashMap;

fn test_paths(tag: &str) -> (String, String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("merkur_lance_{tag}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let lance = dir.join("lance").to_string_lossy().to_string();
    let sqlite = dir.join("meta.db").to_string_lossy().to_string();
    (lance, sqlite, dir)
}

fn mem(content: &str, namespace: &str) -> NewMemory {
    NewMemory {
        content: content.to_string(),
        category: Some("general".to_string()),
        context: HashMap::new(),
        metadata: HashMap::new(),
        embedding: None,
        namespace: namespace.to_string(),
    }
}

#[tokio::test]
async fn test_lancedb_backend_runs_migrations() -> MerkurResult<()> {
    let (lance, sqlite, dir) = test_paths("migrate");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;

    // v3 column: a namespaced insert + read-back must work on a fresh DB.
    let id = storage
        .insert_memory(&mem("redis cluster resharding notes", "alpha"))
        .await?;
    let m = storage.get_memory(&id).await?.unwrap();
    assert_eq!(m.namespace, "alpha");

    // v2 object: the BM25 channel needs memories_fts + triggers.
    let hits = storage.text_search("resharding", "alpha", 5).await?;
    assert_eq!(hits.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// Same starvation contract as the SQLite backend: candidates are scored
/// globally in the vector table and bucket-filtered in SQLite, so the probe
/// must deepen past foreign-bucket hits instead of stopping at a fixed
/// oversample.
#[tokio::test]
async fn test_lancedb_vector_search_deepens_past_foreign_bucket_hits() -> MerkurResult<()> {
    let (lance, sqlite, dir) = test_paths("deepen");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;

    for i in 0..5 {
        let mut m = mem(&format!("foreign filler {i}"), "default");
        m.embedding = Some(vec![1.0, 0.001 * (i as f32 + 1.0), 0.0, 0.0]);
        storage.insert_memory(&m).await?;
    }
    let mut needle = mem("alpha needle", "alpha");
    needle.embedding = Some(vec![0.5, 0.5, 0.5, 0.5]);
    let needle_id = storage.insert_memory(&needle).await?;

    let hits = storage
        .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "alpha", 1)
        .await?;
    assert_eq!(
        hits.len(),
        1,
        "in-bucket needle must be found despite foreign hits outranking it"
    );
    assert_eq!(hits[0].id, needle_id);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}

/// Retrieval purity contract, LanceDB side: vector search never records
/// access; `record_access` is the explicit serving-point signal.
#[tokio::test]
async fn test_lancedb_retrieval_purity_and_record_access() -> MerkurResult<()> {
    let (lance, sqlite, dir) = test_paths("access");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let mut m = mem("demand signal target", "default");
    m.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let id = storage.insert_memory(&m).await?;

    let hits = storage
        .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "default", 5)
        .await?;
    assert!(hits.iter().any(|h| h.id == id));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        storage.get_memory(&id).await?.unwrap().access_count,
        0,
        "retrieval must not record access; serving points do"
    );

    storage.record_access(std::slice::from_ref(&id)).await?;
    assert_eq!(storage.get_memory(&id).await?.unwrap().access_count, 1);

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
