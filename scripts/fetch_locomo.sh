#!/usr/bin/env bash
# Download the LoCoMo benchmark dataset (snap-research/locomo, CC BY-NC 4.0 —
# research use only). Idempotent: skips files that already exist.
set -euo pipefail

DEST="$(cd "$(dirname "$0")/.." && pwd)/crates/eval/data"
mkdir -p "$DEST"

fetch() {
  local name="$1" url="$2"
  if [ -s "$DEST/$name" ]; then
    echo "skip $name (already present)"
  else
    echo "fetch $name"
    curl -fsSL -m 120 -o "$DEST/$name" "$url"
  fi
}

fetch locomo10.json https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json
echo "done -> $DEST"
