use async_trait::async_trait;
use merkur_core::{Embedder, MerkurError, MerkurResult};
use rand::Rng;
use sha2::{Digest, Sha256};

/// In-process deterministic / pseudo-random embedder for tests and demos.
///
/// `deterministic = true` derives the seed from a SHA-256 of the input text,
/// guaranteeing identical vectors across Rust versions and platforms — unlike
/// `std::collections::DefaultHasher`, whose algorithm is explicitly not stable.
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

    fn make_vector(&self, text: &str) -> MerkurResult<Vec<f32>> {
        if self.dim == 0 {
            return Err(MerkurError::Embedding(
                "NoopEmbedder dim must be greater than 0".into(),
            ));
        }

        let vec: Vec<f32> = if self.deterministic {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            let digest = hasher.finalize();
            // Take the first 8 bytes of the SHA-256 digest as a u64 seed for a
            // standard StdRng. SHA-256 is stable across versions / platforms.
            let mut seed_bytes = [0u8; 8];
            seed_bytes.copy_from_slice(&digest[..8]);
            let seed = u64::from_le_bytes(seed_bytes);
            let mut rng: rand::rngs::StdRng = rand::SeedableRng::seed_from_u64(seed);
            (0..self.dim)
                .map(|_| rng.gen_range(-1.0_f32..1.0))
                .collect()
        } else {
            let mut rng = rand::thread_rng();
            (0..self.dim)
                .map(|_| rng.gen_range(-1.0_f32..1.0))
                .collect()
        };

        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            Ok(vec.into_iter().map(|x| x / norm).collect())
        } else {
            // Astronomically unlikely but technically possible; fall back to a
            // canonical unit vector so we never emit a zero vector.
            let mut fallback = vec![0.0_f32; self.dim];
            fallback[0] = 1.0;
            Ok(fallback)
        }
    }
}

#[async_trait]
impl Embedder for NoopEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn encode_batch(&self, texts: &[String]) -> MerkurResult<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.make_vector(t)?);
        }
        Ok(out)
    }

    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>> {
        self.make_vector(text)
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

    #[tokio::test]
    async fn test_noop_embedder_zero_dim_errors() {
        let e = NoopEmbedder::new(0);
        assert!(e.encode("x").await.is_err());
    }
}
