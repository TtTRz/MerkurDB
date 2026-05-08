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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    let path = match &cli.command {
        Commands::Health => "/v1/health",
        Commands::Status => "/v1/status",
        Commands::Consolidate => "/v1/consolidate",
        Commands::Forget => "/v1/forget",
    };

    let url = format!("{}{}", cli.url.trim_end_matches('/'), path);
    let mut req = match &cli.command {
        Commands::Health | Commands::Status => client.get(&url),
        Commands::Consolidate | Commands::Forget => client.post(&url),
    };

    if let Some(ref token) = cli.token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }

    let resp = req.send().await?;
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
