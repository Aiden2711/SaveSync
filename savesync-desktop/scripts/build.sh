#!/usr/bin/env bash
# Build the frontend into dist/ and produce a release desktop binary.
# The release binary embeds the frontend, so it does not need Vite to run.
set -euo pipefail

cd "$(dirname "$0")/.."

if [ ! -d node_modules ]; then
  echo "node_modules is missing. Run: npm install" >&2
  exit 1
fi

npm run build
cd src-tauri
cargo build --release

echo
echo "Binary: $(pwd)/target/release/savesync-desktop"
echo "For installers (.deb/.AppImage/.msi), install the Tauri CLI and run: npm run tauri build"
