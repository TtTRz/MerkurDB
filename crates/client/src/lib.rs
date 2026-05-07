use async_trait::async_trait;
use merkur_core::{Edge, Memory, ScoredMemory};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error: {code} — {message}")]
    Api { code: String, message: String },
}

pub type ClientResult<T> = Result<T, ClientError>;

// ── Response types ──

#[derive(Debug, Deserialize)]
pub struct WriteResponse {
    pub id: String,
    pub status: String,
    pub searchable: bool,
}

#[derive(Debug, Deserialize)]
pub struct WriteBatchResponse {
    pub ids: Vec<String>,
    pub count: usize,
    pub requested: Option<usize>,
    pub errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub mode: String,
    pub results: Vec<ScoredMemory>,
    pub total: usize,
    pub time_ms: u64,
    pub graph: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct StatusResponse {
    pub total_memories: usize,
    pub total_edges: usize,
    pub pending_consolidation: usize,
    pub by_level: HashMap<i32, usize>,
    pub uptime_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ConsolidateResponse {
    pub status: String,
    pub processed: usize,
    pub edges_created: usize,
    pub errors: usize,
}

#[derive(Debug, Deserialize)]
pub struct ForgetResponse {
    pub status: String,
    pub archived: usize,
    pub downgraded: usize,
    pub cleaned: usize,
}

#[derive(Debug, Deserialize)]
pub struct ConsolidationLogEntry {
    pub id: i64,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub memories_processed: i64,
    pub edges_created: i64,
    pub errors: i64,
}

#[derive(Debug, Deserialize)]
pub struct GraphResponse {
    pub center: String,
    pub neighborhood: Vec<serde_json::Value>,
    pub edges: Vec<Edge>,
}

// ── Trait ──

#[async_trait]
pub trait MerkurClient: Send + Sync {
    async fn write(
        &self,
        content: &str,
        context: Option<HashMap<String, String>>,
    ) -> ClientResult<WriteResponse>;

    async fn write_batch(&self, items: &[WriteItem]) -> ClientResult<WriteBatchResponse>;

    async fn search(
        &self,
        query: &str,
        mode: Option<&str>,
        limit: Option<usize>,
        threshold: Option<f64>,
    ) -> ClientResult<SearchResponse>;

    async fn get_memory(&self, id: &str) -> ClientResult<Memory>;

    async fn update_memory(&self, id: &str, content: &str) -> ClientResult<()>;

    async fn delete_memory(&self, id: &str) -> ClientResult<()>;

    async fn status(&self) -> ClientResult<StatusResponse>;

    async fn consolidate(&self) -> ClientResult<ConsolidateResponse>;

    async fn forget(&self) -> ClientResult<ForgetResponse>;

    async fn relate(
        &self,
        source_id: &str,
        target_id: &str,
        relation: Option<&str>,
        weight: Option<f64>,
    ) -> ClientResult<()>;

    async fn graph(&self, id: &str) -> ClientResult<GraphResponse>;

    async fn health(&self) -> ClientResult<String>;
}

// ── Request types ──

#[derive(Debug, Serialize)]
pub struct WriteItem {
    pub content: String,
    pub context: Option<HashMap<String, String>>,
}

// ── HTTP implementation ──

pub struct HttpMerkurClient {
    client: reqwest::Client,
    base_url: String,
}

impl HttpMerkurClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> ClientResult<T> {
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::CREATED {
            Ok(resp.json().await?)
        } else {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let error = &body["error"];
            Err(ClientError::Api {
                code: error["code"].as_str().unwrap_or("UNKNOWN").into(),
                message: error["message"].as_str().unwrap_or("Unknown error").into(),
            })
        }
    }
}

#[async_trait]
impl MerkurClient for HttpMerkurClient {
    async fn write(
        &self,
        content: &str,
        context: Option<HashMap<String, String>>,
    ) -> ClientResult<WriteResponse> {
        let resp = self
            .client
            .post(format!("{}/v1/write", self.base_url))
            .json(&serde_json::json!({ "content": content, "context": context }))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn write_batch(&self, items: &[WriteItem]) -> ClientResult<WriteBatchResponse> {
        let resp = self
            .client
            .post(format!("{}/v1/write-batch", self.base_url))
            .json(&serde_json::json!({ "items": items }))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn search(
        &self,
        query: &str,
        mode: Option<&str>,
        limit: Option<usize>,
        threshold: Option<f64>,
    ) -> ClientResult<SearchResponse> {
        let mut params = vec![("q", query.to_string())];
        if let Some(m) = mode {
            params.push(("mode", m.to_string()));
        }
        if let Some(l) = limit {
            params.push(("limit", l.to_string()));
        }
        if let Some(t) = threshold {
            params.push(("score_threshold", t.to_string()));
        }
        let resp = self
            .client
            .get(format!("{}/v1/search", self.base_url))
            .query(&params)
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn get_memory(&self, id: &str) -> ClientResult<Memory> {
        let resp = self
            .client
            .get(format!("{}/v1/memory/{id}", self.base_url))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn update_memory(&self, id: &str, content: &str) -> ClientResult<()> {
        let resp = self
            .client
            .put(format!("{}/v1/memory/{id}", self.base_url))
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let error = &body["error"];
            Err(ClientError::Api {
                code: error["code"].as_str().unwrap_or("UNKNOWN").into(),
                message: error["message"].as_str().unwrap_or("Unknown error").into(),
            })
        }
    }

    async fn delete_memory(&self, id: &str) -> ClientResult<()> {
        let resp = self
            .client
            .delete(format!("{}/v1/memory/{id}", self.base_url))
            .send()
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let error = &body["error"];
            Err(ClientError::Api {
                code: error["code"].as_str().unwrap_or("UNKNOWN").into(),
                message: error["message"].as_str().unwrap_or("Unknown error").into(),
            })
        }
    }

    async fn status(&self) -> ClientResult<StatusResponse> {
        let resp = self
            .client
            .get(format!("{}/v1/status", self.base_url))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn consolidate(&self) -> ClientResult<ConsolidateResponse> {
        let resp = self
            .client
            .post(format!("{}/v1/consolidate", self.base_url))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn forget(&self) -> ClientResult<ForgetResponse> {
        let resp = self
            .client
            .post(format!("{}/v1/forget", self.base_url))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn relate(
        &self,
        source_id: &str,
        target_id: &str,
        relation: Option<&str>,
        weight: Option<f64>,
    ) -> ClientResult<()> {
        let resp = self
            .client
            .post(format!("{}/v1/relate", self.base_url))
            .json(&serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": relation,
                "weight": weight,
            }))
            .send()
            .await?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::CREATED {
            Ok(())
        } else {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let error = &body["error"];
            Err(ClientError::Api {
                code: error["code"].as_str().unwrap_or("UNKNOWN").into(),
                message: error["message"].as_str().unwrap_or("Unknown error").into(),
            })
        }
    }

    async fn graph(&self, id: &str) -> ClientResult<GraphResponse> {
        let resp = self
            .client
            .get(format!("{}/v1/graph/{id}", self.base_url))
            .send()
            .await?;
        self.handle_response(resp).await
    }

    async fn health(&self) -> ClientResult<String> {
        let resp = self
            .client
            .get(format!("{}/v1/health", self.base_url))
            .send()
            .await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(body["status"].as_str().unwrap_or("unknown").to_string())
    }
}
