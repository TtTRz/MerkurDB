use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "merkurctl", about = "MerkurDB admin CLI")]
struct Cli {
    /// Server base URL
    #[arg(long, default_value = "http://localhost:1934", env = "MERKUR_URL")]
    url: String,

    /// Bearer token for authentication
    #[arg(long, env = "MERKUR_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check server health
    Health,
    /// Show storage stats
    Status,
    /// Trigger consolidation
    Consolidate,
    /// Trigger forgetting evaluation
    Forget,
    /// Search memories
    Search {
        /// Query text
        query: String,
        /// Search mode (fast|deep)
        #[arg(short, long, default_value = "fast")]
        mode: String,
        /// Max results
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
    /// Write a memory
    Write {
        /// Memory content
        content: String,
    },
    /// Delete a memory by id
    Delete {
        /// Memory ID
        id: String,
    },
    /// Get graph neighborhood
    Graph {
        /// Center memory ID
        id: String,
        /// BFS depth
        #[arg(short, long, default_value = "2")]
        depth: usize,
    },
    /// Run database migrations
    Migrate,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let base = cli.url.trim_end_matches('/').to_string();

    let resp = match &cli.command {
        Commands::Health => send(&client, &base, &cli.token, "GET", "/v1/health", None).await?,
        Commands::Status => send(&client, &base, &cli.token, "GET", "/v1/status", None).await?,
        Commands::Consolidate => {
            send(&client, &base, &cli.token, "POST", "/v1/consolidate", None).await?
        }
        Commands::Forget => send(&client, &base, &cli.token, "POST", "/v1/forget", None).await?,
        Commands::Search { query, mode, limit } => {
            let path = format!(
                "/v1/search?q={}&mode={}&limit={}",
                urlenc(query),
                mode,
                limit
            );
            send(&client, &base, &cli.token, "GET", &path, None).await?
        }
        Commands::Write { content } => {
            let body = serde_json::json!({"content": content});
            send(&client, &base, &cli.token, "POST", "/v1/write", Some(body)).await?
        }
        Commands::Delete { id } => {
            let path = format!("/v1/memory/{id}");
            send(&client, &base, &cli.token, "DELETE", &path, None).await?
        }
        Commands::Graph { id, depth } => {
            let path = format!("/v1/graph/{id}?depth={depth}");
            send(&client, &base, &cli.token, "GET", &path, None).await?
        }
        Commands::Migrate => {
            eprintln!("Migration runs automatically on server startup.");
            eprintln!("To force-migrate, restart the server.");
            std::process::exit(0);
        }
    };

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    let pretty = serde_json::to_string_pretty(&body)?;

    if status.is_success() {
        println!("{pretty}");
    } else {
        eprintln!("HTTP {status}\n{pretty}");
        std::process::exit(1);
    }

    Ok(())
}

fn urlenc(s: &str) -> String {
    s.replace(' ', "+").replace('&', "%26").replace('=', "%3D")
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &Option<String>,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<reqwest::Response, Box<dyn std::error::Error>> {
    let url = format!("{base}{path}");
    let mut req = match method {
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").json(&b);
    }
    Ok(req.send().await?)
}
