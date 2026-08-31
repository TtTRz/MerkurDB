# Changelog

All notable changes to MerkurDB. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Write governance: Consolidator adjudication (P1-7)** — after consolidation, each pending memory is adjudicated against its nearest same-bucket neighbors (mem0-style ADD/UPDATE/DELETE/NOOP). UPDATE *absorbs*: the target takes the new content in place (keeping its learned importance, access history, and edges) while the new row is invalidated with an `absorbed_into` audit pointer. DELETE invalidates the loser the LLM names — an existing memory, or the new write itself. Verdicts execute only when the pair's cosine similarity clears `consolidation.adjudication_floor` (0.6, dual-signal); hallucinated or unparseable verdicts fall back to ADD. Requires `plugins.consolidator.type = "llm"`; `NoopConsolidator` adjudicates nothing.
- **Bitemporal soft-invalidation (schema v5)** — `memories.valid_at` (backfilled from `created_at`; lazy — reserved so a future point-in-time query needs no migration) and `memories.invalid_at`. Invalidated rows vanish from every retrieval channel (vector, BM25, BFS, pending-consolidation and forgetting lists) but stay readable via `GET /v1/memory/{id}` for audit until purged.
- **Invalidation retention** — `forgetting.purge_invalidated_days` (default 30) hard-deletes rows past the audit window on each forgetting tick; `/v1/forget` reports `purged`. Client `DELETE /v1/memory/{id}` remains an immediate hard delete — soft-invalidation is a system-only channel.
- **`write.adjudication` reserved config key** — only `"async"` is accepted today; reserves the slot for a future synchronous adjudication mode without breaking config compatibility.
- **Client namespace support** — `HttpMerkurClient::with_namespace("bucket")` sends `X-Merkur-Namespace` on every request; `ForgetResponse` gains `upgraded`/`purged`.
- **Hybrid search (BM25 x vector)** — `/v1/search` gains `mode=hybrid`: FTS5 full-text (trigram tokenizer, CJK-capable) fused with vector cosine via Reciprocal Rank Fusion (`k=60`). Shared orchestration (`merkur_core::hybrid_recall`) powers both the REST handler and the MCP `search_memory` tool; a channel failure degrades to the other instead of failing the recall.
- **FTS5 schema (migration v2)** — `memories_fts` virtual table plus insert/update/delete triggers with automatic backfill of existing rows on upgrade. Triggers keep every write path (both storage backends, CLI tools) in sync.
- **`Storage::text_search` trait method** — best-first BM25 candidate lookup; implemented for `SqliteStorage` and `LanceDbStorage` (which rides the same shared SQLite database).
- **Access-driven promotion (`LevelAction::Upgrade`)** — the forgetting curve now closes the lifecycle loop: a demoted memory whose derived weight clears `threshold_upgrade` (0.6) *and* which has been retrieved at least `upgrade_min_access_count` (3) times climbs back one rung (Title -> Summary -> Full). Archived rows never auto-promote. New config keys under `forgetting:`; `/v1/forget` reports an `upgraded` count.
- **Logical namespaces (P0-3)** — every memory carries a `namespace` bucket (default `"default"`); `X-Merkur-Namespace` request header scopes writes, hybrid/fast/deep search, and BFS traversal to one bucket. Storage trait gains `vector_search_ns` / `bfs_expand_ns` and a namespaced `text_search`; legacy cross-bucket methods remain for audit paths. Migration v3 adds the column + index with zero-downtime backfill. **Isolation is logical, not a security boundary** — any authenticated caller may claim any bucket.
- **Composite scoring (P1-5)** — hybrid results are re-ranked by `final = 0.5·fused + 0.2·weight + 0.3·importance`. `importance` is **system-learned only**: the Consolidator writes it during consolidation (migration v4, neutral 0.5 prior for unassessed rows); the public write API has no such field, so salience can never be client-reported. Weights are conservative untuned defaults, documented as such.
- **`POST /v1/context` (P1-6)** — token-budget context assembly: hybrid recall → MMR dedup (Jaccard ≥ 0.8) → greedy bin-packing → prompt-ready markdown digest. Response carries `digest`, `items`, `token_estimate`, `dropped`. Token counting is the documented `chars/4` approximation (zero-dependency); namespace header scopes the recall exactly like `/v1/search`.
- **Write-time dedup (P2-8)** — `insert_memory_dedup` short-circuits near-duplicate writes: top-1 cosine ≥ `write.dedup_threshold` (0.92, mem0's published value) in the same namespace returns the existing id without inserting. ADD/NOOP half of write governance; UPDATE/DELETE adjudication stays with the Consolidator. `write.dedup_enabled` master switch.
- **Benchmarks** — `text_search_bm25_1k`, `hybrid_recall_end_to_end_1k`.

### Changed

- **`score_threshold` in hybrid mode now gates the fused retrieval relevance** (normalized RRF), not the composite score — previously the composite's structural floor (0.35 for a fresh memory at default weights) silently disabled the threshold in the default mode. Fast mode still gates raw cosine.
- **Access signal is recorded at serving points only** — retrieval (`vector_search_ns`, `text_search`, `bfs_expand_ns`) is now pure; the REST search handler, `/v1/context`, and the MCP `search_memory` tool call `Storage::record_access` for exactly the results they serve. Dedup probes and hydration internals no longer touch `access_count`/`accessed_at`, so the promotion signal reflects demonstrated demand only.
- **`GET /v1/memory/{id}` response** gains `namespace`, `importance`, `valid_at`, `invalid_at`.
- **`GET /v1/graph/{id}`** is namespace-scoped like every other read path and returns the induced subgraph over the visible nodes (no foreign-bucket endpoint leakage); `/v1/search?include_graph=true` applies the same rule.
- **Default search mode is now `hybrid`** (was `fast`). Scores returned under hybrid are composite values re-ranked from the RRF fusion; pass `mode=fast` explicitly to keep pure-vector behavior.
- MCP `search_memory` tool upgraded from pure vector similarity to hybrid retrieval.

### Fixed

- **Migrations are now transactional per version and replay-safe** — a crash between a migration step and its version bump no longer bricks the database (`duplicate column` on restart) or duplicates the FTS backfill; databases bricked by a partial migration heal on boot.
- **LanceDB backend runs schema migrations** — previously it created only the base tables, so every write failed with `no column named namespace`.
- **Namespace-scoped vector search no longer starves small buckets** — both backends deepen the global candidate probe until the bucket is served (a fixed 2× oversample could return zero in-bucket hits; write-time dedup could miss exact in-bucket duplicates).
- **Hybrid pagination** — the fused pool keeps headroom (`max(2×limit, offset+limit)`), so `offset` past page one and post-filters no longer return empty or underfilled pages.
- **Hybrid recall degrades on hydration failure** — a BM25-only candidate whose fetch fails is skipped instead of failing the whole recall.
- **BFS traversal no longer follows cross-bucket edges** — the recursive CTE filters hops by namespace, so foreign nodes can neither appear nor bridge back into the bucket (and no longer spend depth/limit budget).
- **Write-time dedup probe failures fall back to plain insert** instead of failing the write.
- **Config validation** — rejects `write.dedup_threshold` outside `(0, 1]` (0.0 caused silent write loss), `forgetting.threshold_upgrade <= threshold_to_l1` (per-tick level oscillation), and negative `forgetting.purge_invalidated_days`.
- Fresh databases skipped pending migrations: first-run version stamping wrote the current version before migrations ran, so auxiliary objects introduced in later versions were never created until the stored version was bumped externally.

## [0.4.0] — 2026-05-08

Feature expansion: observability, tooling, and AI agent integration.

### Added

- **Prometheus metrics endpoint** (`/v1/metrics`) — request count, latency histogram, exposed via `metrics` + `metrics-exporter-prometheus` crates.
- **Health endpoint enhanced** — `/v1/health` now probes database connectivity and reports embedder dimension; returns `"ok"` or `"degraded"`.
- **Rate limiting** via `governor` crate — configurable token bucket (`rate_limit.enabled`, `rate_limit.requests_per_second`), disabled by default.
- **`merkurctl` CLI** — admin tool with subcommands: `health`, `status`, `consolidate`, `forget`, `search`, `write`, `delete`, `graph`, `migrate`.
- **LlmConsolidator OpenAI backend** — `consolidator.llm.backend = "openai"` routes to `/v1/chat/completions` with `response_format: json_object`.
- **MCP adapter** (`merkur-mcp` binary) — Model Context Protocol server over stdio; tools: `write_memory`, `search_memory`, `get_memory`, `delete_memory`, `relate`.
- **DB migration framework** — `merkur_meta` table tracks schema version; `migrate()` runs on startup; `merkurctl migrate` for manual execution.
- **Criterion benchmarks** — `vector_search_10k_top100`, `bfs_expand_1k`, `upsert_remove_10k`.
- **CI bench compile check** step in GitHub Actions.

### Changed

- **Vector index uses `Arc<str>`** for id storage — O(1) clone during search instead of O(len).
- **Context boost applied before threshold filter** — low-scoring memories with matching context can now survive the threshold.
- **`write_batch` returns 207** Multi-Status when all items fail (was 201).
- **`access_bonus` capped at 3.0×** — prevents immortal memories under extreme access counts.
- **Dockerfile** includes `merkurctl` and `merkur-mcp` binaries.

### Migration notes

- Workspace version bumped to `0.4.0`.
- `merkur_meta` table auto-created on first startup (backward-compatible).
- New config keys: `rate_limit.*`, `consolidator.llm.backend`.

## [0.3.0] — 2026-05-08

Performance and correctness pass: 1 critical data-loss fix, 9 high-severity N+1/performance fixes, 1 medium logic bug.

### Added

- **`Storage::memory_exists_batch`** — validates a batch of ids in a single `json_each(?1)` query instead of N individual lookups. Used by `relate_batch` handler.
- **`Storage::get_edges_batch`** — fetches edges for multiple memory ids in one query. Used by `search` (`include_graph=true`) and `get_graph` handlers.
- **`Storage::update_abstract`** — writes consolidation abstracts directly to the `memories.abstract` column instead of misrouting them to context_tags.
- **Pre-computed L2 norms** in `InMemoryVectorIndex`: avoids recomputing `‖b‖₂` on every cosine similarity comparison during search (O(dim) saved per candidate).
- **LanceDB automatic index builder**: `spawn_index_builder` triggers a background `Index::Auto` build once row count crosses 256. Guarded by `AtomicBool` to prevent concurrent builds.
- **`rebuild_vector_index`** now actually creates the LanceDB vector index (previously a no-op stub).
- **6 new tests**: `test_memory_exists_batch`, `test_get_edges_batch`, `test_update_abstract`, `test_get_memory_no_embedding`, `test_norms_consistent_after_upsert_remove`. Total: **41 passing**.

### Changed

- **`bfs_expand` eliminates N+1 context_tags queries** (HV1): collects all BFS neighbor ids, then fetches context_tags in a single batch via `get_context_tags_batch`.
- **`write_batch` uses `encode_batch`** (HV2): one embedder round-trip instead of N individual `encode` calls.
- **`search` handler `include_graph`** (HV3) and **`get_graph` handler** (HV4): use `get_edges_batch` instead of per-id `get_edges` loops.
- **`relate_batch`** (HV5): validates all source/target ids in a single `memory_exists_batch` call (was 3N SQL queries).
- **`get_memory`** (HV7): no longer fetches the `embedding` BLOB column — saves bandwidth and avoids deserializing large vectors that are never returned to clients.

### Fixed

- **CRITICAL: `run_consolidation_once` data loss** (CV1): previously marked ALL candidate memories as consolidated even when `update_abstract` failed. Now only successfully-processed ids are marked, using the new `update_abstract` method that writes to `memories.abstract` column directly.
- **Consolidator wrote abstracts to context_tags** (MV1): consolidation results are now stored in the proper `memories.abstract` column via `Storage::update_abstract`.
- **LanceDB `update_memory` silently swallowed delete failures** (HV8): errors now propagate so callers can retry or alert.

### Migration notes (BREAKING)

- `Storage` trait has 3 new required methods: `get_edges_batch`, `memory_exists_batch`, `update_abstract`. External implementations must add them.
- `get_memory` no longer returns the `embedding` field (always `None`). Callers that relied on reading embeddings back must use the vector search path instead.
- Workspace version bumped to `0.3.0`.

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
