use async_trait::async_trait;
use merkur_core::{Embedder, MerkurResult};
use rand::Rng;
use std::hash::{DefaultHasher, Hash, Hasher};

pub struct NoopEmbedder {
    dim: usize,
    deterministic: bool,
}

impl NoopEmbedder {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            deterministic: true,
        }
    }

    pub fn new_random(dim: usize) -> Self {
        Self {
            dim,
            deterministic: false,
        }
    }

    fn make_vector(&self, text: &str) -> Vec<f32> {
        if self.deterministic {
            let mut hasher = DefaultHasher::new();
            text.hash(&mut hasher);
            let seed = hasher.finish();
            let mut rng: rand::rngs::StdRng = rand::SeedableRng::seed_from_u64(seed);
            let vec: Vec<f32> = (0..self.dim)
                .map(|_| rng.gen_range(-1.0_f32..1.0))
                .collect();
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                vec.into_iter().map(|x| x / norm).collect()
            } else {
                vec![0.0; self.dim]
            }
        } else {
            let mut rng = rand::thread_rng();
            let vec: Vec<f32> = (0..self.dim)
                .map(|_| rng.gen_range(-1.0_f32..1.0))
                .collect();
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            vec.into_iter().map(|x| x / norm).collect()
        }
    }
}

#[async_trait]
impl Embedder for NoopEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn encode_batch(&self, texts: &[String]) -> MerkurResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.make_vector(t)).collect())
    }

    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>> {
        Ok(self.make_vector(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_noop_embedder_deterministic() {
        let e = NoopEmbedder::new(384);
        assert_eq!(e.dim(), 384);
        let vec1 = e.encode("hello").await.unwrap();
        assert_eq!(vec1.len(), 384);
        let vec2 = e.encode("hello").await.unwrap();
        assert_eq!(vec1, vec2);
        let vec3 = e.encode("world").await.unwrap();
        assert_ne!(vec1, vec3);
    }

    #[tokio::test]
    async fn test_noop_embedder_batch() {
        let e = NoopEmbedder::new(128);
        let texts: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let vecs = e.encode_batch(&texts).await.unwrap();
        assert_eq!(vecs.len(), 3);
        assert_eq!(vecs[0].len(), 128);
    }
}
