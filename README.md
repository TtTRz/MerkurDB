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

# Search (hybrid BM25 x vector fusion — the default)
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc'

# Low-latency vector-only search
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8+gc&mode=fast'

# Graph diffusion search
curl -H "Authorization: Bearer $MERKUR_TOKEN" \
  'localhost:1934/v1/search?q=v8&mode=deep&depth=2&include_graph=true'

# Health (no auth required)
curl localhost:1934/v1/health
```

## Key Features

- **Hybrid Retrieval (default)**: FTS5 trigram full-text (BM25) x vector cosine, fused with Reciprocal Rank Fusion; results re-ranked by a composite of relevance, stored weight, and **system-learned importance** (Consolidator-assessed, never client-reported). Fusion knobs (`retrieval.fusion.*`) are configurable. Works on CJK/unsegmented text out of the box; see [Hybrid Search](#hybrid-search)
- **Evaluated in the open**: reproducible LoCoMo + PersonaMem harness in `crates/eval` with per-question dumps; see [Evaluation](#evaluation)
- **Fast & Deep modes**: `mode=fast` for pure vector top-k, `mode=deep` for BFS graph diffusion via SQLite CTE
- **Ebbinghaus Forgetting Curve**: Exponential weight decay, access boost, cascade downgrade (Full→Summary→Title→Archive) with hysteresis-based promotion back up on repeated retrieval
- **Write Governance (mem0-style)**: near-duplicate writes NOOP onto the existing memory (top-1 cosine ≥ 0.92, same bucket); the async Consolidator adjudicates each new memory against its neighbors — UPDATE absorbs the new content into the existing row (salience, edges, and access history preserved; audit pointer kept), DELETE soft-invalidates the loser. Verdicts execute only with an LLM consolidator AND pair similarity ≥ `consolidation.adjudication_floor` (dual-signal); the synchronous write path stays LLM-free
- **Soft-Invalidation & Retention**: adjudicated-out memories vanish from every retrieval channel immediately but stay auditable via `GET /v1/memory/{id}` until `forgetting.purge_invalidated_days` (30d) hard-deletes them; client `DELETE` stays an immediate hard delete
- **Context Assembly**: `POST /v1/context` packs a token-budgeted, deduplicated, prompt-ready digest from hybrid recall — the MCP-friendly entry point
- **Offline Consolidation**: LLM-driven summarization, entity extraction, and automatic edge creation
- **Logical Namespaces**: `X-Merkur-Namespace` header scopes writes & all search modes to one bucket; hybrid retrieval stays isolated per bucket. Logical isolation, not a security boundary
- **Plugin Architecture**: Embedder / Storage / Consolidator / Forgetter — independently replaceable via trait + config injection
- **Dual Storage**: SQLite (default) + LanceDB disk-based index (feature gated)
- **Rust SDK**: `merkur-client` crate with `MerkurClient` trait and `HttpMerkurClient`
- **OpenAPI 3.0**: Multi-language SDK code generation

## Hybrid Search

`/v1/search` runs two channels in parallel and fuses them with Reciprocal Rank Fusion (`k = 60`, the standard value across retrieval systems):

| Channel | Engine | Strength |
|---|---|---|
| BM25 full-text | SQLite FTS5, trigram tokenizer | Exact terms, code identifiers, CJK substrings |
| Vector cosine | In-memory index (or LanceDB) | Paraphrases, semantic similarity |

Design properties:

- **Default mode.** `mode=hybrid` is implied; `fast` and `deep` remain available as explicit opt-outs.
- **Normalized scores.** Fused scores are scaled to `(0, 1]` by the theoretical maximum (rank-1 in both channels = `1.0`; single-channel hits cap at ~`0.5`). `score_threshold` gates this fused relevance in hybrid mode and raw cosine in fast mode — one meaning across modes.
- **CJK-ready.** The trigram tokenizer indexes every 3-character sliding window, so Chinese/Japanese/Korean queries match without a word segmentation dependency.
- **Short-query fallback.** Queries under 3 characters cannot produce a trigram; the BM25 channel yields no candidates and vector similarity covers those queries alone.
- **Always-on consistency.** FTS5 triggers mirror every insert/update/delete — including writes from the LanceDB backend and admin tools — so both channels always see the same data.
- **Tunable fusion.** `retrieval.fusion.rrf_k` (rank smoothing), `bm25_weight`/`vector_weight` (channel shares), and `score_search`/`score_weight`/`score_importance` (composite re-rank shares) are configurable per deployment; the shipped defaults are validated against the public benchmarks below.

## Evaluation

The `merkur-eval` harness (`crates/eval`, MIT) runs two public benchmarks end-to-end against a live server — the same serving path real clients use — and writes per-question JSONL dumps so every number is auditable.

| Benchmark | Metric | MerkurDB | Reference points |
|---|---|---|---|
| LoCoMo (1,986 QA) | QA accuracy (LLM-judged) | **64.8%** | mem0 paper 66.9% (GPT-4-class answerer + full extraction pipeline) |
| LoCoMo | retrieval hit@30 / coverage | **0.762 / 0.703** | — |
| PersonaMem 32k (589 MC QA) | accuracy | **73.2%** | frontier LLMs full-context ~52%; TencentDB Agent Memory 76.1% (same answer model, full pipeline) |

Measured with raw dialog-turn ingest (no consolidation pipeline) and lightweight answer models (`deepseek-v4-flash-vision-exp` judge on LoCoMo, `kimi-k2.5` on PersonaMem); judge/answer-model choices make cross-paper numbers approximate. Harness design: LLM-free retrieval-recall track scored against LoCoMo evidence annotations, judge-graded QA track (adversarial questions score abstention as correct), and in-situ checkpoint replay for PersonaMem (no future-turn leakage).

```bash
scripts/fetch_locomo.sh          # datasets (CC BY-NC / MIT, gitignored)
scripts/run_locomo.sh            # ingest + recall + qa, throwaway server
scripts/run_personamem.sh        # in-situ PersonaMem replay
scripts/sweep_fusion.sh          # P1-5 fusion-parameter sweep (persistent corpus)
```

## API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/health` | Health check |
| `POST` | `/v1/write` | Write a memory |
| `POST` | `/v1/write-batch` | Batch write |
| `GET` | `/v1/search` | Search (level/category/date filtering; `X-Merkur-Namespace` header scopes to one bucket) |
| `POST` | `/v1/context` | Token-budget context assembly (digest + items; namespace-aware) |
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
| `search_memory` | Hybrid BM25 + vector relevance search |
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

#### v0.5.0 (unreleased) — Retrieval hardening & write governance

| Category | Feature |
|----------|---------|
| Governance | Consolidator adjudication (mem0-style UPDATE/DELETE) with dual-signal gate; UPDATE absorbs in place, DELETE soft-invalidates |
| Schema | Migration v5: `valid_at` (lazy, backfilled) + `invalid_at`; every retrieval channel filters invalidated rows |
| Retention | `purge_invalidated_days` audit window; `/v1/forget` reports `purged` |
| Correctness | Per-version transactional, replay-safe migrations; LanceDB backend migrates |
| Retrieval | Iterative-deepening namespace vector search; hybrid pagination headroom; hydration degradation |
| Scoring | Threshold gates fused relevance in hybrid mode |
| Signal | Access recorded at serving points only (`record_access`); probes are pure |
| Isolation | BFS filters cross-bucket hops; graph endpoints return induced subgraphs |
| SDK | Client `with_namespace`; `ForgetResponse.upgraded/purged`; full-record `GET /v1/memory/{id}` |

### Planned (v0.6.0+)

| Priority | Feature | Description |
|----------|---------|-------------|
| P2 | At-rest encryption | SQLCipher or app-layer embedding column encryption |
| P2 | PostgreSQL backend | PG storage backend via Storage trait |
| P2 | Public evaluation | LoCoMo benchmark harness for retrieval/scoring weight tuning |
| P3 | Multi-modal | Image embedding support (CLIP, etc.) |

## Documentation

- [SPEC.md](docs/SPEC.md) — Design philosophy, cognitive science background, product roadmap
- [ARCHITECTURE.md](docs/ARCHITECTURE.md) — Technical architecture, data model, API spec
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 specification
- [CHANGELOG.md](CHANGELOG.md) — Change log

## License

MIT
