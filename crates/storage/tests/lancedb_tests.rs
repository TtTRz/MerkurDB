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
async fn cosine_scores_and_scaling_match_sqlite() -> MerkurResult<()> {
    let (lance, sqlite, dir) = test_paths("cosine");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let reference =
        merkur_storage::SqliteStorage::new(dir.join("reference.db").to_str().unwrap(), 4)?;
    let vectors = [
        ("aligned large", vec![10.0, 0.0, 0.0, 0.0]),
        (
            "angled",
            vec![0.85, (1.0_f32 - 0.85 * 0.85).sqrt(), 0.0, 0.0],
        ),
        ("orthogonal", vec![0.0, 1.0, 0.0, 0.0]),
        ("zero", vec![0.0; 4]),
        ("opposite", vec![-1.0, 0.0, 0.0, 0.0]),
    ];
    for backend in [&storage as &dyn Storage, &reference as &dyn Storage] {
        for (content, vector) in &vectors {
            let mut memory = mem(content, "alpha");
            memory.embedding = Some(vector.clone());
            backend.insert_memory(&memory).await?;
        }
    }
    for scale in [1.0, 7.0] {
        let query = [scale, 0.0, 0.0, 0.0];
        let expected = reference.vector_search_ns(&query, "alpha", 10).await?;
        let actual = storage.vector_search_ns(&query, "alpha", 10).await?;
        assert_eq!(actual.len(), expected.len());
        assert_eq!(
            actual[0].content, "aligned large",
            "cosine ranking must be magnitude invariant"
        );
        for expected in &expected {
            let actual = actual
                .iter()
                .find(|m| m.content == expected.content)
                .unwrap();
            assert!(
                actual.score.is_finite() && (actual.score - expected.score).abs() < 1e-5,
                "{}: Lance {}, SQLite {} at query scale {scale}",
                expected.content,
                actual.score,
                expected.score
            );
        }
    }
    let zero_hits = storage.vector_search_ns(&[0.0; 4], "alpha", 10).await?;
    assert_eq!(zero_hits.len(), vectors.len());
    assert!(
        zero_hits.iter().all(|m| m.score == 0.0),
        "zero-query cosine must be finite zero"
    );
    for (content, vector) in [
        ("zero in negative bucket", vec![0.0; 4]),
        ("negative", vec![-1.0, 0.0, 0.0, 0.0]),
    ] {
        let mut memory = mem(content, "negative");
        memory.embedding = Some(vector);
        storage.insert_memory(&memory).await?;
    }
    let hits = storage
        .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "negative", 1)
        .await?;
    assert_eq!(
        hits[0].content, "zero in negative bucket",
        "zero must outrank negative cosine even when native KNN omits it"
    );
    assert_eq!(hits[0].score, 0.0);
    drop(reference);
    drop(storage);
    std::fs::remove_dir_all(dir).ok();
    Ok(())
}

#[tokio::test]
async fn cosine_dedup_threshold_is_not_an_l2_approximation() -> MerkurResult<()> {
    let (lance, sqlite, dir) = test_paths("cosine_dedup");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let mut original = mem("original", "alpha");
    original.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let original = storage.insert_memory(&original).await?;
    let mut distinct = mem("related but distinct", "alpha");
    distinct.embedding = Some(vec![0.85, (1.0_f32 - 0.85 * 0.85).sqrt(), 0.0, 0.0]);
    let id = storage.insert_memory_dedup(&distinct, 0.92).await?;
    assert_ne!(
        id, original,
        "cosine 0.85 must not pass dedup threshold 0.92"
    );
    let mut duplicate = mem("same direction scaled", "alpha");
    duplicate.embedding = Some(vec![98.0, 100.0 * (1.0_f32 - 0.98 * 0.98).sqrt(), 0.0, 0.0]);
    assert_eq!(
        storage.insert_memory_dedup(&duplicate, 0.92).await?,
        original
    );
    drop(storage);
    std::fs::remove_dir_all(dir).ok();
    Ok(())
}

#[tokio::test]
async fn lance_vectors_follow_publication_and_survive_absorbed_source_purge() -> MerkurResult<()> {
    use merkur_core::{AdjudicationWrite, MemorySnapshot};
    let (lance, sqlite, dir) = test_paths("publication");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let mut original = mem("old", "alpha");
    original.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let target = storage.insert_memory(&original).await?;
    storage
        .mark_consolidated(std::slice::from_ref(&target))
        .await?;
    let mut source = mem("new", "alpha");
    source.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let source = storage.insert_memory(&source).await?;
    let embeddings = storage
        .get_embeddings(&[source.clone(), target.clone()])
        .await?;
    assert_eq!(embeddings[&source], vec![0.0, 1.0, 0.0, 0.0]);
    let write = AdjudicationWrite::Absorb {
        pending: MemorySnapshot::from(&storage.get_memory(&source).await?.unwrap()),
        target: MemorySnapshot::from(&storage.get_memory(&target).await?.unwrap()),
    };
    assert!(storage.apply_adjudication(&write).await?);
    assert_eq!(storage.get_memory(&target).await?.unwrap().content, "new");
    assert_eq!(storage.purge_invalidated_older_than(0).await?, 1);
    assert_eq!(
        storage
            .get_embeddings(std::slice::from_ref(&target))
            .await?[&target],
        vec![0.0, 1.0, 0.0, 0.0]
    );
    let hits = storage
        .vector_search_ns(&[0.0, 1.0, 0.0, 0.0], "alpha", 1)
        .await?;
    assert_eq!(hits[0].id, target);
    assert!(hits[0].score > 0.99);

    storage
        .update_memory(&target, "edited", Some(&[0.0, 0.0, 1.0, 0.0]))
        .await?;
    let reopened = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let hits = reopened
        .vector_search_ns(&[0.0, 0.0, 1.0, 0.0], "alpha", 5)
        .await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, target);
    assert!(hits[0].score > 0.99);
    assert_eq!(
        reopened
            .get_embeddings(std::slice::from_ref(&target))
            .await?[&target],
        vec![0.0, 0.0, 1.0, 0.0]
    );
    reopened.update_memory(&target, "text only", None).await?;
    assert!(
        storage
            .get_embeddings(std::slice::from_ref(&target))
            .await?
            .is_empty()
    );
    assert!(
        storage
            .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "alpha", 5)
            .await?
            .is_empty()
    );
    drop(reopened);
    drop(storage);
    std::fs::remove_dir_all(dir).ok();
    Ok(())
}

#[tokio::test]
async fn lance_failed_publications_are_invisible_and_stale_absorptions_conflict() -> MerkurResult<()>
{
    use merkur_core::{AdjudicationWrite, MemorySnapshot};
    let (lance, sqlite, dir) = test_paths("conflict");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let mut m = mem("old", "alpha");
    m.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let target = storage.insert_memory(&m).await?;
    m.content = "new".into();
    m.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let source = storage.insert_memory(&m).await?;
    let source_snapshot = MemorySnapshot::from(&storage.get_memory(&source).await?.unwrap());
    let stale = AdjudicationWrite::Absorb {
        pending: source_snapshot.clone(),
        target: MemorySnapshot::from(&storage.get_memory(&target).await?.unwrap()),
    };
    let writer = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    assert!(
        writer
            .update_memory(&target, "bad dimensions", Some(&[1.0, 0.0]))
            .await
            .is_err()
    );
    assert_eq!(storage.get_memory(&target).await?.unwrap().content, "old");
    writer
        .update_memory(&target, "edited", Some(&[0.0, 0.0, 1.0, 0.0]))
        .await?;
    assert!(!storage.apply_adjudication(&stale).await?);
    let fresh = AdjudicationWrite::Absorb {
        pending: source_snapshot,
        target: MemorySnapshot::from(&storage.get_memory(&target).await?.unwrap()),
    };
    let conn = rusqlite::Connection::open(&sqlite).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_invalidation BEFORE UPDATE OF invalid_at ON memories
        BEGIN SELECT RAISE(ABORT, 'injected invalidation failure'); END;",
    )
    .unwrap();
    assert!(storage.apply_adjudication(&fresh).await.is_err());
    assert_eq!(
        storage.get_memory(&target).await?.unwrap().content,
        "edited"
    );
    assert_eq!(
        storage
            .get_embeddings(std::slice::from_ref(&target))
            .await?[&target],
        vec![0.0, 0.0, 1.0, 0.0]
    );
    assert!(
        storage
            .get_memory(&source)
            .await?
            .unwrap()
            .invalid_at
            .is_none()
    );
    conn.execute_batch(
        "DROP TRIGGER fail_invalidation;
        CREATE TRIGGER fail_publication BEFORE UPDATE OF content ON memories
        BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
    )
    .unwrap();
    for _ in 0..5 {
        assert!(
            storage
                .update_memory(&target, "unpublished", Some(&[0.0, 0.0, 0.0, 1.0]))
                .await
                .is_err()
        );
    }
    let hits = storage
        .vector_search_ns(&[0.0, 0.0, 0.0, 1.0], "alpha", 2)
        .await?;
    assert_eq!(hits.len(), 2, "deepen past unreferenced staged keys");
    assert!(
        hits.iter().all(|hit| hit.content != "unpublished"
            && [source.clone(), target.clone()].contains(&hit.id))
    );
    conn.execute_batch("DROP TRIGGER fail_publication;")
        .unwrap();
    conn.execute_batch("UPDATE lance_vector_staging SET created_at = '2000-01-01T00:00:00+00:00';
        DELETE FROM lance_vector_staging WHERE vector_id IN (SELECT vector_id FROM lance_vector_staging LIMIT 1);").unwrap();
    assert_eq!(
        storage.cleanup_orphan_vectors().await?,
        5,
        "collect failed stages and pre-ledger orphans without touching referenced keys"
    );
    assert_eq!(
        storage
            .get_embeddings(&[source.clone(), target.clone()])
            .await?
            .len(),
        2
    );
    assert!(storage.apply_adjudication(&fresh).await?);
    drop(conn);
    drop(writer);
    drop(storage);
    std::fs::remove_dir_all(dir).ok();
    Ok(())
}

#[tokio::test]
async fn lance_legacy_keys_and_optional_embeddings_remain_readable() -> MerkurResult<()> {
    use arrow_array::{FixedSizeListArray, RecordBatch, StringArray, types::Float32Type};
    use std::sync::Arc;
    let (lance, sqlite, dir) = test_paths("legacy");
    let storage = LanceDbStorage::new(&lance, &sqlite, 4).await?;
    let id = storage
        .insert_memory(&mem("legacy vector", "alpha"))
        .await?;
    let text_only = storage
        .insert_memory(&mem("legacy text only", "alpha"))
        .await?;
    let db = lancedb::connect(&lance).execute().await.unwrap();
    let table = db.open_table("vectors").execute().await.unwrap();
    let vector = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
        vec![Some(vec![Some(1.0), Some(0.0), Some(0.0), Some(0.0)])],
        4,
    );
    let batch = RecordBatch::try_new(
        table.schema().await.unwrap(),
        vec![
            Arc::new(StringArray::from(vec![id.clone()])),
            Arc::new(vector),
        ],
    )
    .unwrap();
    table.add(vec![batch]).execute().await.unwrap();
    let conn = rusqlite::Connection::open(&sqlite).unwrap();
    conn.execute("UPDATE memories SET lance_vector_id = NULL", [])
        .unwrap();
    let embeddings = storage
        .get_embeddings(&[id.clone(), text_only.clone()])
        .await?;
    assert_eq!(embeddings[&id], vec![1.0, 0.0, 0.0, 0.0]);
    assert!(!embeddings.contains_key(&text_only));
    let invalid_absorption = merkur_core::AdjudicationWrite::Absorb {
        pending: merkur_core::MemorySnapshot::from(&storage.get_memory(&text_only).await?.unwrap()),
        target: merkur_core::MemorySnapshot::from(&storage.get_memory(&id).await?.unwrap()),
    };
    assert!(
        storage
            .apply_adjudication(&invalid_absorption)
            .await
            .is_err(),
        "legacy optional vectors cannot be absorbed as missing references"
    );
    assert!(
        storage
            .get_memory(&text_only)
            .await?
            .unwrap()
            .invalid_at
            .is_none()
    );
    assert_eq!(
        storage
            .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "alpha", 1)
            .await?[0]
            .id,
        id
    );
    storage.delete_memory(&id).await?;
    assert!(
        storage
            .vector_search_ns(&[1.0, 0.0, 0.0, 0.0], "alpha", 1)
            .await?
            .is_empty()
    );
    drop(conn);
    drop(table);
    drop(db);
    drop(storage);
    std::fs::remove_dir_all(dir).ok();
    Ok(())
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
