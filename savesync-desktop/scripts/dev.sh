#!/usr/bin/env bash
# Start the Vite dev server and the Tauri debug shell together.
#
# The debug shell loads `build.devUrl` (http://127.0.0.1:1420). Running the
# binary without this server shows "Connection refused", so always use this
# script (or `npm run tauri dev` if the Tauri CLI is installed).
set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -d node_modules ]; then
  echo "node_modules is missing. Run: npm install" >&2
  exit 1
fi

npm run dev &
vite_pid=$!
trap 'kill "$vite_pid" 2>/dev/null || true' EXIT INT TERM

for _ in $(seq 1 100); do
  if curl -sf -o /dev/null http://127.0.0.1:1420/; then
    break
  fi
  sleep 0.1
done

if ! curl -sf -o /dev/null http://127.0.0.1:1420/; then
  echo "Vite did not become ready on http://127.0.0.1:1420" >&2
  exit 1
fi

cd src-tauri
cargo run
