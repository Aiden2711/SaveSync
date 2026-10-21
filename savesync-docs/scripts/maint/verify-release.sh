#!/usr/bin/env bash
# SaveSync pre-release verification for the Phase 1 core engine.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$repo_root/savesync-core"

echo "==> cargo fmt"
cargo fmt --all -- --check

echo "==> cargo clippy"
cargo clippy --all-targets -- -D warnings

echo "==> cargo test"
cargo test

if [ -d "$repo_root/savesync-desktop/node_modules" ]; then
  echo "==> desktop frontend build"
  (cd "$repo_root/savesync-desktop" && npm run build)

  echo "==> desktop clippy"
  cd "$repo_root/savesync-desktop/src-tauri"
  cargo clippy --all-targets -- -D warnings

  echo "==> desktop tests"
  cargo test
else
  echo "==> skipping desktop checks (node_modules is missing; run npm install)"
fi

echo "All SaveSync checks passed."
