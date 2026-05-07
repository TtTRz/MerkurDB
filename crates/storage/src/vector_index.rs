use std::sync::RwLock;

pub struct InMemoryVectorIndex {
    vectors: RwLock<Vec<(String, Vec<f32>)>>,
    dim: usize,
}

impl InMemoryVectorIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            vectors: RwLock::new(Vec::new()),
            dim,
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn add(&self, id: String, vec: Vec<f32>) {
        debug_assert_eq!(
            vec.len(),
            self.dim,
            "Vector dimension mismatch: expected {}, got {}",
            self.dim,
            vec.len()
        );
        let mut vectors = self.vectors.write().expect("lock poisoned");
        vectors.retain(|(existing_id, _)| existing_id != &id);
        vectors.push((id, vec));
    }

    pub fn remove(&self, id: &str) {
        let mut vectors = self.vectors.write().expect("lock poisoned");
        vectors.retain(|(existing_id, _)| existing_id != id);
    }

    pub fn search(&self, query: &[f32], limit: usize) -> Vec<(String, f64)> {
        let vectors = self.vectors.read().expect("lock poisoned");
        let mut scored: Vec<(String, f64)> = vectors
            .iter()
            .map(|(id, vec)| {
                let score = cosine_similarity(query, vec);
                (id.clone(), score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }

    pub fn rebuild(&self, all: Vec<(String, Vec<f32>)>) {
        let mut vectors = self.vectors.write().expect("lock poisoned");
        *vectors = all;
    }

    pub fn len(&self) -> usize {
        self.vectors.read().expect("lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm_a: f64 = a
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    let norm_b: f64 = b
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}
