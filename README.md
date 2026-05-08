# MerkurDB

[![CI](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.92+-orange.svg)](rust-toolchain.toml)

> [中文文档](README_CN.md)

A standalone cognitive memory service for AI agents. Inspired by neuroscience, built in Rust.

Single binary, zero runtime dependencies. Supports semantic search, graph diffusion, memory consolidation, and Ebbinghaus forgetting curves.

> Design philosophy: [SPEC.md](docs/SPEC.md) · Technical architecture: [ARCHITECTURE.md](docs/ARCHITECTURE.md)

## Quick Start

```bash
# Start the server (NoopEmbedder + SQLite)
cargo run --release -p merkur-server -- --config config.example.yaml

# Set your bearer token (must match config.example.yaml auth.tokens)
export MERKUR_TOKEN='replace-me-with-a-strong-token'

# Write a memory
curl -X POST localhost:1934/v1/write \
  -H "Authorization: Bearer $MERKUR_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"content":"v8 GC is generational","context":{"agent":"assistant"}}'

# Search
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc&mode=fast'

# Graph diffusion search
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8&mode=deep&depth=2&include_graph=true'

# Health (no auth required)
curl localhost:1934/v1/health
```

## Key Features

- **Dual Retrieval**: S1 Fast (vector top-k) + S2 Deep (BFS graph diffusion via SQLite CTE)
- **Ebbinghaus Forgetting Curve**: Exponential weight decay, access boost, cascade downgrade (Full→Summary→Title→Archive)
- **Offline Consolidation**: LLM-driven summarization, entity extraction, and automatic edge creation
- **Plugin Architecture**: Embedder / Storage / Consolidator / Forgetter — independently replaceable via trait + config injection
- **Dual Storage**: SQLite (default) + LanceDB disk-based index (feature gated)
- **Rust SDK**: `merkur-client` crate with `MerkurClient` trait and `HttpMerkurClient`
- **OpenAPI 3.0**: Multi-language SDK code generation

## API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/health` | Health check |
| `POST` | `/v1/write` | Write a memory |
| `POST` | `/v1/write-batch` | Batch write |
| `GET` | `/v1/search` | Search (level/category/date filtering) |
| `GET` | `/v1/memory/{id}` | Get memory details |
| `PUT` | `/v1/memory/{id}` | Update (auto re-embed) |
| `DELETE` | `/v1/memory/{id}` | Delete (cascade edges + tags) |
| `GET` | `/v1/status` | Storage stats + uptime |
| `POST` | `/v1/consolidate` | Trigger consolidation |
| `GET` | `/v1/consolidate/log` | Consolidation audit log |
| `POST` | `/v1/forget` | Trigger forgetting |
| `POST` | `/v1/relate` | Create edge |
| `POST` | `/v1/relate-batch` | Batch create edges |
| `GET` | `/v1/graph/{id}` | Graph neighborhood with edges |

## Docker

```bash
docker build -t merkurdb .
docker run -p 1934:1934 -v ./data:/var/lib/merkur/data merkurdb
```

## MCP Integration

`merkur-mcp` exposes MerkurDB as a Model Context Protocol server over stdio. AI assistants (Claude Desktop, Cursor, etc.) can directly read/write memories.

```bash
# Build
cargo build --release -p merkur-mcp

# Run standalone (uses NoopEmbedder by default)
MERKUR_DB_PATH=~/.merkur/data/merkur.db merkur-mcp
```

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_DB_PATH": "~/.merkur/data/merkur.db"
      }
    }
  }
}
```

### Cursor

Add to `.cursor/mcp.json` in your project:

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_DB_PATH": "~/.merkur/data/merkur.db"
      }
    }
  }
}
```

### Available Tools

| Tool | Description |
|------|-------------|
| `write_memory` | Write a new memory |
| `search_memory` | Semantic similarity search |
| `get_memory` | Get memory by ID |
| `delete_memory` | Delete memory by ID |
| `relate` | Create edge between memories |

## Development

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-features -- -D warnings

# Feature gates
cargo build --features openai,lancedb
```

## Project Structure

```
crates/
├── core/              # Types + traits + errors
├── storage/           # SQLite + LanceDB backends
├── embedders/         # Noop / Ollama / OpenAI
├── consolidators/     # Noop / LLM
├── forgetters/        # Ebbinghaus
├── server/            # HTTP server + scheduler
└── client/            # Rust SDK
```

## Roadmap

### Completed

#### v0.1.0 — Foundation

| Category | Feature |
|----------|---------|
| Core | Type system (Memory, Edge, MemoryLevel), 4 plugin traits, MerkurError |
| Storage | SqliteStorage (WAL + r2d2), InMemoryVectorIndex (cosine similarity) |
| Storage | LanceDbStorage (disk-based vector search, feature gated) |
| Embedders | NoopEmbedder, OllamaEmbedder, OpenAIEmbedder (feature gated) |
| Retrieval | S1 Fast (vector top-k), S2 Deep (CTE BFS graph diffusion) |
| Consolidation | NoopConsolidator, LlmConsolidator (LLM summary + edge creation) |
| Forgetting | EbbinghausForgetter (exponential decay + access boost + cascade) |
| Server | 14 REST endpoints, CORS, Scheduler, graceful shutdown |
| SDK | `merkur-client` crate, OpenAPI 3.0 spec |
| DevOps | Docker, GitHub Actions CI |

#### v0.2.0 — Hardening

| Category | Feature |
|----------|---------|
| Security | Bearer-token auth middleware, constant-time comparison |
| Safety | `foreign_keys=ON` per-connection, `spawn_blocking` for all SQLite |
| Correctness | Ebbinghaus formula fixed (true half-life), BFS cycle detection |
| Performance | Bounded min-heap top-k, batch `json_each` queries |
| Config | Figment multi-layer merge, runtime validation |
| API | Structured error responses, request body limit (10 MiB) |

#### v0.3.0 — Performance & Reliability

| Category | Feature |
|----------|---------|
| Critical fix | Consolidation no longer marks failed memories as complete |
| Performance | N+1 eliminated in 5 hot paths (bfs, write_batch, search, graph, relate) |
| Performance | Pre-cached L2 norms in vector index, LanceDB auto-index at 256 rows |
| Security | `subtle` crate for constant-time token comparison |
| API | `write_batch` returns 207 on full failure, context boost before threshold |
| Cleanup | Dead code removed (Timeout/Unauthorized variants, rebuild_vector_index) |
| Docs | Mermaid diagrams (crate deps, retrieval flow, lifecycle, consolidation) |

### Planned (v0.4.0+)

| Priority | Feature | Description |
|----------|---------|-------------|
| P1 | MCP adapter | Model Context Protocol integration for Agent direct access |
| P1 | gRPC API | `tonic`-based high-performance streaming API alongside REST |
| P2 | At-rest encryption | SQLCipher or app-layer embedding column encryption |
| P2 | DB migration tool | Schema versioning, `merkur migrate` CLI |
| P2 | PostgreSQL backend | PG storage backend via Storage trait |
| P2 | Rust CLI | `merkurctl` — admin operations (trigger consolidate, query status, backup) |
| P3 | Web Dashboard | Tauri/Yew SPA — memory graph visualization, config editor |
| P3 | Multi-modal | Image embedding support (CLIP, etc.) |
| P3 | Distributed consolidation | Multi-worker parallel consolidation for large memory bases |

## Documentation

- [SPEC.md](docs/SPEC.md) — Design philosophy, cognitive science background, product roadmap
- [ARCHITECTURE.md](docs/ARCHITECTURE.md) — Technical architecture, data model, API spec
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 specification
- [CHANGELOG.md](CHANGELOG.md) — Change log

## License

MIT
