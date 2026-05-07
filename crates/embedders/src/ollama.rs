use async_trait::async_trait;
use merkur_core::{Embedder, MerkurError, MerkurResult};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

/// Ollama batch embeddings endpoint.
///
/// We target the modern `/api/embed` endpoint, which accepts either a single
/// string or an array of strings under the `input` field and always returns
/// `embeddings: [[...]]`. The legacy `/api/embeddings` endpoint uses the
/// singular field names `prompt` / `embedding` and is no longer guaranteed to
/// exist on recent Ollama builds.
const OLLAMA_PATH: &str = "/api/embed";

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| MerkurError::Embedding(format!("Failed to build HTTP client: {e}")))?;
        let base_url = base_url.trim_end_matches('/').to_string();

        // Probe to discover the embedding dimension. We refuse to start if the
        // probe fails or returns an empty vector — guessing 384 is worse than
        // failing loudly because it would silently corrupt the vector index.
        let probe_resp = client
            .post(format!("{base_url}{OLLAMA_PATH}"))
            .json(&OllamaEmbedRequest {
                model,
                input: serde_json::Value::String("probe".to_string()),
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to connect to Ollama: {e}")))?;

        if !probe_resp.status().is_success() {
            let status = probe_resp.status();
            let body = probe_resp.text().await.unwrap_or_default();
            return Err(MerkurError::Embedding(format!(
                "Ollama probe returned {status}: {body}"
            )));
        }

        let resp: OllamaEmbedResponse = probe_resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse Ollama response: {e}")))?;

        let dim = resp
            .embeddings
            .first()
            .map(|v| v.len())
            .filter(|&n| n > 0)
            .ok_or_else(|| {
                MerkurError::Embedding("Ollama probe returned an empty embedding".into())
            })?;

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

        let inputs = serde_json::Value::Array(
            texts
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect(),
        );

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, OLLAMA_PATH))
            .json(&OllamaEmbedRequest {
                model: &self.model,
                input: inputs,
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Ollama request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MerkurError::Embedding(format!(
                "Ollama returned {status}: {body}"
            )));
        }

        let resp: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse Ollama response: {e}")))?;

        if resp.embeddings.len() != texts.len() {
            return Err(MerkurError::Embedding(format!(
                "Ollama returned {} embeddings for {} inputs",
                resp.embeddings.len(),
                texts.len()
            )));
        }

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
