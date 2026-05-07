use async_trait::async_trait;
use merkur_core::{Embedder, MerkurError, MerkurResult};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::debug;

#[derive(Debug, Serialize)]
struct OpenAIEmbedRequest<'a> {
    model: &'a str,
    input: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
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
    /// Optional output dimension override for `text-embedding-3-*` models.
    requested_dim: Option<usize>,
}

impl OpenAIEmbedder {
    pub async fn new(base_url: &str, api_key: &str, model: &str) -> MerkurResult<Self> {
        Self::new_with_dimensions(base_url, api_key, model, None).await
    }

    pub async fn new_with_dimensions(
        base_url: &str,
        api_key: &str,
        model: &str,
        dimensions: Option<usize>,
    ) -> MerkurResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| MerkurError::Embedding(format!("Failed to build HTTP client: {e}")))?;
        let base_url = base_url.trim_end_matches('/').to_string();

        let probe_resp = client
            .post(format!("{base_url}/v1/embeddings"))
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&OpenAIEmbedRequest {
                model,
                input: serde_json::Value::String("probe".to_string()),
                dimensions,
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

        let dim = resp
            .data
            .first()
            .map(|d| d.embedding.len())
            .filter(|&n| n > 0)
            .ok_or_else(|| MerkurError::Embedding("OpenAI probe returned no embeddings".into()))?;

        debug!("OpenAIEmbedder initialized: model={model}, dim={dim}");

        Ok(Self {
            client,
            base_url,
            api_key: api_key.to_string(),
            model: model.to_string(),
            dim,
            requested_dim: dimensions,
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
        // OpenAI caps inputs at 2048 per request; we conservatively enforce
        // a smaller cap to leave token budget headroom.
        const MAX_BATCH: usize = 512;
        if texts.len() > MAX_BATCH {
            return Err(MerkurError::BadRequest(format!(
                "OpenAI batch size {} exceeds limit {MAX_BATCH}",
                texts.len()
            )));
        }

        let inputs = serde_json::Value::Array(
            texts
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect(),
        );

        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&OpenAIEmbedRequest {
                model: &self.model,
                input: inputs,
                dimensions: self.requested_dim,
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

        if resp.data.len() != texts.len() {
            return Err(MerkurError::Embedding(format!(
                "OpenAI returned {} embeddings for {} inputs",
                resp.data.len(),
                texts.len()
            )));
        }

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
