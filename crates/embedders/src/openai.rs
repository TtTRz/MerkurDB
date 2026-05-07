use async_trait::async_trait;
use merkur_core::{Embedder, MerkurError, MerkurResult};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Serialize)]
struct OpenAIEmbedRequest {
    model: String,
    input: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenAIEmbedResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

pub struct OpenAIEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dim: usize,
}

impl OpenAIEmbedder {
    pub async fn new(base_url: &str, api_key: &str, model: &str) -> MerkurResult<Self> {
        let client = reqwest::Client::new();
        let base_url = base_url.trim_end_matches('/').to_string();

        // Probe to get the embedding dimension
        let probe_resp = client
            .post(format!("{base_url}/v1/embeddings"))
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&OpenAIEmbedRequest {
                model: model.to_string(),
                input: serde_json::Value::String("probe".to_string()),
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to connect to OpenAI API: {e}")))?;

        if !probe_resp.status().is_success() {
            let status = probe_resp.status();
            let body = probe_resp.text().await.unwrap_or_default();
            return Err(MerkurError::Embedding(format!(
                "OpenAI API returned {status}: {body}"
            )));
        }

        let resp: OpenAIEmbedResponse = probe_resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse OpenAI response: {e}")))?;

        let dim = resp.data.first().map(|d| d.embedding.len()).unwrap_or(1536);

        debug!("OpenAIEmbedder initialized: model={model}, dim={dim}");

        Ok(Self {
            client,
            base_url,
            api_key: api_key.to_string(),
            model: model.to_string(),
            dim,
        })
    }
}

#[async_trait]
impl Embedder for OpenAIEmbedder {
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
            .post(format!("{}/v1/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&OpenAIEmbedRequest {
                model: self.model.clone(),
                input: serde_json::Value::Array(inputs),
            })
            .send()
            .await
            .map_err(|e| MerkurError::Embedding(format!("OpenAI request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MerkurError::Embedding(format!(
                "OpenAI API returned {status}: {body}"
            )));
        }

        let mut resp: OpenAIEmbedResponse = resp
            .json()
            .await
            .map_err(|e| MerkurError::Embedding(format!("Failed to parse OpenAI response: {e}")))?;

        resp.data.sort_by_key(|d| d.index);
        let embeddings: Vec<Vec<f32>> = resp.data.into_iter().map(|d| d.embedding).collect();

        Ok(embeddings)
    }

    async fn encode(&self, text: &str) -> MerkurResult<Vec<f32>> {
        let vecs = self.encode_batch(&[text.to_string()]).await?;
        vecs.into_iter()
            .next()
            .ok_or_else(|| MerkurError::Embedding("OpenAI returned empty response".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires OPENAI_API_KEY env var"]
    async fn test_openai_embedder() {
        let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
        let e = OpenAIEmbedder::new("https://api.openai.com", &api_key, "text-embedding-3-small")
            .await
            .unwrap();
        assert!(e.dim() > 0);
        let vec = e.encode("hello world").await.unwrap();
        assert_eq!(vec.len(), e.dim());
    }
}
