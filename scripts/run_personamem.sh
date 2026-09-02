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
  interval_seconds: 86400
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

echo "== pm-run limit=$LIMIT jobs=$JOBS context_jobs=$CONTEXT_JOBS (model: $MERKUR_EVAL_CHAT_MODEL)"
"$EVAL_BIN" --server "http://127.0.0.1:$PORT" --token "$TOKEN" \
  pm-run --limit "$LIMIT" --jobs "$JOBS" --context-jobs "$CONTEXT_JOBS" \
  ${CTX_ARG[@]+"${CTX_ARG[@]}"} \
  --json "$WORK/$TAG.json" --dump "$WORK/$TAG.jsonl" 2>&1

cp "$WORK/$TAG.json" "$WORK/$TAG.jsonl" "$ROOT/crates/eval/data/"
echo "== done; reports at crates/eval/data/$TAG.json{,l}"
