use async_trait::async_trait;
use merkur_core::{Embedder, MerkurError, MerkurResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest {
    model: String,
    input: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

pub struct OllamaEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dim: usize,
}

impl OllamaEmbedder {
    pub async fn new(base_url: &str, model: &str) -> MerkurResult<Self> {
        let client = reqwest::Client::new();
        let base_url = base_url.trim_end_matches('/').to_string();

        // Probe to get the embedding dimension
        let probe_resp = client
            .post(format!("{base_url}/api/embeddings"))
            .json(&OllamaEmbedRequest {
                model: model.to_string(),
                input: serde_json::Value::String("probe".to_string()),
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to connect to Ollama: {e}")))?;

        let resp: OllamaEmbedResponse = probe_resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse Ollama response: {e}")))?;

        let dim = resp.embeddings.first().map(|v| v.len()).unwrap_or(384);

        debug!("OllamaEmbedder initialized: model={model}, dim={dim}");

        Ok(Self {
            client,
            base_url,
            model: model.to_string(),
            dim,
        })
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn encode_batch(&self, texts: &[String]) -> MerkurResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let inputs: Vec<serde_json::Value> = texts
            .iter()
            .map(|t| serde_json::Value::String(t.clone()))
            .collect();

        let resp = self
            .client
            .post(format!("{}/api/embeddings", self.base_url))
            .json(&OllamaEmbedRequest {
                model: self.model.clone(),
                input: serde_json::Value::Array(inputs),
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Ollama request failed: {e}")))?;

        let resp: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse Ollama response: {e}")))?;

        Ok(resp.embeddings)
    }

    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>> {
        let vecs = self.encode_batch(&[text.to_string()]).await?;
        vecs.into_iter()
            .next()
            .ok_or_else(|| MerkurError::Embedding("Ollama returned empty response".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires ollama running"]
    async fn test_ollama_embedder() {
        let e = OllamaEmbedder::new("http://localhost:11434", "all-minilm")
            .await
            .unwrap();
        assert!(e.dim() > 0);
        let vec = e.encode("hello world").await.unwrap();
        assert_eq!(vec.len(), e.dim());
        assert!(vec.iter().any(|&x| x != 0.0));
    }
}
