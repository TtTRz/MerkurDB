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

### Completed (v0.1.0 → v0.3.0)

| Category | Feature |
|----------|---------|
| Core | Type system (Memory, Edge, MemoryLevel), 4 plugin traits, MerkurError |
| Storage | SqliteStorage (WAL + r2d2), InMemoryVectorIndex (cosine similarity) |
| Storage | LanceDbStorage (disk-based IVF-PQ, feature gated) |
| Embedders | NoopEmbedder (test), OllamaEmbedder, OpenAIEmbedder (feature gated) |
| Retrieval | S1 Fast (vector top-k), S2 Deep (CTE BFS graph diffusion) |
| Consolidation | NoopConsolidator, LlmConsolidator (LLM summary + entity extraction) |
| Forgetting | EbbinghausForgetter (decay + access boost + cascade downgrade) |
| Scheduler | Background consolidation + forgetting loop, manual triggers |
| API | 14 REST endpoints, CORS, graceful shutdown (SIGTERM) |
| SDK | `merkur-client` crate: MerkurClient trait + HttpMerkurClient |
| DevOps | Docker, GitHub Actions CI, OpenAPI 3.0 spec |
| Docs | README + ARCHITECTURE + SPEC + config example |

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
