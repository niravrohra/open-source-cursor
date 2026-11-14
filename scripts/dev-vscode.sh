#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "Building rursor-sidecar (debug)..."
cargo build --manifest-path "${ROOT_DIR}/sidecar/Cargo.toml"

echo "Compiling rursor-ai extension..."
pushd "${ROOT_DIR}/extension" >/dev/null
if [[ ! -d node_modules ]]; then
  npm install
fi
npm run compile
popd >/dev/null

echo "Launching VS Code with Rursor extension development host..."
code --extensionDevelopmentPath="${ROOT_DIR}/extension" "${ROOT_DIR}"
