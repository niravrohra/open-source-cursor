#!/usr/bin/env bash
set -euo pipefail

MIN_FREE_GB=15

echo "=== Rursor macOS arm64 Build Environment Check ==="
arch_name="$(uname -m)"
echo "Architecture: ${arch_name}"
if [[ "${arch_name}" != "arm64" ]]; then
  echo "WARNING: Expected arm64 (Apple Silicon), found ${arch_name}"
fi

for cmd in git node npm python3 rustc cargo xcode-select; do
  if command -v "${cmd}" >/dev/null 2>&1; then
    echo "[OK] ${cmd}: $(command -v "${cmd}")"
  else
    echo "[MISSING] ${cmd}"
  fi
done

avail_kb="$(df -k . | awk 'NR==2 {print $4}')"
avail_gb="$((avail_kb / 1024 / 1024))"
echo "Available disk space: ~${avail_gb} GiB"

if (( avail_gb < MIN_FREE_GB )); then
  echo "WARNING: Full Code - OSS clone + arm64 Electron build typically needs >= ${MIN_FREE_GB} GiB free."
  echo "         Free up disk space before running the full VS Code fork build."
  exit 1
fi

echo "Environment looks ready for a full VS Code fork build."
