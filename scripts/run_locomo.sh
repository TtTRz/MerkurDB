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
#
# The server binary must be built with the openai embedder feature:
#   cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIMIT="${MERKUR_EVAL_LIMIT:-10}"
JOBS="${MERKUR_EVAL_JOBS:-8}"
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
  consolidator:
    type: "noop"
auth:
  tokens: ["$TOKEN"]
consolidation:
  interval_seconds: 86400   # offline pipeline parked: we measure retrieval
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

echo "== recall@$LIMIT (jobs=$JOBS)"
"$EVAL_BIN" recall --limit "$LIMIT" --jobs "$JOBS" ${CONV_ARG[@]+"${CONV_ARG[@]}"} --json "$WORK/recall.json" --dump "$WORK/recall.jsonl"

if [ -n "${MERKUR_EVAL_CHAT_BASE_URL:-}" ]; then
  echo "== qa@$LIMIT (judge: ${MERKUR_EVAL_CHAT_MODEL:-?}, jobs=$JOBS)"
  "$EVAL_BIN" qa --limit "$LIMIT" --jobs "$JOBS" ${CONV_ARG[@]+"${CONV_ARG[@]}"} --json "$WORK/qa.json" --dump "$WORK/qa.jsonl"
else
  echo "== qa skipped (MERKUR_EVAL_CHAT_* not set)"
fi

cp "$WORK"/*.json "$WORK"/*.jsonl "$ROOT/crates/eval/data/" 2>/dev/null || true
echo "== done; reports copied to crates/eval/data/"
