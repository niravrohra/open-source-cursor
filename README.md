# Rursor

A macOS (`arm64`) AI-native code editor built on a thin **Code - OSS (VS Code)** fork, a built-in TypeScript extension, and a **Rust** local AI sidecar powered by the **Gemini Cloud API**.

## Architecture

- [`fork/`](fork/) — Thin VS Code (`Code - OSS`) overlay: `product.json` branding, Open VSX marketplace config, and macOS `arm64` build/preflight scripts.
- [`extension/`](extension/) — Built-in VS Code extension (`rursor-ai`) providing:
  - `Cmd+K` Inline Edit with green line highlight & Keep/Revert flow
  - `Cmd+L` Agent Sidebar (`gemini-2.5-flash` / `gemini-2.5-pro`) with streaming output and `@workspace` search
  - Secure Gemini API key storage via VS Code `SecretStorage` (OS Keychain)
- [`sidecar/`](sidecar/) — `rursor-sidecar` async Rust daemon (`tokio` + `reqwest` + `serde`) communicating over line-delimited JSON-RPC (`stdin`/`stdout`) with:
  - Gemini Cloud API SSE streaming (`streamGenerateContent?alt=sse`) and request cancellation
  - Incremental workspace file walker & lexical/symbol search (`index_workspace`, `search_context`)

## Prerequisites (macOS Apple Silicon)

- macOS `arm64` with Xcode Command Line Tools
- Node.js (verified with v22.22.0) + Python 3
- Rust toolchain (`rustc` / `cargo`)
- **Disk space:** Building `microsoft/vscode` (`Code - OSS`) + Electron + native modules requires **at least 15–20 GB of free disk space**.

## Quick Start (Fast Extension + Sidecar Dev Loop)

While freeing disk space for the full `Rursor.app` Electron build, you can run and test Rursor directly inside VS Code:

1. Run `./scripts/dev-vscode.sh` (builds `rursor-sidecar`, compiles `extension/`, and launches VS Code with `--extensionDevelopmentPath`).
2. Run Command Palette -> **Rursor: Set Gemini API Key** (stored in macOS Keychain).
3. Use `Cmd+K` for Inline Edit or `Cmd+L` for the Agent Sidebar.

## Full `Rursor.app` (`darwin-arm64`) Build

Once you have >= 15–20 GB of free disk space:

```bash
./fork/scripts/build-rursor-arm64.sh
```
