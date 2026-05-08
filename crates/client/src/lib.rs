//! Async HTTP client SDK for MerkurDB.

use async_trait::async_trait;
use merkur_core::{Edge, Memory, ScoredMemory, WriteItem};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("API error: {code} — {message}")]
    Api { code: String, message: String },
}

impl From<reqwest::Error> for ClientError {
    fn from(e: reqwest::Error) -> Self {
        // Strip the URL out of reqwest errors so credentials embedded in URLs
        // don't leak to consumers of the SDK.
        ClientError::Http(format!("{e}"))
    }
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
pub struct GraphResponse {
    pub center: String,
    pub neighborhood: Vec<serde_json::Value>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Default, Clone)]
pub struct SearchParams {
    pub mode: Option<String>,
    pub limit: Option<usize>,
    pub score_threshold: Option<f64>,
    pub depth: Option<usize>,
    pub degree_limit: Option<usize>,
    pub offset: Option<usize>,
    pub level: Option<String>,
    pub category: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_graph: Option<bool>,
    pub context: Option<HashMap<String, String>>,
}

// ── Trait ──

#[async_trait]
pub trait MerkurClient: Send + Sync {
    async fn write(
        &self,
        content: &str,
        context: Option<HashMap<String, String>>,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> ClientResult<WriteResponse>;

    async fn write_batch(&self, items: &[WriteItem]) -> ClientResult<WriteBatchResponse>;

    async fn search(&self, query: &str, params: &SearchParams) -> ClientResult<SearchResponse>;

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

    async fn graph(&self, id: &str, depth: Option<usize>) -> ClientResult<GraphResponse>;

    async fn health(&self) -> ClientResult<String>;
}

// ── HTTP implementation ──

pub struct HttpMerkurClient {
    client: reqwest::Client,
    base_url: String,
    bearer: Option<String>,
}

impl HttpMerkurClient {
    /// Build a client against `base_url` with a 30-second default timeout
    /// and no bearer token.
    pub fn new(base_url: &str) -> ClientResult<Self> {
        Self::with_options(base_url, None, Duration::from_secs(30))
    }

    /// Build a client that authenticates with `Authorization: Bearer <token>`.
    pub fn with_token(base_url: &str, token: impl Into<String>) -> ClientResult<Self> {
        Self::with_options(base_url, Some(token.into()), Duration::from_secs(30))
    }

    /// Low-level constructor. Propagates any `reqwest::Client::build` failure
    /// (TLS initialization, etc.) as a `ClientError::Http`.
    pub fn with_options(
        base_url: &str,
        bearer: Option<String>,
        timeout: Duration,
    ) -> ClientResult<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            bearer,
        })
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.bearer {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn parse_or_error(resp: reqwest::Response) -> ClientResult<serde_json::Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let body: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if status.is_success() {
            Ok(body)
        } else {
            let error = &body["error"];
            let code = error["code"].as_str().unwrap_or("UNKNOWN").into();
            let message = error["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "HTTP {status}: {}",
                        text.chars().take(200).collect::<String>()
                    )
                });
            Err(ClientError::Api { code, message })
        }
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> ClientResult<T> {
        let value = Self::parse_or_error(resp).await?;
        serde_json::from_value(value).map_err(|e| ClientError::Http(format!("decode: {e}")))
    }

    async fn handle_unit(resp: reqwest::Response) -> ClientResult<()> {
        Self::parse_or_error(resp).await.map(|_| ())
    }
}

#[derive(Debug, Serialize)]
struct WriteBody<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<HashMap<String, serde_json::Value>>,
}

#[async_trait]
impl MerkurClient for HttpMerkurClient {
    async fn write(
        &self,
        content: &str,
        context: Option<HashMap<String, String>>,
        metadata: Option<HashMap<String, serde_json::Value>>,
    ) -> ClientResult<WriteResponse> {
        let req = self
            .client
            .post(format!("{}/v1/write", self.base_url))
            .json(&WriteBody {
                content,
                context,
                metadata,
            });
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn write_batch(&self, items: &[WriteItem]) -> ClientResult<WriteBatchResponse> {
        let req = self
            .client
            .post(format!("{}/v1/write-batch", self.base_url))
            .json(&serde_json::json!({ "items": items }));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn search(&self, query: &str, params: &SearchParams) -> ClientResult<SearchResponse> {
        let mut q: Vec<(&str, String)> = vec![("q", query.to_string())];
        if let Some(m) = &params.mode {
            q.push(("mode", m.clone()));
        }
        if let Some(v) = params.limit {
            q.push(("limit", v.to_string()));
        }
        if let Some(v) = params.score_threshold {
            q.push(("score_threshold", v.to_string()));
        }
        if let Some(v) = params.depth {
            q.push(("depth", v.to_string()));
        }
        if let Some(v) = params.degree_limit {
            q.push(("degree_limit", v.to_string()));
        }
        if let Some(v) = params.offset {
            q.push(("offset", v.to_string()));
        }
        if let Some(v) = &params.level {
            q.push(("level", v.clone()));
        }
        if let Some(v) = &params.category {
            q.push(("category", v.clone()));
        }
        if let Some(v) = &params.from {
            q.push(("from", v.clone()));
        }
        if let Some(v) = &params.to {
            q.push(("to", v.clone()));
        }
        if let Some(v) = params.include_graph {
            q.push(("include_graph", v.to_string()));
        }
        if let Some(ctx) = &params.context
            && let Ok(s) = serde_json::to_string(ctx)
        {
            q.push(("context", s));
        }
        let req = self
            .client
            .get(format!("{}/v1/search", self.base_url))
            .query(&q);
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn get_memory(&self, id: &str) -> ClientResult<Memory> {
        let req = self.client.get(format!("{}/v1/memory/{id}", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn update_memory(&self, id: &str, content: &str) -> ClientResult<()> {
        let req = self
            .client
            .put(format!("{}/v1/memory/{id}", self.base_url))
            .json(&serde_json::json!({ "content": content }));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_unit(resp).await
    }

    async fn delete_memory(&self, id: &str) -> ClientResult<()> {
        let req = self
            .client
            .delete(format!("{}/v1/memory/{id}", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_unit(resp).await
    }

    async fn status(&self) -> ClientResult<StatusResponse> {
        let req = self.client.get(format!("{}/v1/status", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn consolidate(&self) -> ClientResult<ConsolidateResponse> {
        let req = self
            .client
            .post(format!("{}/v1/consolidate", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn forget(&self) -> ClientResult<ForgetResponse> {
        let req = self.client.post(format!("{}/v1/forget", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn relate(
        &self,
        source_id: &str,
        target_id: &str,
        relation: Option<&str>,
        weight: Option<f64>,
    ) -> ClientResult<()> {
        let req = self
            .client
            .post(format!("{}/v1/relate", self.base_url))
            .json(&serde_json::json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": relation,
                "weight": weight,
            }));
        let resp = self.apply_auth(req).send().await?;
        Self::handle_unit(resp).await
    }

    async fn graph(&self, id: &str, depth: Option<usize>) -> ClientResult<GraphResponse> {
        let mut req = self.client.get(format!("{}/v1/graph/{id}", self.base_url));
        if let Some(d) = depth {
            req = req.query(&[("depth", d.to_string())]);
        }
        let resp = self.apply_auth(req).send().await?;
        Self::handle_response(resp).await
    }

    async fn health(&self) -> ClientResult<String> {
        let req = self.client.get(format!("{}/v1/health", self.base_url));
        let resp = self.apply_auth(req).send().await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(body["status"].as_str().unwrap_or("unknown").to_string())
    }
}
