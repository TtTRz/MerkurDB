#!/usr/bin/env bash
# Fusion-parameter sweep (P1-5): ONE persistent corpus, N server configs.
# Phase A ingests LoCoMo once into crates/eval/data/tune/merkur.db; phase B
# restarts the server per config (env-overridden fusion params) and runs
# recall only — no judge, no re-ingest, ranking-only comparison.
#
#   scripts/sweep_fusion.sh            # full sweep (ingest once + recalls)
#   scripts/sweep_fusion.sh --no-ingest # skip phase A (corpus already built)
#
# Knobs:
#   MERKUR_EVAL_TUNE_DIR       corpus dir (default crates/eval/data/tune;
#                              use a different dir per pipeline variant)
#   MERKUR_EVAL_CONSOLIDATOR   "llm" = ingest with the extraction pipeline on
#                              (drain before measuring; composite-weight
#                              sweeps are only meaningful on such a corpus)
#   MERKUR_EVAL_CONSOL_*       consolidator chat endpoint (defaults to CHAT_*)
#   MERKUR_EVAL_LIMIT / _JOBS  recall depth / concurrency
#
# Corpus persistence note: access_count drifts as recalls run, but with the
# forgetting tick parked the drifted counts never feed ranking — every config
# sees an identical corpus.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TUNE_DIR="${MERKUR_EVAL_TUNE_DIR:-$ROOT/crates/eval/data/tune}"
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
  local consolidator_block='  consolidator:
    type: "noop"'
  local consolidation_knobs="consolidation:
  interval_seconds: 86400"
  if [ "${MERKUR_EVAL_CONSOLIDATOR:-}" = "llm" ]; then
    local consol_base="${MERKUR_EVAL_CONSOL_BASE_URL:-${MERKUR_EVAL_CHAT_BASE_URL:-}}"
    local consol_key="${MERKUR_EVAL_CONSOL_API_KEY:-${MERKUR_EVAL_CHAT_API_KEY:-}}"
    local consol_model="${MERKUR_EVAL_CONSOL_MODEL:-${MERKUR_EVAL_CHAT_MODEL:-}}"
    : "${consol_base:?llm consolidator needs MERKUR_EVAL_CONSOL_* or CHAT_* env}"
    consolidator_block="  consolidator:
    type: \"llm\"
    llm:
      base_url: \"$consol_base\"
      api_key: \"$consol_key\"
      model: \"$consol_model\"
      backend: \"openai\"
      timeout_seconds: 600"
    consolidation_knobs="consolidation:
  interval_seconds: 5
  batch_size: 100
  adjudication_candidates: 0"
  fi
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
$consolidator_block
auth:
  tokens: ["$TOKEN"]
$consolidation_knobs
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
  if [ "${MERKUR_EVAL_CONSOLIDATOR:-}" = "llm" ]; then
    echo "== draining consolidation queue"
    for i in $(seq 1 2880); do
      PENDING=$(curl -fs -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT/v1/status" | python3 -c "import json,sys; print(json.load(sys.stdin)['pending_consolidation'])")
      [ "$PENDING" = "0" ] && break
      if [ "$i" = "2880" ]; then
        echo "consolidation did not drain within 4 h — aborting" >&2
        exit 1
      fi
      if [ "$((i % 60))" = "0" ]; then echo "   pending: $PENDING"; fi
      sleep 5
    done
    echo "== consolidation drained"
  fi
  stop_server
fi

# ── phase B: sweep (restart per config, same db) ──
echo "== phase B: sweep (limit=$LIMIT jobs=$JOBS)"

boot default
recall default
stop_server

# Composite score-weight sweep (meaningful on a consolidator corpus where
# importance varies). search/weight/importance shares via env overrides.
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=1.0 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.0 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.0 boot sw_rel_only && recall sw_rel_only; stop_server
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=0.6 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.2 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.2 boot sw_622 && recall sw_622; stop_server
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=0.4 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.3 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.3 boot sw_433 && recall sw_433; stop_server
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=0.7 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.15 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.15 boot sw_715 && recall sw_715; stop_server
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=0.5 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.0 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.5 boot sw_imp50 && recall sw_imp50; stop_server
MERKUR_RETRIEVAL__FUSION__SCORE_SEARCH=0.5 MERKUR_RETRIEVAL__FUSION__SCORE_WEIGHT=0.5 MERKUR_RETRIEVAL__FUSION__SCORE_IMPORTANCE=0.0 boot sw_w50 && recall sw_w50; stop_server

echo "== done; reports in $TUNE_DIR/sweep_*.json"
