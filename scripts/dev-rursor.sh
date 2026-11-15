#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VSCODE_TAG="${VSCODE_TAG:-1.96.4}"
VSCODE_DIR="${VSCODE_DIR:-${ROOT_DIR}/fork/.vscode-src}"

FORCE_COMPILE=0
FORCE_CI=0
RELEASE_SIDECAR=0
NO_LAUNCH=0
CODE_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --compile)
      FORCE_COMPILE=1
      shift
      ;;
    --ci)
      FORCE_CI=1
      FORCE_COMPILE=1
      shift
      ;;
    --release-sidecar)
      RELEASE_SIDECAR=1
      shift
      ;;
    --no-launch)
      NO_LAUNCH=1
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Usage: ./scripts/dev-rursor.sh [options] [workspace-or-code-args...]

Fast daily patched-fork development loop for Rursor IDE:
  --compile          Force `npm run compile` in fork/.vscode-src even if out/ exists
  --ci               Force `npm ci` + `npm run compile` in fork/.vscode-src
  --release-sidecar  Build rursor-sidecar in --release mode (default: debug)
  --no-launch        Prepare sidecar, extension, patches, and compile without launching Electron
  -h, --help         Show this help message
EOF
      exit 0
      ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do
        CODE_ARGS+=("$1")
        shift
      done
      ;;
    *)
      CODE_ARGS+=("$1")
      shift
      ;;
  esac
done

echo "=== 1. Building rursor-sidecar ==="
if [[ "${RELEASE_SIDECAR}" == "1" ]]; then
  cargo build --release --manifest-path "${ROOT_DIR}/sidecar/Cargo.toml"
  SIDECAR_BIN="${ROOT_DIR}/sidecar/target/release/rursor-sidecar"
else
  cargo build --manifest-path "${ROOT_DIR}/sidecar/Cargo.toml"
  SIDECAR_BIN="${ROOT_DIR}/sidecar/target/debug/rursor-sidecar"
fi

echo "=== 2. Compiling built-in extension (rursor-ai) & bundling sidecar ==="
pushd "${ROOT_DIR}/extension" >/dev/null
if [[ ! -d node_modules ]]; then
  npm install
fi
npm run compile
mkdir -p bin
cp "${SIDECAR_BIN}" bin/rursor-sidecar
chmod +x bin/rursor-sidecar
popd >/dev/null

echo "=== 3. Syncing Rursor product.json, icons, rursor-ai extension & fork patches ==="
if [[ ! -d "${VSCODE_DIR}" ]]; then
  echo "Cloning Code - OSS (${VSCODE_TAG}) into ${VSCODE_DIR}..."
  git clone --depth 1 --branch "${VSCODE_TAG}" https://github.com/microsoft/vscode.git "${VSCODE_DIR}"
fi

cp "${ROOT_DIR}/fork/product.json" "${VSCODE_DIR}/product.json"
cp "${ROOT_DIR}/fork/resources/darwin/rursor.icns" "${VSCODE_DIR}/resources/darwin/code.icns"
cp "${ROOT_DIR}/fork/resources/darwin/rursor.icns" "${VSCODE_DIR}/resources/darwin/rursor.icns"

rm -rf "${VSCODE_DIR}/extensions/rursor-ai"
mkdir -p "${VSCODE_DIR}/extensions/rursor-ai"
cp -R \
  "${ROOT_DIR}/extension/package.json" \
  "${ROOT_DIR}/extension/.vscodeignore" \
  "${ROOT_DIR}/extension/out" \
  "${ROOT_DIR}/extension/bin" \
  "${VSCODE_DIR}/extensions/rursor-ai/"

for optional_asset in themes media icons images package.nls.json; do
  if [[ -e "${ROOT_DIR}/extension/${optional_asset}" ]]; then
    cp -R "${ROOT_DIR}/extension/${optional_asset}" "${VSCODE_DIR}/extensions/rursor-ai/"
  fi
done
chmod +x "${VSCODE_DIR}/extensions/rursor-ai/bin/rursor-sidecar"

PATCHES_APPLIED=0
shopt -s nullglob
for patch_file in "${ROOT_DIR}"/fork/patches/*.patch; do
  patch_name="$(basename "${patch_file}")"
  if git -C "${VSCODE_DIR}" apply --reverse --check "${patch_file}" >/dev/null 2>&1; then
    echo "  [ok] ${patch_name} already applied"
  elif git -C "${VSCODE_DIR}" apply --check "${patch_file}"; then
    echo "  [apply] ${patch_name}"
    git -C "${VSCODE_DIR}" apply "${patch_file}"
    PATCHES_APPLIED=$((PATCHES_APPLIED + 1))
  else
    echo "ERROR: Patch ${patch_name} failed git apply --check in ${VSCODE_DIR}." >&2
    echo "       Reset ${VSCODE_DIR}/src with 'git -C \"${VSCODE_DIR}\" checkout -- src/' and retry." >&2
    exit 1
  fi
done
shopt -u nullglob

if [[ -d "${VSCODE_DIR}/.build/electron" && ! -d "${VSCODE_DIR}/.build/electron/Rursor.app" ]]; then
  echo "Clearing non-Rursor .build/electron bundle so Rursor.app is staged cleanly..."
  rm -rf "${VSCODE_DIR}/.build/electron"
fi

if [[ ! -d "${VSCODE_DIR}/out" || "${FORCE_COMPILE}" == "1" || "${PATCHES_APPLIED}" -gt 0 ]]; then
  echo "=== 4. Compiling patched Rursor workbench in ${VSCODE_DIR} ==="
  pushd "${VSCODE_DIR}" >/dev/null
  if [[ ! -d node_modules || "${FORCE_CI}" == "1" ]]; then
    npm ci
  fi
  npm run compile
  popd >/dev/null
else
  echo "=== 4. Skipping workbench compile (${VSCODE_DIR}/out exists; pass --compile to rebuild) ==="
fi

if [[ "${NO_LAUNCH}" == "1" ]]; then
  echo "=== 5. Ready (--no-launch set; skipping Electron launch) ==="
  exit 0
fi

echo "=== 5. Launching patched Rursor IDE workbench ==="
exec "${VSCODE_DIR}/scripts/code.sh" ${CODE_ARGS[@]+"${CODE_ARGS[@]}"}
