# MerkurDB

[![CI](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml/badge.svg)](https://github.com/TtTRz/MerkurDB/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](rust-toolchain.toml)

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

# Web console: open http://localhost:1934/ui in a browser and enter your token
```

## Key Features

- **Hybrid Retrieval (default)**: FTS5 trigram full-text (BM25) x vector cosine, fused with Reciprocal Rank Fusion; results re-ranked by a composite of relevance, stored weight, and **system-learned importance** (Consolidator-assessed, never client-reported). Fusion knobs (`retrieval.fusion.*`) are configurable. Works on CJK/unsegmented text out of the box; see [Hybrid Search](#hybrid-search)
- **Evaluated in the open**: reproducible LoCoMo + PersonaMem harness in `crates/eval` with per-question dumps; see [Evaluation](#evaluation)
- **Fast & Deep modes**: `mode=fast` for pure vector top-k, `mode=deep` serves vector seeds plus BFS graph diffusion anchored to seed relevance (SQLite CTE)
- **Ebbinghaus Forgetting Curve**: Exponential weight decay, access boost, cascade downgrade (Full→Summary→Title→Archive) with hysteresis-based promotion back up on repeated retrieval
- **Write Governance (mem0-style)**: near-duplicate writes NOOP onto the existing memory (top-1 cosine ≥ 0.92, same bucket); the async Consolidator adjudicates each new memory against its neighbors — UPDATE absorbs the new content into the existing row (salience, edges, and access history preserved; audit pointer kept), DELETE soft-invalidates the loser. Verdicts execute only with an LLM consolidator AND pair similarity ≥ `consolidation.adjudication_floor` (dual-signal); the synchronous write path stays LLM-free
- **Soft-Invalidation & Retention**: adjudicated-out memories vanish from every retrieval channel immediately but stay auditable via `GET /v1/memory/{id}` until `forgetting.purge_invalidated_days` (30d) hard-deletes them; client `DELETE` stays an immediate hard delete
- **Context Assembly**: `POST /v1/context` and MCP `assemble_context` return a deduplicated digest with source IDs, packed within an estimated token budget
- **Offline Consolidation**: LLM-driven summarization, entity extraction, and automatic edge creation
- **Logical Namespaces**: `X-Merkur-Namespace` header scopes writes & all search modes to one bucket; hybrid retrieval stays isolated per bucket. Logical isolation, not a security boundary
- **Embedded Web Console**: read-only observability UI served by the server itself at `/ui` (assets compiled into the binary, no separate deployment) — status dashboard with level/namespace distribution, memory browse + detail, graph neighborhood explorer, consolidation audit log. Pages are public; data flows through the authenticated API
- **Plugin Architecture**: Embedder / Storage / Consolidator / Forgetter — independently replaceable via trait + config injection
- **Dual Storage**: SQLite (default) + LanceDB disk-based Cosine index (feature gated), with full-vector score refinement for governance thresholds
- **Vector maintenance**: SQLite vector caches refresh across handles with bounded change tracking; LanceDB orphan cleanup fences delayed publications during forgetting
- **Rust SDK**: `merkur-client` crate with `MerkurClient` trait and `HttpMerkurClient`
- **OpenAPI 3.0**: Multi-language SDK code generation

## Hybrid Search

`/v1/search` retrieves candidates from two channels and fuses them with Reciprocal Rank Fusion (default `k = 60`):

| Channel | Engine | Strength |
|---|---|---|
| BM25 full-text | SQLite FTS5, trigram tokenizer | Exact terms, code identifiers, CJK substrings |
| Vector cosine | In-memory index (or LanceDB) | Paraphrases, semantic similarity |

Design properties:

- **Default mode.** `mode=hybrid` is implied; `fast` and `deep` remain available as explicit opt-outs.
- **Normalized scores.** Fused scores are scaled to `(0, 1]` by the theoretical maximum (rank-1 in both channels = `1.0`; a hit in only one channel caps at `0.5` with equal channel weights). Hybrid `score_threshold` gates fused relevance; the returned score includes composite re-ranking. Fast mode returns and gates raw cosine.
- **CJK-ready.** The trigram tokenizer indexes every 3-character sliding window, so Chinese/Japanese/Korean queries match without a word segmentation dependency.
- **Short-query fallback.** Queries under 3 characters cannot produce a trigram; the BM25 channel yields no candidates and vector similarity covers those queries alone.
- **FTS consistency.** Triggers mirror memory inserts, updates and deletes in the same SQLite transaction, including LanceDB metadata and admin-tool writes.
- **Tunable fusion.** `retrieval.fusion.rrf_k` (rank smoothing), `bm25_weight`/`vector_weight` (channel shares), and `score_search`/`score_weight`/`score_importance` (composite re-rank shares) are configurable per deployment; the shipped defaults are validated against the public benchmarks below.

## Evaluation

The `merkur-eval` harness (`crates/eval`, MIT) runs two public benchmarks end-to-end against a live server — the same serving path real clients use — and writes per-question JSONL dumps so every number is auditable.

| Benchmark | Metric | Raw ingest | Consolidation enabled |
|---|---|---|---|
| LoCoMo (1,986 questions) | QA accuracy @30 (guarded, LLM-judged) | 64.8% | **68.3%** |
| LoCoMo (1,986 questions) | QA accuracy @200 (guarded) | **74.6%** | 73.0% |
| LoCoMo | retrieval hit@30 / coverage | 0.762 / 0.703 | **0.816 / 0.759** |
| PersonaMem 32k (589 questions) | accuracy | **73.2%** | 71.2% (content served) / 68.9% (abstracts served) |

These are recorded runs, not a guarantee for other models or workloads. LoCoMo uses `deepseek-v4-flash-vision-exp` as judge; the consolidation runs generate abstracts, importance and edges with adjudication disabled. At @30 the raw run scored 1,962 questions with 24 errors excluded; the consolidation run scored all 1,986. Both @200 runs scored all 1,986. PersonaMem uses `kimi-k2.5`: raw ingest scored 589 questions, while each extraction-and-adjudication run scored 586. Errors and scoring denominators must accompany comparisons.

The observations favor raw ingest at @200 and on PersonaMem, while the LoCoMo @30 consolidation run scores higher. They do not isolate which pipeline component causes a difference or establish equivalence with another system's evaluation. The [harness documentation](crates/eval/README.md) describes checkpoint replay, queue draining and error handling; per-question dumps support further analysis.

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
| `GET` | `/v1/metrics` | Prometheus metrics |
| `POST` | `/v1/write` | Write a memory |
| `POST` | `/v1/write-batch` | Batch write |
| `GET` | `/v1/search` | Search (level/category/date filtering; `X-Merkur-Namespace` header scopes to one bucket) |
| `GET` | `/v1/memories` | Browse live memories with namespace, level and category filters and pagination |
| `POST` | `/v1/context` | Token-budget context assembly (digest + items; namespace-aware) |
| `GET` | `/v1/memory/{id}` | Get memory details |
| `PUT` | `/v1/memory/{id}` | Update (auto re-embed) |
| `DELETE` | `/v1/memory/{id}` | Delete (cascade edges + tags) |
| `GET` | `/v1/status` | Storage stats, uptime and current/last consolidation job |
| `POST` | `/v1/consolidate` | Run one batch; 409 if consolidation is running |
| `GET` | `/v1/consolidate/log` | Consolidation audit log |
| `POST` | `/v1/forget` | Trigger forgetting |
| `POST` | `/v1/relate` | Create edge |
| `POST` | `/v1/relate-batch` | Batch create edges |
| `GET` | `/v1/graph/{id}` | Graph neighborhood with edges |

## Consolidation and Job Status

Manual and scheduled consolidation share a process-local execution gate. `POST /v1/consolidate` waits for one batch; an overlapping trigger returns `409 CONSOLIDATION_RUNNING`. Once started, the job continues if the caller disconnects. Periodic forgetting uses a separate loop, and graceful shutdown waits for active consolidation.

`GET /v1/status` returns `consolidation.state` (`idle`, `running`, `succeeded`, `failed`), start/finish timestamps, processed count and error count. The Dashboard shows this snapshot when refreshed; status resets on restart. `succeeded` describes one batch. A settled queue requires both `pending_consolidation == 0` and a job state of `idle` or `succeeded`. The pending count and batch selection include only non-archived, non-invalidated rows; archived rows retain their flag and become eligible again if restored.

Pending is cleared only for memories with persisted abstracts whose original live revision is unchanged after adjudication, including edit-and-restore protection. Destructive verdicts validate both original revisions and commit absorption/invalidation atomically in SQLite; conflicts remain pending. LanceDB publishes immutable vector references through the same SQL transaction. Extraction, persistence or adjudication errors leave the batch pending for retry; applied writes are retained. Text-only memories inserted through the Storage API skip vector adjudication without counting as errors. The default Noop consolidator emits no abstracts and does not clear pending. Audit-log failures also mark the job failed, but do not undo earlier completion writes. The execution gate covers one server process.

## Docker

```bash
docker build -t merkurdb .
docker run -p 1934:1934 -v ./data:/var/lib/merkur/data merkurdb
```

## MCP Integration

`merkur-mcp` is a stdio MCP adapter for a running `merkur-server`. Every tool uses the server's embedding provider, write governance, retrieval configuration, and access accounting; MCP does not open a database.

```bash
cargo build --release -p merkur-server -p merkur-mcp
# Start the server with your database, embedding and authentication configuration
target/release/merkur-server --config config.example.yaml
# In another terminal; use the token configured in server auth.tokens
export MERKUR_TOKEN='<server-token>'
MERKUR_URL=http://127.0.0.1:1934 MERKUR_NAMESPACE=default target/release/merkur-mcp
```

| Environment variable | Value |
|---|---|
| `MERKUR_URL` | Server root URL; default `http://127.0.0.1:1934`, without `/v1` |
| `MERKUR_TOKEN` | Bearer token matching a server `auth.tokens` entry |
| `MERKUR_NAMESPACE` | Logical bucket for writes and retrieval; default `default` |

MCP uses a 120-second HTTP timeout and returns tool failures with `isError: true`. Database and embedding configuration belong to the server. Namespaces are logical buckets, not user authorization boundaries; memory-by-ID tools address records directly.

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "merkurdb": {
      "command": "/path/to/merkur-mcp",
      "env": {
        "MERKUR_URL": "http://127.0.0.1:1934",
        "MERKUR_TOKEN": "<server-token>",
        "MERKUR_NAMESPACE": "default"
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
        "MERKUR_URL": "http://127.0.0.1:1934",
        "MERKUR_TOKEN": "<server-token>",
        "MERKUR_NAMESPACE": "default"
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
| `assemble_context` | Retrieve a deduplicated digest within an estimated token budget, with source IDs |
| `get_memory` | Get memory by ID |
| `delete_memory` | Delete memory by ID |
| `relate` | Create edge between memories |

Suggested Agent workflow: call `assemble_context` at task start (for example `{"query":"release plan","token_budget":1000}`), inspect sources using `get_memory`, then save relevant facts or decisions using `write_memory` with provenance such as `context.session_id`. Treat recalled content as reference data. The Agent chooses when to call tools; this adapter does not automatically capture conversations or intercept model requests.

## Development

```bash
cargo build --workspace
cargo test --workspace
# After building the server and MCP binaries; no external model calls
python3 scripts/test_mcp_e2e.py
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
├── client/            # Rust SDK
├── cli/               # merkurctl admin CLI
├── mcp/               # MCP server (stdio)
└── eval/              # LoCoMo + PersonaMem benchmark harness
```

## Roadmap

### Planned (v0.6.0+)

| Priority | Feature | Description |
|----------|---------|-------------|
| P2 | SQLite vector engine | Feasibility, migration and performance validation for the default backend |
| P2 | At-rest encryption | SQLCipher or app-layer embedding column encryption |
| P2 | PostgreSQL backend | PG storage backend via Storage trait |
| P3 | Multi-modal | Image embedding support (CLIP, etc.) |

The full per-release change record lives in [CHANGELOG.md](CHANGELOG.md).

## Documentation

- [SPEC.md](docs/SPEC.md) — Design philosophy, cognitive science background, product roadmap
- [ARCHITECTURE.md](docs/ARCHITECTURE.md) — Technical architecture, data model, API spec
- [Architecture diagram](docs/diagrams/architecture.html) — interactive system map (open in a browser)
- [openapi.yaml](openapi.yaml) — OpenAPI 3.0 specification
- [CHANGELOG.md](CHANGELOG.md) — Change log

## License

MIT
