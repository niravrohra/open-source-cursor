# Rursor

A standalone macOS (`arm64` Apple Silicon) AI-native IDE built on a thin **Code - OSS (`1.96.4`)** fork, a built-in TypeScript workbench integration (`rursor-ai`), and an async **Rust** daemon (`rursor-sidecar`) powered by **Gemini**, **Anthropic / OpenAI BYOK Smart Routing**, and local Apple Silicon **Ollama / MLX**.

## Highlights

- **Pure Native Rursor IDE Shell (`fork/`)**:
  - **Right-Docked Agent Bar (`Cmd+L`)**: Opens `rursor-sidebar` natively in the Right Auxiliary Bar (`360px` default width) while keeping File Explorer untouched on the left and hiding stock VS Code Chat/Copilot chrome.
  - **Floating Monaco `ZoneWidget` (`Cmd+K`)**: Opens a native inline prompt bar right above your cursor line (`03-rursor-monaco-inline-widget.patch`), streams live red/green diff previews per chunk, and shows CodeLens **Accept (`Cmd+Y`) / Revert (`Cmd+N`) / Refine**.
  - **Rursor Obsidian Default Theme & Dedicated Settings (`Cmd+Shift+J`)**: Ships with the high-contrast **Rursor Obsidian** dark theme (`#0B0D12` / `#131822` + `#7C5CFF` electric violet) and a 6-tab **Rursor Settings** webview editor (Models & Keys, Tab & NES, Agent & Sandbox, Rules & Memory, MCP Servers, Indexing & Privacy).
- **Compiler-Grade Context, AST Syntax Gate & Repo Map (`sidecar/` + `extension/`)**:
  - **Tree-sitter AST Index + Hybrid RRF Search**: Incremental Tree-sitter symbol/call-edge indexing (`ast_index.rs`) fused with SQLite FTS5 BM25 + 128-dim cosine vectors (`context.rs`).
  - **Personalized PageRank Repo Map**: 12-iteration Personalized PageRank (`repomap.rs`) over cross-file call/import edges injected automatically as `<repo_map>` in Agent turns.
  - **160ms Live LSP Grounding & Two-Stage Verification**: Injects hover types, definitions, callees, and diagnostics (`lspContext.ts`) into `Cmd+K`, gates mutations through a pre-write Tree-sitter syntax check (`syntax_gate.rs` + 1-pass auto-self-repair), and feeds post-write `waitForLspDiagnosticsDelta` (`220ms`) back to the Agent.
  - **Temporal Developer Intent Ledger**: Tracks file visits, edits, terminal runs, and diagnostics (`<developer_intent>`) across `Cmd+K`, Agent chat, and Next-Edit Prediction (`nes.rs`).
- **Native Fork Superpowers (`nativeForkFeatures.ts`)**:
  - **AI Session Gutter (`QuickDiffProvider`)**: Live editor gutter bars tracking all AI-authored deltas since session start.
  - **Multi-File AI Changes Review (`Alt+Cmd+D`)**: Opens `vscode.changes` MultiDiffEditor across every AI-touched buffer.
  - **Inline Diagnostics Lens (`EditorInset`)**, **Terminal Failure Quick Fix (`TerminalQuickFixProvider`)**, **AI Rename Suggestions (`NewSymbolNamesProvider`)**, and **Live DAP Debugger Frame Inspector**.
- **macOS Apple Silicon Exclusives (`macos_sandbox.rs` + `voiceAndTelemetry.ts`)**:
  - **Instant APFS Copy-on-Write Sandboxes**: Zero-extra-block workspace clones (`cp -c -R` / `clonefile`) under `~/.rursor/cow-clones/` for isolated parallel tasks (`Rursor: Manage APFS CoW Sandboxes`).
  - **macOS Seatbelt (`sandbox-exec`) Profiles**: Command risk classification (`safe` / `network_or_build` / `destructive`) and kernel-enforced filesystem/credential isolation (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube` denied).
  - **Spatial Voice-to-Code (`Cmd+Shift+V`)** & **Live Telemetry / Context-Cache HUD** in the status bar.

## Key Shortcuts

| Shortcut | Action |
|---|---|
| `Cmd+K` | Native floating Monaco inline edit (`ZoneWidget`) + streaming diff |
| `Cmd+L` | Focus Right-Docked Rursor Agent & Ask pane |
| `Tab` / `Cmd+Right` | Ghost-text FIM autocomplete, Next-Edit Jump (`NES`), or word-by-word accept |
| `Cmd+Shift+J` | Open **Rursor Settings** (`Models`, `Tab & NES`, `Sandbox`, `Rules`, `MCP`, `Privacy`) |
| `Cmd+Shift+V` | Spatial **Voice Prompt** (`Cmd+K` when selection active, Agent pane otherwise) |
| `Alt+Cmd+D` | Open **Multi-File AI Changes** (`vscode.changes` MultiDiffEditor) |
| `Cmd+Y` / `Cmd+N` | Accept / Revert inline `Cmd+K` diff |

## Architecture

- `fork/` — Code - OSS `1.96.4` overlay: `product.json` (branding + 9 unlocked proposed APIs), `patches/01..05` (Right Auxiliary Bar, custom Rursor Welcome & Titlebar pill, Monaco `ZoneWidget`, `auxiliarybar` container + stock chat removal, Rursor Obsidian default theme), `rursor.icns`, entitlements, and `arm64` build/DMG scripts.
- `extension/` — Built-in `rursor-ai` workbench integration (`extension.ts`, `agentPanel.ts`, `inlineEdit.ts`, `inlineCompletion.ts`, `lspContext.ts`, `nativeForkFeatures.ts`, `settingsPanel.ts`, `voiceAndTelemetry.ts`, `webPreview.ts`, `themes/rursor-obsidian-dark.json`).
- `sidecar/` — Async Rust engine (`main.rs`, `ast_index.rs`, `context.rs`, `repomap.rs`, `syntax_gate.rs`, `macos_sandbox.rs`, `nes.rs`, `router.rs`, `worktree.rs`, `pty_runner.rs`, `subagents.rs`, `memory.rs`, `mcp.rs`, `gemini.rs`, `local_llm.rs`).
- `scripts/rursor` & `eval/` — macOS `rursor` CLI (`open`, `ask`, `agent`, `status`, `eval`, `install-cli`) and `rursor-eval` deterministic benchmark suite (`eval/run_eval.py`).

## Quick Start

### 1. Fast Patched Rursor IDE Dev Mode (Recommended)
Applies all 5 Rursor fork patches (`01..05`) into `fork/.vscode-src`, bundles `rursor-sidecar` + `rursor-ai`, compiles once (`out/`), and launches the real patched **Rursor** Electron workbench:

```bash
./scripts/dev-rursor.sh
```

*(If you want to test only the extension inside an existing VS Code / Antigravity host without compiling `fork/.vscode-src`, run `./scripts/dev-vscode.sh`.)*

### 2. Rursor macOS CLI & Benchmark Harness

```bash
./scripts/rursor status          # Sidecar health, sub-millisecond RPC latency & index stats
./scripts/rursor eval            # Run deterministic 21-task retrieval, AST, Seatbelt & CoW suite
./scripts/rursor ask "How does Personalized PageRank repo_map work?"
./scripts/rursor install-cli     # Symlink `rursor` onto your PATH
```

### 3. Full Standalone `Rursor.app` & `Rursor-arm64.dmg` Build (~20 min, ~15–20 GB disk)

```bash
./fork/scripts/build-rursor-arm64.sh
./fork/scripts/package-dmg.sh
```

Outputs `fork/VSCode-darwin-arm64/Rursor.app` and `fork/Rursor-arm64.dmg`.
