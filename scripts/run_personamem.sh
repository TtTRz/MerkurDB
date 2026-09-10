#!/usr/bin/env bash
# PersonaMem (32k tier) against a throwaway MerkurDB server.
# Same boot discipline as run_locomo.sh: temp sqlite, parked schedulers,
# health gate with server-death detection (fails loudly instead of racing
# ahead and connection-refusing every context).
#
# Required env (embedding, server-side):
#   MERKUR_EVAL_EMBED_BASE_URL / _API_KEY / _MODEL
# Required env (chat, eval-side; for TencentDB-comparable numbers use
# kimi-k2.5 via OpenRouter):
#   MERKUR_EVAL_CHAT_BASE_URL / MERKUR_EVAL_CHAT_API_KEY / MERKUR_EVAL_CHAT_MODEL
# Optional knobs:
#   MERKUR_EVAL_PM_LIMIT          retrieval depth (default 30)
#   MERKUR_EVAL_PM_JOBS           answers in flight per checkpoint (default 8)
#   MERKUR_EVAL_PM_CONTEXT_JOBS   contexts replayed concurrently (default 4)
#   MERKUR_EVAL_PM_CONTEXT        restrict to one context id prefix (smoke)
#   MERKUR_EVAL_PM_TAG            report file suffix (default "pm")
#   MERKUR_EVAL_PM_SERVE_ABSTRACTS  "1" = answer model gets distilled abstracts
#                                 instead of raw turn content (pipeline runs)
#   MERKUR_EVAL_CONSOLIDATOR      "llm" = extraction + adjudication pipeline on
#                                 (each checkpoint drains the queue before
#                                 answering — measured state is settled, not
#                                 scheduler-lagged)
#   MERKUR_EVAL_CONSOL_*          consolidator chat endpoint (defaults to CHAT_*)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIMIT="${MERKUR_EVAL_PM_LIMIT:-30}"
JOBS="${MERKUR_EVAL_PM_JOBS:-8}"
CONTEXT_JOBS="${MERKUR_EVAL_PM_CONTEXT_JOBS:-4}"
TAG="${MERKUR_EVAL_PM_TAG:-pm}"
PORT="${MERKUR_EVAL_PORT:-19390}"
SERVER_BIN="$ROOT/target/release/merkur-server"
EVAL_BIN="$ROOT/target/release/merkur-eval"
TOKEN="pm-$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')"

: "${MERKUR_EVAL_EMBED_BASE_URL:?set MERKUR_EVAL_EMBED_BASE_URL}"
: "${MERKUR_EVAL_EMBED_API_KEY:?set MERKUR_EVAL_EMBED_API_KEY}"
: "${MERKUR_EVAL_EMBED_MODEL:?set MERKUR_EVAL_EMBED_MODEL}"
: "${MERKUR_EVAL_CHAT_BASE_URL:?set MERKUR_EVAL_CHAT_BASE_URL}"
: "${MERKUR_EVAL_CHAT_MODEL:?set MERKUR_EVAL_CHAT_MODEL}"

for bin in "$SERVER_BIN" "$EVAL_BIN"; do
  if [ ! -x "$bin" ]; then
    echo "missing $bin — build first:" >&2
    echo "  cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval" >&2
    exit 1
  fi
done

WORK="$(mktemp -d /tmp/merkur-pm.XXXXXX)"
SERVER_PID=""
trap 'if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; fi; rm -rf "$WORK"' EXIT

CONSOLIDATOR_BLOCK='  consolidator:
    type: "noop"'
CONSOLIDATION_KNOBS="consolidation:
  interval_seconds: 86400"
DRAIN_ARG=()
# chat endpoint preflight matters independently of the consolidator one:
# the answer model and the consolidator can live on different providers
# (this run shape: kimi answers via OpenRouter + deepseek consolidates),
# and either one being out of credit makes the run worthless.
preflight_chat() { # $1 base, $2 key, $3 model, $4 label
  local ok=0
  for attempt in 1 2; do
    if curl -fs -m 45 "$1/v1/chat/completions" \
        -H "Authorization: Bearer $2" -H 'Content-Type: application/json' \
        -d "{\"model\":\"$3\",\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}],\"max_tokens\":1}" >/dev/null; then
      ok=1; break
    fi
    echo "$4 preflight attempt $attempt failed, retrying in 10s" >&2
    sleep 10
  done
  if [ "$ok" != "1" ]; then
    echo "$4 LLM preflight FAILED ($1 model=$3) — endpoint dead, broke, or congested" >&2
    exit 1
  fi
}

: "${MERKUR_EVAL_CHAT_API_KEY:?set MERKUR_EVAL_CHAT_API_KEY}"
preflight_chat "$MERKUR_EVAL_CHAT_BASE_URL" "$MERKUR_EVAL_CHAT_API_KEY" "$MERKUR_EVAL_CHAT_MODEL" "answer-model"

if [ "${MERKUR_EVAL_CONSOLIDATOR:-}" = "llm" ]; then
  CONSOL_BASE="${MERKUR_EVAL_CONSOL_BASE_URL:-${MERKUR_EVAL_CHAT_BASE_URL:-}}"
  CONSOL_KEY="${MERKUR_EVAL_CONSOL_API_KEY:-${MERKUR_EVAL_CHAT_API_KEY:-}}"
  CONSOL_MODEL="${MERKUR_EVAL_CONSOL_MODEL:-${MERKUR_EVAL_CHAT_MODEL:-}}"
  : "${CONSOL_BASE:?llm consolidator needs MERKUR_EVAL_CONSOL_* or CHAT_* env}"
  if [ "$CONSOL_BASE" != "$MERKUR_EVAL_CHAT_BASE_URL" ] || [ "$CONSOL_MODEL" != "$MERKUR_EVAL_CHAT_MODEL" ]; then
    preflight_chat "$CONSOL_BASE" "$CONSOL_KEY" "$CONSOL_MODEL" "consolidator"
  fi
  CONSOLIDATOR_BLOCK="  consolidator:
    type: \"llm\"
    llm:
      base_url: \"$CONSOL_BASE\"
      api_key: \"$CONSOL_KEY\"
      model: \"$CONSOL_MODEL\"
      backend: \"openai\"
      timeout_seconds: 600"
  # Adjudication stays at defaults (candidates 5, floor 0.6) — preference
  # evolution IS the mechanism under test. Batch 50: a 100-memory extraction
  # response exceeds a 600 s budget on slower chat providers (observed with
  # kimi-k2.5 via OpenRouter: the call died mid-body at the timeout).
  CONSOLIDATION_KNOBS="consolidation:
  interval_seconds: 5
  batch_size: 50"
  DRAIN_ARG=(--drain-consolidation)
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

HEALTHY=0
for i in $(seq 1 150); do
  if curl -fs -m 1 "http://127.0.0.1:$PORT/v1/health" >/dev/null 2>&1; then
    HEALTHY=1
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "server died during boot" >&2
    exit 1
  fi
  sleep 0.2
done
if [ "$HEALTHY" != "1" ]; then
  echo "server did not become healthy within 30s — aborting" >&2
  exit 1
fi

CTX_ARG=()
if [ -n "${MERKUR_EVAL_PM_CONTEXT:-}" ]; then
  CTX_ARG=(--context "$MERKUR_EVAL_PM_CONTEXT")
fi

SERVE_ARG=()
if [ "${MERKUR_EVAL_PM_SERVE_ABSTRACTS:-}" = "1" ]; then
  SERVE_ARG=(--serve-abstracts)
fi

echo "== pm-run limit=$LIMIT jobs=$JOBS context_jobs=$CONTEXT_JOBS (model: $MERKUR_EVAL_CHAT_MODEL, consolidator: ${MERKUR_EVAL_CONSOLIDATOR:-noop}, serve_abstracts: ${MERKUR_EVAL_PM_SERVE_ABSTRACTS:-0})"
"$EVAL_BIN" --server "http://127.0.0.1:$PORT" --token "$TOKEN" \
  pm-run --limit "$LIMIT" --jobs "$JOBS" --context-jobs "$CONTEXT_JOBS" \
  ${CTX_ARG[@]+"${CTX_ARG[@]}"} ${DRAIN_ARG[@]+"${DRAIN_ARG[@]}"} ${SERVE_ARG[@]+"${SERVE_ARG[@]}"} \
  --json "$WORK/$TAG.json" --dump "$WORK/$TAG.jsonl" 2>&1

cp "$WORK/$TAG.json" "$WORK/$TAG.jsonl" "$ROOT/crates/eval/data/"
echo "== done; reports at crates/eval/data/$TAG.json{,l}"
