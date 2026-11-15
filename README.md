<div align="center">

# Rursor

**An open, Apple Silicon–native AI code editor built on a thin `Code - OSS` fork, a lightweight TypeScript extension, and an async Rust sidecar powered directly by the Gemini Cloud API.**

[![Platform](https://img.shields.io/badge/macOS-Apple%20Silicon%20(arm64)-000000?style=for-the-badge&logo=apple&logoColor=white)](#full-rursorapp-darwin-arm64-build)
[![Sidecar](https://img.shields.io/badge/Sidecar-Rust%20%2B%20Tokio-ea580c?style=for-the-badge&logo=rust&logoColor=white)](#3-native-rust-sidecar-sidecar)
[![AI Engine](https://img.shields.io/badge/AI-Gemini%202.5%20SSE-0284c7?style=for-the-badge&logo=googlegemini&logoColor=white)](#why-rursor-vs-cursor--antigravity)
[![License](https://img.shields.io/badge/Fork-Code%20--%20OSS%20(MIT)-6366f1?style=for-the-badge)](#1-thin-code---oss-fork-fork)

</div>

---

## Product Intro Video

> Watch or download the 28-second 1080p launch intro ([`rursor-intro.mp4`](./rursor-intro.mp4)):

<video src="./rursor-intro.mp4" controls width="100%" preload="metadata">
  Your markdown viewer does not support inline HTML5 video. <a href="./rursor-intro.mp4">Open rursor-intro.mp4</a> directly.
</video>

> **Local preview tip:** In VS Code / macOS Finder, open [`./rursor-intro.mp4`](./rursor-intro.mp4) directly (`open rursor-intro.mp4`).

---

## Why Rursor? (vs. Cursor & Antigravity)

Most AI editors force a trade-off between **closed-source SaaS wrappers**, **monthly per-seat markups**, and **opaque background telemetry**. Rursor takes a deliberately different engineering path: **own your editor, own your API key, and keep the AI runtime fast and auditable in Rust.**

| Capability | **Rursor** | **Cursor** | **Antigravity** |
| :--- | :--- | :--- | :--- |
| **Pricing & API Billing** | **100% BYOK** — pay Google AI directly with your own Gemini API key; zero seat markup or credit caps | Subscription tiers + metered fast-request / usage-based overages | Managed account / cloud-hosted quota tiers |
| **Credential Storage** | **macOS OS Keychain** via VS Code `SecretStorage` (`rursor.geminiApiKey`) | Proprietary cloud auth session | Cloud OAuth / platform session |
| **Core AI Sidecar** | **Compiled Rust binary (`rursor-sidecar`)** on `tokio` + `reqwest` (`rustls-tls`) over stdio JSON-RPC | Closed-source TypeScript/Node + remote proxy backend | Multi-process Electron + cloud orchestration sidecars |
| **Network Architecture** | **Direct client → Gemini SSE (`streamGenerateContent?alt=sse`)** — your code never transits a third-party SaaS proxy | Code & prompts routed through Cursor's backend infrastructure | Prompts routed through Antigravity's platform services |
| **Editor Base & Extensions** | **Thin `Code - OSS` fork** (`com.rursor.editor`) + **Open VSX** marketplace, zero Microsoft telemetry | Closed proprietary VS Code fork with custom marketplace rules | Custom VS Code fork + standalone desktop workspace app |
| **Cancellation Latency** | **Instant `JoinHandle::abort()`** per `requestId` in Rust drops in-flight HTTP/SSE streams immediately | Remote stream cancellation over proxy | Agent task cancellation via runtime controller |
| **Hackability & Transparency** | **< 2,000 LOC** across `sidecar/` + `extension/` — inspect, modify, and compile every line locally | Closed source | Closed source |

### Where Rursor Wins in Practice

1. **Direct-to-Model Speed Without Proxy Tax**
   Cursor and hosted AI IDEs route your context through intermediary servers for billing, routing, and telemetry. Rursor's Rust sidecar opens a direct TLS (`rustls`) Server-Sent Events stream straight to `generativelanguage.googleapis.com`, cutting out middleman hops and keeping your codebase between your Mac and Google's API.

2. **Rust-Native Indexing & Concurrency Instead of Heavy Node Processes**
   Indexing repositories, watching saves, and ranking context in JavaScript competes with the extension host thread. Rursor offloads workspace walking, lexical/symbol scoring, `grep_workspace`, and concurrent SSE parsing into a single lightweight compiled Rust binary (`rursor-sidecar`), leaving the editor UI snappy even on large repos.

3. **Zero Vendor Lock-In, Zero Subscription Fatigue**
   Swap between `gemini-2.5-flash` (fast edits & agent turns), `gemini-2.5-pro` (deep reasoning), and `gemini-2.5-flash-lite` (low-latency Tab autocomplete) on demand—billed purely at raw API rates via your macOS Keychain key.

4. **Thin, Maintainable `Code - OSS` Fork**
   Instead of maintaining a massive diverged fork that breaks on every VS Code release, Rursor keeps its fork surface razor-thin (`fork/product.json` + one built-in `rursor-ai` extension). Rebasing onto newer upstream `microsoft/vscode` tags takes minutes, not weeks.

5. **Focused Craft Over Feature Bloat**
   Rather than bundling heavy remote containers, proprietary telemetry pipelines, and background cloud sync like Cursor or full multi-workspace orchestration daemons like Antigravity, Rursor delivers the core 95% of daily AI coding velocity—**Tab Autocomplete**, **`Cmd+K` Inline Edit**, **`Cmd+L` Agentic Chat with file/command tools**, and **CodeLens Keep/Revert**—in a clean macOS `arm64` bundle.

---

## Architecture Overview

Rursor splits responsibilities cleanly across three layers communicating over deterministic, line-delimited JSON-RPC (`stdin` / `stdout`):

```
┌───────────────────────────────────────────────────────────────────────────┐
│  1. Thin Code - OSS Fork (fork/)                                          │
│     • com.rursor.editor · rursor:// URL scheme · Open VSX gallery         │
│     • macOS darwin-arm64 Electron app bundle (Rursor.app)                 │
└────────────────────────────────────┬──────────────────────────────────────┘
                                     │ built-in extension
┌────────────────────────────────────▼──────────────────────────────────────┐
│  2. Built-in TypeScript Extension (extension/)                            │
│     • Tab Autocomplete (inlineCompletion.ts, 260ms debounce + LRU cache)  │
│     • Cmd+K Inline Edit (inlineEdit.ts, green diff + CodeLens Keep/Revert)│
│     • Cmd+L Agent Sidebar (agentPanel.ts, @workspace, rursor-diff://)     │
│     • OS Keychain SecretStorage + FileSystemWatcher incremental sync      │
└────────────────────────────────────┬──────────────────────────────────────┘
                                     │ line-delimited JSON-RPC (stdin/stdout)
┌────────────────────────────────────▼──────────────────────────────────────┐
│  3. Native Rust Sidecar (sidecar/ · rursor-sidecar)                        │
│     • Tokio async runtime + per-request JoinHandle cancellation           │
│     • Workspace index & lexical/symbol scorer + grep_workspace            │
│     • Multi-step Agent tool loop (up to 12 steps: edit/write/run_command) │
│     • Direct Gemini SSE streaming (reqwest + rustls-tls)                  │
└───────────────────────────────────────────────────────────────────────────┘
```

---

## Core Features

### 1. Thin `Code - OSS` Fork (`fork/`)
- Custom macOS identity (`Rursor`, `com.rursor.editor`, `rursor://` protocol, `.rursor` data folder) defined in [`fork/product.json`](./fork/product.json).
- Pre-configured with **Open VSX** (`open-vsx.org`) for extensions without proprietary marketplace lock-in.
- Automated preflight environment verification ([`fork/scripts/check-env.sh`](./fork/scripts/check-env.sh)) and Apple Silicon packaging ([`fork/scripts/build-rursor-arm64.sh`](./fork/scripts/build-rursor-arm64.sh)).

### 2. Built-in AI Extension (`extension/`)
- **Tab Autocomplete ([`inlineCompletion.ts`](./extension/src/inlineCompletion.ts))**: Context-aware ghost text powered by `gemini-2.5-flash-lite` with `260ms` keystroke debounce, 60-entry prefix/suffix cache, recent-file context injection, and a one-click status bar toggle (`Zap Tab: On/Off`).
- **Inline Edit `Cmd+K` ([`inlineEdit.ts`](./extension/src/inlineEdit.ts))**: Captures 80 lines of prefix/suffix context around your cursor or selection, streams replacements from Gemini, highlights changed lines in green, and attaches interactive **CodeLens (`Keep` | `Revert`)** plus **Quick Fix** actions.
- **Agent Sidebar `Cmd+L` ([`agentPanel.ts`](./extension/src/agentPanel.ts))**: Multi-turn chat with `gemini-2.5-flash` / `gemini-2.5-pro` switching, automatic active-file attachment, `@workspace` search, `rursor-diff://` virtual diff previews, and interactive approval/execution for file edits and terminal commands.
- **Incremental Indexing ([`extension.ts`](./extension/src/extension.ts))**: Watches file creates, saves, and deletes via `FileSystemWatcher` and incrementally updates the Rust sidecar index (`update_file`, `remove_file`).

### 3. Native Rust Sidecar (`sidecar/`)
- **Typed JSON-RPC Protocol ([`protocol.rs`](./sidecar/src/protocol.rs))**: Handles `ping`, `status`, `set_config`, `index_workspace`, `update_file`, `remove_file`, `search_context`, `grep_workspace`, `inline_complete`, `inline_edit`, `agent_chat`, `submit_tool_result`, and `cancel`.
- **Direct Gemini SSE Client ([`gemini.rs`](./sidecar/src/gemini.rs))**: Streams `streamGenerateContent?alt=sse` over `rustls-tls` using the `x-goog-api-key` header, emitting real-time `stream_chunk` events to the editor.
- **Fast Context Engine ([`context.rs`](./sidecar/src/context.rs))**: Skips `.git`, `node_modules`, `target`, binary files, and oversized files (`> 512 KB`), ranking workspace matches by path and lexical/symbol relevance.
- **Concurrent Task Cancellation ([`main.rs`](./sidecar/src/main.rs))**: Every RPC call runs inside a tracked Tokio `JoinHandle` so hitting **Stop** or typing over an autocomplete request aborts the previous in-flight Gemini request immediately.

---

## Keyboard Shortcuts & Commands

| Shortcut / Command | Action | Description |
| :--- | :--- | :--- |
| **`Cmd + K`** | `rursor.inlineEdit` | Open Inline Edit at cursor/selection and stream a green diff with Keep/Revert CodeLens |
| **`Cmd + L`** | `rursor.openAgent` | Focus the Rursor Agent Sidebar with active file & `@workspace` context |
| **`Cmd + Shift + P`** → `Rursor: Set Gemini API Key` | `rursor.setApiKey` | Save your Gemini Cloud API key securely in the macOS Keychain |
| **`Cmd + Shift + P`** → `Rursor: Reindex Workspace` | `rursor.reindex` | Force a full workspace reindex in `rursor-sidecar` |
| **`Cmd + Shift + P`** → `Rursor: Show Sidecar Status` | `rursor.status` | Inspect sidecar uptime, active model, and indexed file count |
| Status bar **`Tab: On/Off`** | `rursor.toggleAutocomplete` | Toggle Gemini Tab ghost-text completions on or off |

---

## Prerequisites (macOS Apple Silicon)

- **Hardware / OS:** macOS on Apple Silicon (`arm64`) with Xcode Command Line Tools
- **Node.js:** v20+ (verified on `v22.22.0`) + `npm`
- **Rust:** `rustc` + `cargo`
- **Python 3** (required by Chromium / Code - OSS build tooling)
- **Disk Space:**
  - Extension + Sidecar dev loop: **< 500 MB**
  - Full standalone `Rursor.app` (`microsoft/vscode` clone + Electron + native modules): **at least 15–20 GB free**

---

## Quick Start (Fast Dev Loop Inside VS Code)

Iterate on Rursor in seconds without waiting for a full Electron build:

```bash
# 1. Build rursor-sidecar, compile extension/, and launch VS Code with --extensionDevelopmentPath
./scripts/dev-vscode.sh
```

2. In the launched window, open the Command Palette (`Cmd + Shift + P`) and run **`Rursor: Set Gemini API Key`**.
3. Try **`Cmd + K`** for Inline Edit, **`Cmd + L`** for the Agent Sidebar, or start typing for **Tab Autocomplete**.

---

## Full `Rursor.app` (`darwin-arm64`) Build

Once you have **15–20 GB** of free disk space and want to compile the standalone branded macOS application:

```bash
# Runs preflight checks, builds release rursor-sidecar, compiles rursor-ai,
# clones microsoft/vscode, applies fork/product.json, and packages Rursor.app
./fork/scripts/build-rursor-arm64.sh
```

The resulting native Apple Silicon bundle is output under:

```text
fork/VSCode-darwin-arm64/Rursor.app
```

---

## Project Structure

```text
rursor/
├── rursor-intro.mp4              # 1080p product intro video
├── README.md
├── scripts/
│   └── dev-vscode.sh             # Fast build + launch loop in VS Code
├── extension/                    # Built-in TypeScript extension (rursor-ai)
│   ├── package.json
│   ├── tsconfig.json
│   └── src/
│       ├── extension.ts          # Activation, Keychain secrets, FileSystemWatcher
│       ├── sidecarClient.ts      # JSON-RPC stdio client & tool event router
│       ├── inlineCompletion.ts   # Debounced Tab autocomplete provider
│       ├── inlineEdit.ts         # Cmd+K inline edit, diff highlights & CodeLens
│       └── agentPanel.ts         # Cmd+L Agent webview & rursor-diff:// provider
├── sidecar/                      # Native Rust AI daemon (rursor-sidecar)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs               # Tokio RPC loop, tool runtime, cancel handling
│       ├── protocol.rs           # Typed JSON-RPC requests, responses & events
│       ├── gemini.rs             # Direct Gemini SSE streaming client
│       └── context.rs            # Workspace indexer, scorer & grep
└── fork/                         # Thin Code - OSS overlay & macOS build scripts
    ├── product.json              # Rursor branding & Open VSX configuration
    └── scripts/
        ├── check-env.sh          # Toolchain & disk space preflight check
        └── build-rursor-arm64.sh # Standalone Rursor.app darwin-arm64 builder
```
