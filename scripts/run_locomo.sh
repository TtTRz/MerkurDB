#!/usr/bin/env bash
# Orchestrate a full LoCoMo run against a throwaway MerkurDB server:
# build config -> boot server -> ingest -> recall -> qa (if chat env set).
#
# Required env (embedding, server-side). BASE_URLs are service ROOTs — the
# /v1/... path is appended by the server/eval code (same convention for chat).
#   MERKUR_EVAL_EMBED_BASE_URL   e.g. https://api.openai.com  (no trailing /v1)
#   MERKUR_EVAL_EMBED_API_KEY    API key
#   MERKUR_EVAL_EMBED_MODEL      e.g. text-embedding-3-small
# Optional env (chat, eval-side; enables the qa stage):
#   MERKUR_EVAL_CHAT_BASE_URL / MERKUR_EVAL_CHAT_API_KEY / MERKUR_EVAL_CHAT_MODEL
# Optional knobs:
#   MERKUR_EVAL_LIMIT   retrieval depth for recall/qa (default 10)
#   MERKUR_EVAL_JOBS    concurrent questions in recall/qa (default 8; 1 = serial)
#   MERKUR_EVAL_CONV    restrict to one sample_id (default: all 10)
#   MERKUR_EVAL_CONSOLIDATOR  "llm" enables the extraction pipeline after
#                             ingest (importance/abstracts/edges; adjudication
#                             stays off) and waits for the queue to drain
#                             before measuring. Unset = noop (parked pipeline).
#   MERKUR_EVAL_CONSOL_BASE_URL / _API_KEY / _MODEL  consolidator chat endpoint
#                             (defaults to the DeepSeek MERKUR_EVAL_CHAT_* set)
#
# The server binary must be built with the openai embedder feature:
#   cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIMIT="${MERKUR_EVAL_LIMIT:-10}"
JOBS="${MERKUR_EVAL_JOBS:-8}"
ANSWER_STYLE="${MERKUR_EVAL_ANSWER_STYLE:-baseline}"
PORT="${MERKUR_EVAL_PORT:-19390}"
SERVER_BIN="$ROOT/target/release/merkur-server"
EVAL_BIN="$ROOT/target/release/merkur-eval"
TOKEN="eval-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"

: "${MERKUR_EVAL_EMBED_BASE_URL:?set MERKUR_EVAL_EMBED_BASE_URL}"
: "${MERKUR_EVAL_EMBED_API_KEY:?set MERKUR_EVAL_EMBED_API_KEY}"
: "${MERKUR_EVAL_EMBED_MODEL:?set MERKUR_EVAL_EMBED_MODEL}"

for bin in "$SERVER_BIN" "$EVAL_BIN"; do
  if [ ! -x "$bin" ]; then
    echo "missing $bin — build first:" >&2
    echo "  cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval" >&2
    exit 1
  fi
done

WORK="$(mktemp -d /tmp/merkur-eval.XXXXXX)"
trap 'kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$WORK"' EXIT

CONSOLIDATOR_BLOCK='  consolidator:
    type: "noop"'
CONSOLIDATION_KNOBS="consolidation:
  interval_seconds: 86400"
if [ "${MERKUR_EVAL_CONSOLIDATOR:-}" = "llm" ]; then
  CONSOL_BASE="${MERKUR_EVAL_CONSOL_BASE_URL:-${MERKUR_EVAL_CHAT_BASE_URL:-}}"
  CONSOL_KEY="${MERKUR_EVAL_CONSOL_API_KEY:-${MERKUR_EVAL_CHAT_API_KEY:-}}"
  CONSOL_MODEL="${MERKUR_EVAL_CONSOL_MODEL:-${MERKUR_EVAL_CHAT_MODEL:-}}"
  : "${CONSOL_BASE:?llm consolidator needs MERKUR_EVAL_CONSOL_* or CHAT_* env}"
  # Extraction on (importance/abstracts/edges), adjudication off
  # (candidates=0 skips the phase entirely; per-memory LLM verdicts would
  # dominate the runtime without changing this corpus much).
  CONSOLIDATOR_BLOCK="  consolidator:
    type: \"llm\"
    llm:
      base_url: \"$CONSOL_BASE\"
      api_key: \"$CONSOL_KEY\"
      model: \"$CONSOL_MODEL\"
      backend: \"openai\"
      timeout_seconds: 600"
  CONSOLIDATION_KNOBS="consolidation:
  interval_seconds: 5
  batch_size: 25
  adjudication_candidates: 0"
fi

cat > "$WORK/config.yaml" <<EOF
server:
  host: "127.0.0.1"
  port: $PORT
  dev_mode: false
storage:
  type: "sqlite"
  sqlite:
    path: "$WORK/merkur.db"
plugins:
  embedder:
    type: "openai"
    openai:
      base_url: "$MERKUR_EVAL_EMBED_BASE_URL"
      api_key: "$MERKUR_EVAL_EMBED_API_KEY"
      model: "$MERKUR_EVAL_EMBED_MODEL"
$CONSOLIDATOR_BLOCK
auth:
  tokens: ["$TOKEN"]
$CONSOLIDATION_KNOBS
forgetting:
  interval_seconds: 86400
logging:
  level: "warn"
EOF

echo "== boot server on :$PORT (db: $WORK/merkur.db)"
"$SERVER_BIN" --config "$WORK/config.yaml" &
SERVER_PID=$!

for i in $(seq 1 50); do
  if curl -fs -m 1 "http://127.0.0.1:$PORT/v1/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "server died during boot" >&2
    exit 1
  fi
  sleep 0.2
done

export MERKUR_EVAL_SERVER="http://127.0.0.1:$PORT"
export MERKUR_EVAL_TOKEN="$TOKEN"
CONV_ARG=()
if [ -n "${MERKUR_EVAL_CONV:-}" ]; then
  CONV_ARG=(--conv "$MERKUR_EVAL_CONV")
fi
# bash 3.2 (macOS) + set -u: expanding an empty array needs the + guard
# at each use site — `"${CONV_ARG[@]+"${CONV_ARG[@]}"}"`.

echo "== ingest"
"$EVAL_BIN" ingest ${CONV_ARG[@]+"${CONV_ARG[@]}"}

if [ "${MERKUR_EVAL_CONSOLIDATOR:-}" = "llm" ]; then
  echo "== waiting for consolidation queue to drain"
  # 5882 memories / 25 per tick with a reasoning model ≈ 2-3.5 h; budget 4 h.
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

TAG="${MERKUR_EVAL_TAG:-l${LIMIT}_${ANSWER_STYLE}}"

echo "== recall@$LIMIT (jobs=$JOBS)"
"$EVAL_BIN" recall --limit "$LIMIT" --jobs "$JOBS" ${CONV_ARG[@]+"${CONV_ARG[@]}"} --json "$WORK/recall_${TAG}.json" --dump "$WORK/recall_${TAG}.jsonl"

if [ -n "${MERKUR_EVAL_CHAT_BASE_URL:-}" ]; then
  echo "== qa@$LIMIT (judge: ${MERKUR_EVAL_CHAT_MODEL:-?}, jobs=$JOBS, style=$ANSWER_STYLE)"
  "$EVAL_BIN" qa --limit "$LIMIT" --jobs "$JOBS" --answer-style "$ANSWER_STYLE" ${CONV_ARG[@]+"${CONV_ARG[@]}"} --json "$WORK/qa_${TAG}.json" --dump "$WORK/qa_${TAG}.jsonl"
else
  echo "== qa skipped (MERKUR_EVAL_CHAT_* not set)"
fi

cp "$WORK"/*.json "$WORK"/*.jsonl "$ROOT/crates/eval/data/" 2>/dev/null || true
echo "== done; reports copied to crates/eval/data/ (tag: $TAG)"
