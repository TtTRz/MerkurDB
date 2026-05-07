# Changelog

All notable changes to MerkurDB. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project follows [Semantic Versioning](https://semver.org/).

## [0.2.0] — 2026-05-07

Cross-stack hardening pass. BREAKING changes touch HTTP response bodies, config keys, trait surface, and enum serialization; see the bottom of this entry for migration notes.

### Added

- **Bearer-token authentication** (`server::auth::require_auth`) is a from_fn_with_state middleware applied to every `/v1/*` route except `/v1/health`. Tokens come from `config.auth.tokens` and are compared in constant time. Empty token list with `auth.disabled = false` fails closed.
- **Structured API errors** (`server::error::ApiError`) map every `MerkurError` variant to the right HTTP status (BadRequest → 400, MemoryNotFound → 404, Embedding → 502, Timeout → 504, etc.). Internal detail hits `tracing::error!` but never the response body.
- **Request body limit** via `DefaultBodyLimit::max(MAX_BODY_BYTES)` caps bodies at 10 MiB, preventing oversize-JSON OOM.
- **Hard parameter bounds** via `core::limits` (MAX_SEARCH_LIMIT=1000, MAX_BFS_DEPTH=5, MAX_BFS_DEGREE=100, MAX_BATCH_ITEMS=500, MAX_CONTENT_BYTES=64 KiB, MAX_BODY_BYTES=10 MiB); handlers clamp or reject out-of-range values.
- **Graceful shutdown signal** for the scheduler: `Scheduler::run` now accepts a `tokio::sync::watch::Receiver<bool>` and exits after the current tick instead of being aborted mid-write.
- **LLM consolidator is wired in**: `plugins.consolidator.type = "llm"` with a `llm.base_url` / `llm.model` block finally reaches `main`. Previously the implementation existed but `main` hard-coded `NoopConsolidator`.
- **Config validation** (`Config::validate`) rejects zero ports, non-positive half-lives, negative archive windows, out-of-range score thresholds, wildcard CORS without `dev_mode`, and missing auth tokens in production.
- **Built-in defaults YAML** always merges first, so running without `--config` produces a coherent config instead of panicking on missing required fields.
- **`Storage::memory_exists`** lets higher layers validate FK-like preconditions without relying on engine FKs.
- **New `AuthConfig` / `ConsolidatorConfig`** plus `ServerConfig.cors_allow_origin` and `ServerConfig.dev_mode`.
- **`OpenAIEmbedder::new_with_dimensions`** adds the `dimensions` parameter for the `text-embedding-3-*` family.
- **Subgraph edges on `GET /v1/graph/{id}`**: the response now includes edges for every node in the neighborhood, not just the centre. `?depth` / `?degree_limit` query parameters are honoured.
- **Batch helpers** in storage (`get_edges_batch`, `update_access` with chunked IDs) replace N+1 round-trips with single `IN (SELECT value FROM json_each(?))` queries.
- **9 new tests**: `test_delete_cascades_edges_and_context`, `test_insert_edge_to_unknown_memory_fails`, `test_memory_exists`, `test_relate_self_edge_rejected`, `test_relate_unknown_target_rejected`, `test_search_invalid_mode_400`, `test_half_life_is_exact`, `test_clock_skew_treated_as_zero`, and vector-index unit tests (`test_upsert_replaces_existing`, `test_remove_swap`, `test_topk_smaller_than_limit`, `test_zero_vector_score_is_zero`). Total tests: **36 passing, 0 failing, 2 ignored (require live Ollama / OpenAI)**.

### Changed

- **Every rusqlite call runs inside `tokio::task::spawn_blocking`** via a `run_blocking` helper; synchronous SQL no longer starves tokio workers.
- **Atomic writes**: `insert_memory` wraps memories + context_tags in a single transaction; the in-memory vector index is only touched after the DB commits, so a failure cannot leave a dangling vector. `update_memory` with `embedding = None` clears the vector, matching the "invalidate on None" contract across both SQLite and LanceDB backends.
- **`InMemoryVectorIndex` is O(1) upsert / O(n log k) search**: parallel `vectors` + `ids` storage with a `HashMap<id, index>` for constant-time updates and swap-removes; search uses a bounded min-heap instead of a full sort. An `OrderedF64` wrapper pushes NaN to the heap bottom so degenerate similarities no longer panic `partial_cmp`.
- **Search parameters are clamped** to `core::limits` bounds in the HTTP layer; unknown `mode` values return 400 with a structured error.
- **`update_memory` existence check before embedding** (`memory::update_memory`): a non-existent id no longer burns a paid OpenAI / Ollama request.
- **Self-edges and unknown endpoints rejected at the HTTP layer** (`POST /v1/relate`, `/v1/relate-batch`) with 400 / 404 on top of the FK enforcement now present at the storage engine.
- **Scheduler reports actual insert counts**: `ConsolidationReport.edges_created` reflects the number of edges that actually inserted, not what the LLM merely proposed.
- **LLM consolidator input validation**: abstracts and edges reference only ids present in the input batch; self-edges are rejected; hallucinated ids are dropped and counted into `report.errors`. Prompts are built with `serde_json` so backslashes / Unicode in content can't corrupt the prompt. `extract_json_object` trims markdown fences and surrounding prose so real local-model output parses without brittle regex.
- **`AppState` wraps `Config` in `Arc`** so a handler invocation is no longer a full `Config` clone.
- **`main` returns `anyhow::Result`**: startup failures flow through `tracing::error!` and exit 1 instead of panicking.
- **Embedder probe failures are fatal**: guessing the embedding dimension is worse than failing loudly because it would corrupt the vector index.
- **Both HTTP embedders carry a 30 s `reqwest::Client` timeout**; hung providers no longer pin workers.
- **Ebbinghaus decay formula** is now `w(t) = w₀ · exp(-t · ln 2 / half_life)`, so the `half_life` name is mathematically honest. The previous `decay_factor.powf(t / half_life)` form was still exponential but had an effective half-life ~6.58× the configured value. `decay_factor` is retained for backwards-compatible config parsing but no longer participates in the computation. Clock skew (`accessed_at > now`) is clamped to zero with a warning.
- **Client SDK** shares `merkur_core::WriteItem` directly (previously the SDK redefined it without `metadata`), exposes the full `SearchParams` surface (depth, degree_limit, offset, level, category, from, to, include_graph, context), supports bearer tokens via `with_token`, carries a 30 s default timeout, and strips reqwest URLs from `ClientError` so a bearer token in the URL cannot leak.
- **OpenAPI 3.0.3 spec** declares a global `bearerAuth` security scheme, narrows search parameter bounds (limit 1–1000, depth 0–5, degree_limit 1–100, score_threshold -1..1), documents `SearchResponse.filters`, `StatusResponse.uptime_seconds`, `write-batch` errors/requested, and switches `Memory.level` to the lowercase enum.
- **Config example** is rewritten around the new feature set and notes that OpenAI's api_key can come from `MERKUR_PLUGINS__EMBEDDER__OPENAI__API_KEY`.

### Fixed

- **Foreign keys were silently off** for every pooled connection except the first. `PRAGMA foreign_keys` is per-connection, but the DDL script only runs once; every `ON DELETE CASCADE` reference was a no-op. Pool construction now goes through `sqlite_helpers::build_pool` with a `with_init` hook that runs `PRAGMA foreign_keys = ON` on every connection. Cascade delete actually cascades now.
- **Ollama embedder contract mismatch**: the previous code posted to the legacy `/api/embeddings` path but sent a body shaped for the modern `/api/embed` endpoint, so real Ollama servers rejected every request. Switch to `/api/embed` + `input` array + `embeddings` response.
- **LanceDB distance-to-score formula was wrong**: `1 - d / 2` is neither cosine similarity nor any standard measure. Replace with `cos(a,b) = 1 - d² / 2` clamped to `[-1, 1]`; the score field is now comparable with the SqliteStorage cosine output.
- **LanceDB `update_memory(embedding = None)` left the old vector**, so searches matched the pre-update semantics for updated content. Always drop the existing row first, matching the SqliteStorage contract.
- **BFS path cycle detection** now uses delimited paths (`',id,'`) so an id that is a substring of another id (`mem_a` vs `mem_abc`) cannot cause false cycle hits. BFS seed ids pass through `json_each(?1)` as a bound parameter; the previous format-string interpolation of seed ids was a latent SQL-injection vector.
- **`OpenAIEmbedder` response-length check**: the embedder now errors when the response length doesn't match the input length, so a partial batch cannot silently desync ids.
- **`mark_consolidated` chunking**: ids are split into groups of 500 to stay under SQLite's `SQLITE_MAX_VARIABLE_NUMBER`.
- **`Memory.embedding` never leaks into API responses**: `#[serde(default, skip_serializing)]` ensures the vector is neither returned to clients nor required on deserialize.
- **`MemoryLevel::from_i32`** coerces unknown values to `Archived` instead of promoting them to `Full`, so corrupt rows are hidden from retrieval rather than masquerading as the highest retention tier.
- **`NoopEmbedder` seeds StdRng from SHA-256** instead of `std::hash::DefaultHasher`, whose algorithm is explicitly not stable across Rust versions. Deterministic vectors now survive compiler upgrades. Zero-dim is rejected; zero-norm falls back to a canonical unit vector.
- **LanceDB `quote_id_strict`** replaces the previous `debug_assert!`-only id sanitization, so release builds validate ids instead of silently skipping the check.
- **`ensure_vector_table`** no longer calls `create_index` on an empty table; indexing is deferred until the table has real data.
- **`LlmConsolidator::new`** returns `MerkurResult`; a TLS / HTTP-client build failure no longer panics at startup.
- **`ConsolidationLogEntry` timestamps** are `DateTime<Utc>` instead of `String`, matching `Memory` fields.

### Security

- `/v1/*` endpoints are authenticated by default; attempts to start in production without `auth.tokens` are rejected.
- CORS wildcard (`Any`) is refused unless `server.dev_mode = true` is explicitly set; a comma-separated allow-list is the supported production shape.
- Error responses no longer carry raw SQL error strings, file paths, or provider messages; internal detail is logged server-side only.
- `ClientError` omits the reqwest URL, so bearer tokens embedded in URLs cannot leak through SDK error propagation.
- Request bodies are capped at 10 MiB.

### Migration notes (BREAKING)

- `Memory.level` and `EdgeType` serialize as lower-case in API responses (`"full"`, `"summary"`, `"title"`, `"archived"`, `"auto"`, `"manual"`). Clients parsing the previous PascalCase form must update.
- Error responses are always shaped as `{"error": {"code": "...", "message": "..."}}` with the status code carrying semantics; clients that scraped raw strings should switch to `error.code`.
- The environment-variable level separator is now `__` (double underscore). Rename `MERKUR_FORGETTING_HALF_LIFE_SECONDS` to `MERKUR_FORGETTING__HALF_LIFE_SECONDS`, and so on.
- `auth.tokens` is required in non-dev mode. Either set at least one bearer token or explicitly set `auth.disabled = true` together with `server.dev_mode = true`.
- `Storage::memory_exists` is a new trait method; any external `impl Storage` must provide it.
- `Cargo.toml` workspace version is `0.2.0`; dependents pinned to `0.1.0` need to bump.

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
