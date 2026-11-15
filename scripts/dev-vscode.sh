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
mkdir -p bin
cp "${ROOT_DIR}/sidecar/target/debug/rursor-sidecar" bin/rursor-sidecar
chmod +x bin/rursor-sidecar
popd >/dev/null

if command -v code >/dev/null 2>&1; then
  HOST_CMD="code"
elif [[ -x "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code" ]]; then
  HOST_CMD="/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"
elif [[ -x "/Applications/Antigravity IDE.app/Contents/Resources/app/bin/antigravity" ]]; then
  HOST_CMD="/Applications/Antigravity IDE.app/Contents/Resources/app/bin/antigravity"
else
  echo "Neither 'code' CLI nor VS Code / Antigravity IDE CLI was found in PATH."
  echo "Extension compiled successfully at ${ROOT_DIR}/extension/out."
  exit 1
fi

echo "Launching ${HOST_CMD} with Rursor extension development host..."
"${HOST_CMD}" --extensionDevelopmentPath="${ROOT_DIR}/extension" "${ROOT_DIR}"
