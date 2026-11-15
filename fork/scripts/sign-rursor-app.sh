#!/usr/bin/env bash
set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_PATH="${1:-${FORK_DIR}/VSCode-darwin-arm64/Rursor.app}"
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"
ENTITLEMENTS_PLIST="${FORK_DIR}/resources/darwin/entitlements.plist"

if [[ ! -d "${APP_PATH}" ]]; then
  echo "ERROR: App bundle not found at ${APP_PATH}" >&2
  exit 1
fi

echo "=== Preparing & signing $(basename "${APP_PATH}") (identity: ${CODESIGN_IDENTITY}) ==="

# 1. Strip quarantine / Finder extended attributes that break sealed bundle verification
xattr -cr "${APP_PATH}" 2>/dev/null || true

# 2. Ensure macOS TCC usage descriptions exist in Info.plist (Voice-to-Code, AppleEvents, Camera)
INFO_PLIST="${APP_PATH}/Contents/Info.plist"
if [[ -f "${INFO_PLIST}" ]]; then
  plutil -replace NSMicrophoneUsageDescription -string \
    "Rursor uses the microphone for Voice-to-Code (Cmd+Shift+V)." "${INFO_PLIST}"
  plutil -replace NSAppleEventsUsageDescription -string \
    "Rursor uses AppleScript for external terminal and automation integration." "${INFO_PLIST}"
  plutil -replace NSCameraUsageDescription -string \
    "An extension in Rursor wants to use the camera." "${INFO_PLIST}"
fi

# 3. Ensure executable bit on embedded Rust sidecar and CLI launcher
SIDECAR_BIN="${APP_PATH}/Contents/Resources/app/extensions/rursor-ai/bin/rursor-sidecar"
if [[ -f "${SIDECAR_BIN}" ]]; then
  chmod +x "${SIDECAR_BIN}"
fi
if [[ -f "${APP_PATH}/Contents/Resources/app/bin/code" ]]; then
  chmod +x "${APP_PATH}/Contents/Resources/app/bin/code"
fi

SIGN_FLAGS=(--force --sign "${CODESIGN_IDENTITY}")
if [[ "${CODESIGN_IDENTITY}" != "-" ]]; then
  SIGN_FLAGS+=(--options runtime --timestamp)
fi

# 4. Inside-out pass A: sign standalone Mach-O binaries & native .node addons under Contents/Resources
#    (`codesign --deep` does not traverse Contents/Resources, so embedded sidecars/ripgrep/node-pty must be signed first)
if [[ -d "${APP_PATH}/Contents/Resources" ]]; then
  while IFS= read -r -d '' candidate; do
    if file -b "${candidate}" 2>/dev/null | grep -q "Mach-O"; then
      codesign "${SIGN_FLAGS[@]}" --entitlements "${ENTITLEMENTS_PLIST}" "${candidate}"
    fi
  done < <(find "${APP_PATH}/Contents/Resources" -type f \( -perm +111 -o -name "*.node" -o -name "*.dylib" -o -name "*.so" \) -print0)
fi

# 5. Inside-out pass B: sign nested frameworks and Electron helper .app bundles
if [[ -d "${APP_PATH}/Contents/Frameworks" ]]; then
  while IFS= read -r -d '' dylib_path; do
    if file -b "${dylib_path}" 2>/dev/null | grep -q "Mach-O"; then
      codesign "${SIGN_FLAGS[@]}" "${dylib_path}"
    fi
  done < <(find "${APP_PATH}/Contents/Frameworks" -type f \( -name "*.dylib" -o -name "*.so" \) -print0)

  for bundle in "${APP_PATH}/Contents/Frameworks/"*.framework "${APP_PATH}/Contents/Frameworks/"*.app; do
    if [[ -e "${bundle}" ]]; then
      codesign "${SIGN_FLAGS[@]}" --deep --entitlements "${ENTITLEMENTS_PLIST}" "${bundle}"
    fi
  done
fi

# 6. Sign top-level Rursor.app bundle
codesign "${SIGN_FLAGS[@]}" --deep --entitlements "${ENTITLEMENTS_PLIST}" "${APP_PATH}"

# 7. Strict verification of outer bundle + embedded sidecar
if [[ -f "${SIDECAR_BIN}" ]]; then
  codesign --verify --strict "${SIDECAR_BIN}"
fi
codesign --verify --deep --strict "${APP_PATH}"

echo "Code signature verified: ${APP_PATH}"
