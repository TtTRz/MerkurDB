use parking_lot::RwLock;
use std::collections::{BinaryHeap, HashMap};

/// In-memory vector index with O(1) id-keyed update and O(n log k) top-k search.
///
/// Uses [`parking_lot::RwLock`] so lock acquisition never fails and
/// poison-recovery is not a concern. All critical sections are short and
/// contain no `.await`, so the blocking lock is safe from async contexts.
pub struct InMemoryVectorIndex {
    inner: RwLock<Inner>,
    dim: usize,
}

struct Inner {
    /// Parallel storage: `ids[i]` corresponds to `vectors[i]`.
    ids: Vec<String>,
    vectors: Vec<Vec<f32>>,
    /// id → index position in the vectors array.
    index_of: HashMap<String, usize>,
}

impl Inner {
    fn new() -> Self {
        Self {
            ids: Vec::new(),
            vectors: Vec::new(),
            index_of: HashMap::new(),
        }
    }

    fn upsert(&mut self, id: String, vec: Vec<f32>) {
        if let Some(&idx) = self.index_of.get(&id) {
            self.vectors[idx] = vec;
        } else {
            let idx = self.ids.len();
            self.index_of.insert(id.clone(), idx);
            self.ids.push(id);
            self.vectors.push(vec);
        }
    }

    fn remove(&mut self, id: &str) {
        if let Some(idx) = self.index_of.remove(id) {
            // Swap-remove to keep O(1); patch the moved entry's index.
            let last = self.ids.len() - 1;
            if idx != last {
                self.ids.swap(idx, last);
                self.vectors.swap(idx, last);
                let moved_id = self.ids[idx].clone();
                self.index_of.insert(moved_id, idx);
            }
            self.ids.pop();
            self.vectors.pop();
        }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }
}

impl InMemoryVectorIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            inner: RwLock::new(Inner::new()),
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
        self.inner.write().upsert(id, vec);
    }

    pub fn remove(&self, id: &str) {
        self.inner.write().remove(id);
    }

    /// Top-k cosine similarity search using a min-heap of size `limit`.
    ///
    /// Complexity: O(n log k) vs the naive O(n log n). Returns scores in the
    /// closed interval [-1, 1] for pairs of non-zero vectors, or 0.0 when
    /// either operand is the zero vector. Ordering uses [`f64::total_cmp`],
    /// so NaN scores are handled consistently without violating the `Ord`
    /// contract.
    pub fn search(&self, query: &[f32], limit: usize) -> Vec<(String, f64)> {
        if limit == 0 {
            return Vec::new();
        }
        let inner = self.inner.read();

        // Min-heap: smallest score at the top so we can pop it when a better
        // candidate arrives. Stores (TotalF64, index_in_ids).
        let mut heap: BinaryHeap<std::cmp::Reverse<(TotalF64, usize)>> =
            BinaryHeap::with_capacity(limit + 1);
        let query_norm = l2_norm(query);
        for (i, vec) in inner.vectors.iter().enumerate() {
            let score = cosine_similarity(query, vec, query_norm);
            heap.push(std::cmp::Reverse((TotalF64(score), i)));
            if heap.len() > limit {
                heap.pop();
            }
        }

        let mut results: Vec<(String, f64)> = heap
            .into_iter()
            .map(|std::cmp::Reverse((TotalF64(score), i))| (inner.ids[i].clone(), score))
            .collect();
        // Sort descending by score for stable output. Using total_cmp keeps
        // NaN at the bottom (largest via total order semantics), matching
        // the heap's eviction policy.
        results.sort_by(|a, b| b.1.total_cmp(&a.1));
        results
    }

    pub fn rebuild(&self, all: Vec<(String, Vec<f32>)>) {
        let mut inner = self.inner.write();
        *inner = Inner::new();
        for (id, vec) in all {
            inner.upsert(id, vec);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// f64 wrapper with a total order based on [`f64::total_cmp`].
///
/// Unlike a hand-rolled `PartialOrd` → `Ord` bridge, this respects the
/// `Eq` contract: `a == b` iff `cmp(a, b) == Equal`, because `total_cmp`
/// is a true total order from IEEE 754's `totalOrder` predicate.
#[derive(Debug, Clone, Copy)]
struct TotalF64(f64);

impl PartialEq for TotalF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == std::cmp::Ordering::Equal
    }
}

impl Eq for TotalF64 {}

impl PartialOrd for TotalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TotalF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

fn l2_norm(v: &[f32]) -> f64 {
    v.iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt()
}

fn cosine_similarity(a: &[f32], b: &[f32], norm_a: f64) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let norm_b = l2_norm(b);
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upsert_replaces_existing() {
        let idx = InMemoryVectorIndex::new(3);
        idx.add("a".into(), vec![1.0, 0.0, 0.0]);
        idx.add("a".into(), vec![0.0, 1.0, 0.0]);
        assert_eq!(idx.len(), 1);
        let r = idx.search(&[0.0, 1.0, 0.0], 1);
        assert_eq!(r[0].0, "a");
        assert!((r[0].1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_remove_swap() {
        let idx = InMemoryVectorIndex::new(2);
        idx.add("a".into(), vec![1.0, 0.0]);
        idx.add("b".into(), vec![0.0, 1.0]);
        idx.add("c".into(), vec![1.0, 1.0]);
        idx.remove("a");
        assert_eq!(idx.len(), 2);
        // Remaining entries still searchable.
        let r = idx.search(&[0.0, 1.0], 2);
        let ids: Vec<_> = r.iter().map(|(id, _)| id.clone()).collect();
        assert!(ids.contains(&"b".to_string()));
        assert!(ids.contains(&"c".to_string()));
    }

    #[test]
    fn test_topk_smaller_than_limit() {
        let idx = InMemoryVectorIndex::new(2);
        idx.add("a".into(), vec![1.0, 0.0]);
        idx.add("b".into(), vec![0.0, 1.0]);
        let r = idx.search(&[1.0, 0.0], 10);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, "a");
    }

    #[test]
    fn test_zero_vector_score_is_zero() {
        let idx = InMemoryVectorIndex::new(2);
        idx.add("z".into(), vec![0.0, 0.0]);
        let r = idx.search(&[1.0, 0.0], 1);
        assert_eq!(r[0].1, 0.0);
    }
}
