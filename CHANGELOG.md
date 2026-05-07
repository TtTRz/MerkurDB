# Changelog

All notable changes to MerkurDB.

## [0.1.0] — 2026-05-07

### Core

- Memory data model: `Memory`, `NewMemory`, `MemoryLevel` (Full/Summary/Title/Archived), `ScoredMemory`
- Edge model: `Edge`, `NewEdge`, `EdgeType` (Auto/Manual)
- Plugin traits: `Embedder`, `Storage`, `Consolidator`, `Forgetter` — each independently replaceable
- Error types: `HippoError` (Storage/Embedding/MemoryNotFound/Config/Internal)
- Support types: `StorageStats`, `ConsolidationReport`, `LevelAction`, `SearchMode`, `WriteItem`, `SearchOptions`, `WriteResponse`, `WriteBatchResponse`

### Storage (SQLite + InMemoryVectorIndex / LanceDB)

- **SqliteStorage**: SQLite metadata + in-memory vector index with cosine similarity
- **LanceDbStorage**: SQLite metadata + LanceDB disk-based vector index (zero-copy, IVF-PQ)
  - Feature-gated behind `--features lancedb`, requires system `protoc`
  - Automatic vector index creation and management
  - Cosine distance search with score conversion
- Full CRUD: insert, get, delete, vector search, context tags
- BFS graph expansion via recursive CTE with cycle detection
- Consolidation pipeline: list_pending, mark_consolidated, update_level
- Forgetting pipeline: list_for_forgetting, delete_archived_older_than (with vector/LanceDB cleanup)
- Access tracking: `access_count` and `accessed_at` updated on every read
- Consolidation log table writes timestamped records
- 6 storage tests covering CRUD, vector search, BFS, cascade delete, stats

### Embedders

- **NoopEmbedder**: deterministic hash-based vectors for testing (same text → same vector)
- **OllamaEmbedder**: integration with Ollama `/api/embeddings` endpoint
- **OpenAIEmbedder**: integration with OpenAI/DeepSeek `/v1/embeddings` API
- Feature gating: `ollama` (default) and `openai` features, noop always available

### Consolidators

- **NoopConsolidator**: returns empty report, for basic usage
- **LlmConsolidator**: calls LLM to generate abstracts and extract entity relations
- `ConsolidationReport` carries abstracts and edges for application by the scheduler
- 2 tests covering empty and non-empty consolidation

### Forgetters

- **EbbinghausForgetter**: implements the Ebbinghaus forgetting curve
  - Formula: w(t) = w₀ · α^(Δt/d) · (1 + β · ln(1 + n)/ln(2))
  - Configurable decay factor, half-life, access boost, three-level thresholds
  - Cascade downgrade: Full → Summary → Title → Archive
- 5 tests covering weight decay, access boost, downgrade decisions, archive

### HTTP Server (axum)

**Endpoints (14 total):**

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/health` | Health check with version |
| `POST` | `/v1/write` | Write single memory |
| `POST` | `/v1/write-batch` | Batch write memories |
| `GET` | `/v1/search` | Semantic search (fast vector / deep BFS) |
| `GET` | `/v1/memory/{id}` | Get memory details |
| `DELETE` | `/v1/memory/{id}` | Delete memory |
| `GET` | `/v1/status` | Storage statistics |
| `POST` | `/v1/consolidate` | Trigger consolidation manually |
| `GET` | `/v1/consolidate/log` | Consolidation audit trail |
| `POST` | `/v1/forget` | Trigger forgetting evaluation |
| `POST` | `/v1/relate` | Create manual edge between memories |
| `GET` | `/v1/graph/{id}` | View memory graph neighborhood |

**Features:**
- Configurable via YAML file and `MERKUR_` environment variables
- Tilde expansion for database path
- Context-dependent search with soft filtering and score boosting
- Deep search (S2): vector seeds → BFS graph expansion, configurable depth and degree limit
- Background scheduler: automatic consolidation (60s) and forgetting evaluation (300s)
- Consolidation log persistence with timestamps
- Structured error responses: `{"error": {"code": "...", "message": "..."}}`

**Dual retrieval:**
- S1 Fast: cosine similarity on in-memory vector index
- S2 Deep: vector search for seeds → BFS graph diffusion via SQLite CTE

**6 integration tests** covering write+search, memory CRUD, status, consolidation, relate+graph, deep search.

### Configuration

- `config.example.yaml` with all settings documented
- Server: host, port
- Storage: type (sqlite), path
- Plugins: embedder type (noop/ollama/openai) with per-backend config
- Retrieval: fast_default_limit, score_threshold
- Scheduler: consolidation/forgetting intervals, batch sizes, archive retention
- Logging: level, format

### API Documentation

- `openapi.yaml` — OpenAPI 3.0.3 spec with all endpoints, schemas, examples
- Compatible with `openapi-generator` for Python/TypeScript/Go SDK generation

### Project Structure

```
crates/
├── core/            # Types, traits, errors (275 lines)
├── storage/         # SQLite + vector index (967 lines)
├── embedders/       # Noop, Ollama, OpenAI (375 lines)
├── consolidators/   # Noop, LLM (189 lines)
├── forgetters/      # Ebbinghaus (169 lines)
└── server/          # axum HTTP + scheduler (1320 lines)
```

21 tests, 0 clippy warnings.

### Rename

- `HippoError` → `MerkurError`, `HippoResult` → `MerkurResult` (237 occurrences across 13 files)

### Client SDK

- `merkur-client` crate: `MerkurClient` trait (14 async methods) + `HttpMerkurClient` (reqwest-based)
- Response types: WriteResponse, SearchResponse, StatusResponse, ConsolidateResponse, etc.

### Documentation

- Split into 3 docs: README.md (intro), ARCHITECTURE.md (technical), SPEC.md (design philosophy)
- Replaced obsolete `docs/merkur-spec.md` and `docs/merkur-design.md`
