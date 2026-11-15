# Rursor

A macOS (`arm64`) AI-native code editor built on a thin **Code - OSS (VS Code)** fork, a built-in TypeScript extension, and a **Rust** local AI sidecar powered by the **Gemini Cloud API** (with optional local Ollama / MLX Apple Silicon autocomplete).

## Features

- **`Cmd+K` Inline Edit & Red/Green Hunk Diff**:
  - QuickPick prompt with presets (`Fix diagnostics`, `Refactor`, `Error handling`, `Unit tests`, `Docstrings`) and recent prompt history.
  - Interleaved red (removed) / green (added) inline editor diff with CodeLens **Accept (`Cmd+Y`) / Revert (`Cmd+N`) / Refine** and `Fix with Rursor AI` Quick Fix actions.
- **Tab Autocomplete (Cloud + Local Hybrid)**:
  - Low-latency FIM completions (`gemini-2.5-flash-lite`) with recent-file and recent-edit hints, caching, and status bar **Tab: On/Off** toggle.
  - Configurable `rursor.autocomplete.backend` (`gemini`, `auto`, `ollama`, `mlx`) with local endpoint & model settings for zero-latency Apple Silicon completions.
- **`Cmd+L` Agent & Ask Sidebar**:
  - **Agent mode**: Multi-turn Gemini function calling (`read_file`, `list_files`, `search_workspace`, `search_symbols`, `grep_workspace`, `git_context`, `edit_file`, `write_file`, `run_command`, plus MCP tools).
  - **Interactive Review & Live Terminal**: `View Diff` (`vscode.diff`), per-file `Apply` / `Revert` / `Reject`, sticky **Multi-File Review Bar** (`Accept All` / `Revert All`), and real-time streaming shell command output.
  - **Inline `@` Mention Popover & Multimodal Input**: Type `@` for `@workspace`, `@git`, `@errors`, or file chips; paste screenshots or click `+ Img` to attach images to Gemini.
  - **Persistent Sessions**: Switch, create (`+ New`), and delete saved workspace conversations (`~/.rursor/sessions/`).
- **`.gitignore`-Aware Symbol Indexer, Project Rules & MCP**:
  - Respects `.gitignore`, extracts code symbols with BM25-weighted ranking, and updates incrementally on save/create/delete.
  - Auto-loads `.rursorrules`, `.cursorrules`, `AGENTS.md`, `CLAUDE.md`, and `.rursor/rules/*.md`.
  - Connects to stdio MCP servers configured in `.rursor/mcp.json` or `~/.rursor/mcp.json` (`Rursor: Reload MCP Servers`).
- **OS Keychain Key Storage**: Stores your Gemini API key via VS Code `SecretStorage`.

## Architecture

- [`fork/`](fork/) — Thin VS Code (`Code - OSS`) overlay: `product.json` branding, custom `rursor.icns` macOS icon, Open VSX marketplace config, `build-rursor-arm64.sh`, and `package-dmg.sh`.
- [`extension/`](extension/) — Built-in VS Code extension (`rursor-ai`).
- [`sidecar/`](sidecar/) — `rursor-sidecar` async Rust daemon (`tokio` + `reqwest` + `serde`).

## Prerequisites (macOS Apple Silicon)

- macOS `arm64` with Xcode Command Line Tools
- Node.js (>= v20.18.0) + Python 3
- Rust toolchain (`rustc` / `cargo`)
- **Disk space for full `.app` build:** Building `microsoft/vscode` (`Code - OSS`) + Electron + native modules requires **at least 15–20 GB of free disk space**.

## Quick Start (Fast Dev Loop — Seconds)

You do **not** need to wait for the full Electron build to develop and use Rursor:

1. Run `./scripts/dev-vscode.sh` (builds `rursor-sidecar`, compiles `extension/`, and launches VS Code / Antigravity IDE with `--extensionDevelopmentPath`).
2. Run Command Palette -> **Rursor: Set Gemini API Key** (stored in macOS Keychain).
3. Use `Cmd+K` for Inline Edit, `Tab` for Autocomplete, and `Cmd+L` for the Agent Sidebar.

## Full `Rursor.app` & `.dmg` Build (Run Later Whenever Ready)

```bash
./fork/scripts/build-rursor-arm64.sh
./fork/scripts/package-dmg.sh
```

Outputs `fork/VSCode-darwin-arm64/Rursor.app` and `fork/Rursor-arm64.dmg`.
