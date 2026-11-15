#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VSCODE_TAG="${VSCODE_TAG:-1.96.4}"
VSCODE_DIR="${ROOT_DIR}/fork/.vscode-src"

"${ROOT_DIR}/fork/scripts/check-env.sh"

echo "=== 1. Building rursor-sidecar (release, arm64) ==="
cargo build --release --manifest-path "${ROOT_DIR}/sidecar/Cargo.toml"

echo "=== 2. Building built-in extension (rursor-ai) ==="
pushd "${ROOT_DIR}/extension" >/dev/null
if [[ ! -d node_modules ]]; then
  npm install
fi
npm run compile
mkdir -p bin
cp "${ROOT_DIR}/sidecar/target/release/rursor-sidecar" bin/rursor-sidecar
chmod +x bin/rursor-sidecar
popd >/dev/null

echo "=== 3. Fetching Code - OSS (${VSCODE_TAG}) ==="
if [[ ! -d "${VSCODE_DIR}" ]]; then
  git clone --depth 1 --branch "${VSCODE_TAG}" https://github.com/microsoft/vscode.git "${VSCODE_DIR}"
fi

echo "=== 4. Applying Rursor product.json, app icon & bundling rursor-ai ==="
cp "${ROOT_DIR}/fork/product.json" "${VSCODE_DIR}/product.json"
cp "${ROOT_DIR}/fork/resources/darwin/rursor.icns" "${VSCODE_DIR}/resources/darwin/code.icns"
rm -rf "${VSCODE_DIR}/extensions/rursor-ai"
mkdir -p "${VSCODE_DIR}/extensions/rursor-ai"
cp -R \
  "${ROOT_DIR}/extension/package.json" \
  "${ROOT_DIR}/extension/.vscodeignore" \
  "${ROOT_DIR}/extension/out" \
  "${ROOT_DIR}/extension/bin" \
  "${VSCODE_DIR}/extensions/rursor-ai/"
chmod +x "${VSCODE_DIR}/extensions/rursor-ai/bin/rursor-sidecar"

echo "=== 5. Building Rursor.app (darwin-arm64) ==="
pushd "${VSCODE_DIR}" >/dev/null
npm ci
npm run gulp vscode-darwin-arm64
popd >/dev/null

echo "Build complete. Output bundle is under ${ROOT_DIR}/fork/VSCode-darwin-arm64/Rursor.app"
