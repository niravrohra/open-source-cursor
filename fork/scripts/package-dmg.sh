#!/usr/bin/env bash
set -euo pipefail

FORK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_PATH="${1:-${FORK_DIR}/VSCode-darwin-arm64/Rursor.app}"
DMG_PATH="${2:-${FORK_DIR}/Rursor-arm64.dmg}"

if [[ ! -d "${APP_PATH}" ]]; then
  echo "ERROR: App bundle not found at ${APP_PATH}" >&2
  echo "Run ${FORK_DIR}/scripts/build-rursor-arm64.sh first." >&2
  exit 1
fi

STAGING_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGING_DIR}"' EXIT

echo "=== Staging Rursor.app for DMG ==="
ditto "${APP_PATH}" "${STAGING_DIR}/Rursor.app"

chmod +x "${FORK_DIR}/scripts/sign-rursor-app.sh"
"${FORK_DIR}/scripts/sign-rursor-app.sh" "${STAGING_DIR}/Rursor.app"

ln -s /Applications "${STAGING_DIR}/Applications"

rm -f "${DMG_PATH}"

echo "=== Creating ${DMG_PATH} ==="
hdiutil create \
  -volname "Rursor" \
  -srcfolder "${STAGING_DIR}" \
  -ov \
  -format UDZO \
  "${DMG_PATH}"

echo "DMG created: ${DMG_PATH}"
ls -lh "${DMG_PATH}"
