use merkur_core::{Embedder, NewEdge, NewMemory, Storage};
use merkur_embedders::NoopEmbedder;
use merkur_storage::SqliteStorage;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
struct MerkurMcp {
    storage: Arc<dyn Storage>,
    embedder: Arc<dyn Embedder>,
    tool_router: ToolRouter<Self>,
}

impl MerkurMcp {
    fn new(storage: Arc<dyn Storage>, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            storage,
            embedder,
            tool_router: Self::tool_router(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct WriteArgs {
    /// Memory content text
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SearchArgs {
    /// Search query text
    query: String,
    /// Max results (default 10)
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct IdArgs {
    /// Memory ID
    id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct RelateArgs {
    /// Source memory ID
    source_id: String,
    /// Target memory ID
    target_id: String,
    /// Relationship label
    relation: Option<String>,
}

#[tool_router(router = tool_router)]
impl MerkurMcp {
    #[tool(description = "Write a new memory to MerkurDB")]
    async fn write_memory(&self, args: Parameters<WriteArgs>) -> String {
        let args = args.0;
        let embedding = match self.embedder.encode(&args.content).await {
            Ok(e) => Some(e),
            Err(e) => return format!("Embedding error: {e}"),
        };
        let mem = NewMemory {
            content: args.content,
            category: None,
            context: Default::default(),
            metadata: Default::default(),
            embedding,
        };
        match self.storage.insert_memory(&mem).await {
            Ok(id) => format!(r#"{{"id":"{id}","status":"ok"}}"#),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Search memories by semantic similarity")]
    async fn search_memory(&self, args: Parameters<SearchArgs>) -> String {
        let args = args.0;
        let vec = match self.embedder.encode(&args.query).await {
            Ok(v) => v,
            Err(e) => return format!("Embedding error: {e}"),
        };
        match self.storage.vector_search(&vec, args.limit).await {
            Ok(results) => {
                serde_json::to_string_pretty(&results).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Get a memory by ID")]
    async fn get_memory(&self, args: Parameters<IdArgs>) -> String {
        let args = args.0;
        match self.storage.get_memory(&args.id).await {
            Ok(Some(m)) => {
                serde_json::to_string_pretty(&m).unwrap_or_else(|e| format!("Error: {e}"))
            }
            Ok(None) => format!("Memory {} not found", args.id),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Delete a memory by ID")]
    async fn delete_memory(&self, args: Parameters<IdArgs>) -> String {
        let args = args.0;
        match self.storage.delete_memory(&args.id).await {
            Ok(()) => format!(r#"{{"status":"deleted","id":"{}"}}"#, args.id),
            Err(e) => format!("Error: {e}"),
        }
    }

    #[tool(description = "Create an edge between two memories")]
    async fn relate(&self, args: Parameters<RelateArgs>) -> String {
        let args = args.0;
        let edge = NewEdge {
            source_id: args.source_id,
            target_id: args.target_id,
            weight: Some(1.0),
            relation: args.relation,
            edge_type: merkur_core::EdgeType::Manual,
        };
        match self.storage.insert_edge(&edge).await {
            Ok(()) => r#"{"status":"edge_created"}"#.to_string(),
            Err(e) => format!("Error: {e}"),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MerkurMcp {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();

    let db_path =
        std::env::var("MERKUR_DB_PATH").unwrap_or_else(|_| "~/.merkur/data/merkur.db".to_string());
    let dim: usize = std::env::var("MERKUR_EMBED_DIM")
        .unwrap_or_else(|_| "384".to_string())
        .parse()
        .unwrap_or(384);

    let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::new(&db_path, dim)?);
    let embedder: Arc<dyn Embedder> = Arc::new(NoopEmbedder::new(dim));

    let server = MerkurMcp::new(storage, embedder);
    let transport = rmcp::transport::io::stdio();
    let handle = server.serve(transport).await?;
    let _ = handle.waiting().await;

    Ok(())
}
