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

criterion_group!(benches, bench_vector_search);
criterion_main!(benches);
