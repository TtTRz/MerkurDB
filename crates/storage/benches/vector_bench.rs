use criterion::{Criterion, black_box, criterion_group, criterion_main};
use merkur_storage::InMemoryVectorIndex;

fn bench_vector_search(c: &mut Criterion) {
    let dim = 384;
    let n = 10_000;
    let idx = InMemoryVectorIndex::new(dim);

    for i in 0..n {
        let mut vec = vec![0.0f32; dim];
        vec[i % dim] = 1.0;
        vec[(i + 1) % dim] = 0.5;
        idx.add(format!("mem_{i:06}"), vec);
    }

    let query: Vec<f32> = (0..dim).map(|i| (i as f32).sin()).collect();

    c.bench_function("vector_search_10k_top100", |b| {
        b.iter(|| {
            black_box(idx.search(&query, 100));
        });
    });
}

fn bench_upsert_remove(c: &mut Criterion) {
    let dim = 384;

    c.bench_function("upsert_10k_remove_1k", |b| {
        b.iter(|| {
            let idx = InMemoryVectorIndex::new(dim);
            for i in 0..10_000 {
                let mut vec = vec![0.0f32; dim];
                vec[i % dim] = 1.0;
                idx.add(format!("mem_{i:06}"), vec);
            }
            for i in 0..1_000 {
                idx.remove(&format!("mem_{i:06}"));
            }
            black_box(idx.len());
        });
    });
}

fn bench_bfs_expand(c: &mut Criterion) {
    use merkur_core::{EdgeType, NewEdge, NewMemory, Storage};
    use merkur_storage::SqliteStorage;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let storage = SqliteStorage::new("file:bench_bfs?mode=memory&cache=shared", 4).unwrap();

    // Build a chain of 1K nodes
    let ids: Vec<String> = rt.block_on(async {
        let mut ids = Vec::new();
        for i in 0..1_000 {
            let id = storage
                .insert_memory(&NewMemory {
                    content: format!("node {i}"),
                    category: None,
                    context: Default::default(),
                    metadata: Default::default(),
                    embedding: Some(vec![i as f32, 0.0, 0.0, 0.0]),
                })
                .await
                .unwrap();
            ids.push(id);
        }
        for i in 0..999 {
            let _ = storage
                .insert_edge(&NewEdge {
                    source_id: ids[i].clone(),
                    target_id: ids[i + 1].clone(),
                    weight: Some(1.0),
                    relation: None,
                    edge_type: EdgeType::Auto,
                })
                .await;
        }
        ids
    });

    c.bench_function("bfs_expand_1k_depth3", |b| {
        b.iter(|| {
            rt.block_on(async {
                let seeds = vec![ids[0].clone()];
                black_box(storage.bfs_expand(&seeds, 3, 100).await.unwrap());
            });
        });
    });
}

criterion_group!(
    benches,
    bench_vector_search,
    bench_upsert_remove,
    bench_bfs_expand
);
criterion_main!(benches);
