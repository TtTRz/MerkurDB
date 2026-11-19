use axum::{Json, Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
};

async fn send(input: &mut ChildStdin, message: Value) {
    input
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
}

async fn response(lines: &mut Lines<BufReader<ChildStdout>>, id: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("MCP stdout closed");
            let message: Value =
                serde_json::from_str(&line).expect("stdout must contain only JSON-RPC");
            if message["id"] == id {
                return message;
            }
        }
    })
    .await
    .expect("MCP response timed out")
}

#[tokio::test]
async fn stdio_discovers_and_calls_budgeted_context_over_http() {
    let app = Router::new().route("/v1/context", post(
        |headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer integration-token");
            assert_eq!(headers["x-merkur-namespace"], "project-a");
            assert_eq!(body["q"], "release");
            assert_eq!(body["token_budget"], 100);
            Json(json!({
                "digest": "- Release Friday",
                "items": [{"id":"source-1","content":"Release Friday","score":0.9,"weight":1.0,"importance":0.5,"namespace":"project-a"}],
                "token_estimate": 4, "dropped": 0
            }))
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut child = Command::new(env!("CARGO_BIN_EXE_merkur-mcp"))
        .env_remove("MERKUR_DB_PATH")
        .env_remove("MERKUR_EMBED_DIM")
        .env("MERKUR_URL", url)
        .env("MERKUR_TOKEN", "integration-token")
        .env("MERKUR_NAMESPACE", "project-a")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(&mut input, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}
    }})).await;
    assert!(response(&mut lines, 1).await["result"]["capabilities"]["tools"].is_object());
    send(
        &mut input,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = response(&mut lines, 2).await;
    let names: Vec<_> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for name in [
        "write_memory",
        "search_memory",
        "get_memory",
        "delete_memory",
        "relate",
        "assemble_context",
    ] {
        assert!(names.contains(&name), "missing {name}");
    }
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"assemble_context","arguments":{"query":"release","token_budget":100}
        }}),
    )
    .await;
    let result = response(&mut lines, 3).await;
    let payload: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(payload["items"][0]["id"], "source-1");
    assert_eq!(payload["digest"], "- Release Friday");
    child.kill().await.unwrap();
    server.abort();
}
