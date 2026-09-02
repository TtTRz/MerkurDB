#!/usr/bin/env bash
# Fusion-parameter sweep (P1-5): ONE persistent corpus, N server configs.
# Phase A ingests LoCoMo once into crates/eval/data/tune/merkur.db; phase B
# restarts the server per config (env-overridden fusion params) and runs
# recall only — no judge, no re-ingest, ranking-only comparison.
#
#   scripts/sweep_fusion.sh            # full sweep (ingest once + 9 recalls)
#   scripts/sweep_fusion.sh --no-ingest # skip phase A (corpus already built)
#
# Corpus persistence note: access_count drifts as recalls run, but with the
# forgetting tick parked the drifted counts never feed ranking — every config
# sees an identical corpus.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TUNE_DIR="$ROOT/crates/eval/data/tune"
DB="$TUNE_DIR/merkur.db"
LIMIT="${MERKUR_EVAL_LIMIT:-30}"
JOBS="${MERKUR_EVAL_JOBS:-8}"
PORT="${MERKUR_EVAL_PORT:-19391}"
SERVER_BIN="$ROOT/target/release/merkur-server"
EVAL_BIN="$ROOT/target/release/merkur-eval"
TOKEN="sweep-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"

: "${MERKUR_EVAL_EMBED_BASE_URL:?set MERKUR_EVAL_EMBED_BASE_URL}"
: "${MERKUR_EVAL_EMBED_API_KEY:?set MERKUR_EVAL_EMBED_API_KEY}"
: "${MERKUR_EVAL_EMBED_MODEL:?set MERKUR_EVAL_EMBED_MODEL}"

mkdir -p "$TUNE_DIR"

SERVER_PID=""
stop_server() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""; fi
}
trap stop_server EXIT

boot() {
  # $1 = log tag (server stderr is appended to the sweep output)
  cat > "$TUNE_DIR/config.yaml" <<EOF
server:
  host: "127.0.0.1"
  port: $PORT
  dev_mode: false
storage:
  type: "sqlite"
  sqlite:
    path: "$DB"
plugins:
  embedder:
    type: "openai"
    openai:
      base_url: "$MERKUR_EVAL_EMBED_BASE_URL"
      api_key: "$MERKUR_EVAL_EMBED_API_KEY"
      model: "$MERKUR_EVAL_EMBED_MODEL"
  consolidator:
    type: "noop"
auth:
  tokens: ["$TOKEN"]
consolidation:
  interval_seconds: 86400
forgetting:
  interval_seconds: 86400
logging:
  level: "warn"
EOF
  "$SERVER_BIN" --config "$TUNE_DIR/config.yaml" &
  SERVER_PID=$!
  local healthy=0
  for i in $(seq 1 150); do
    if curl -fs -m 1 "http://127.0.0.1:$PORT/v1/health" >/dev/null 2>&1; then healthy=1; break; fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then echo "server died during boot ($1)" >&2; exit 1; fi
    sleep 0.2
  done
  [ "$healthy" = "1" ] || { echo "server unhealthy ($1)" >&2; exit 1; }
}

recall() {
  # $1 tag, extra args follow
  local tag="$1"; shift
  "$EVAL_BIN" --server "http://127.0.0.1:$PORT" --token "$TOKEN" \
    recall --limit "$LIMIT" --jobs "$JOBS" "$@" \
    --json "$TUNE_DIR/sweep_${tag}.json" 2>&1 | grep -E "^overall|^category|errors"
}

# ── phase A: persistent corpus ──
if [ "${1:-}" = "--no-ingest" ] && [ -f "$DB" ]; then
  echo "== ingest skipped (corpus exists: $DB)"
else
  echo "== phase A: ingest (persistent corpus at $DB)"
  rm -f "$DB" "$DB-shm" "$DB-wal"
  boot ingest
  "$EVAL_BIN" --server "http://127.0.0.1:$PORT" --token "$TOKEN" ingest
  stop_server
fi

# ── phase B: sweep (restart per config, same db) ──
echo "== phase B: fusion sweep (limit=$LIMIT jobs=$JOBS)"

boot default
recall default
# Pure channels need no restart — mode is a per-request knob.
recall vec-only --mode vector
recall bm25-only --mode bm25
stop_server

MERKUR_RETRIEVAL__FUSION__RRF_K=20 boot k20 && recall k20; stop_server
MERKUR_RETRIEVAL__FUSION__RRF_K=100 boot k100 && recall k100; stop_server
MERKUR_RETRIEVAL__FUSION__BM25_WEIGHT=1.5 boot bm25x15 && recall bm25x15; stop_server
MERKUR_RETRIEVAL__FUSION__VECTOR_WEIGHT=1.5 boot vecx15 && recall vecx15; stop_server
MERKUR_RETRIEVAL__FUSION__BM25_WEIGHT=2.0 boot bm25x2 && recall bm25x2; stop_server
MERKUR_RETRIEVAL__FUSION__VECTOR_WEIGHT=2.0 boot vecx2 && recall vecx2; stop_server

echo "== done; reports in $TUNE_DIR/sweep_*.json"
