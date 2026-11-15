import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { SidecarClient, ToolEventPayload, ToolRequestPayload } from './sidecarClient';

interface ChatTurn {
  role: 'user' | 'assistant';
  content: string;
}

interface PendingEditSnapshot {
  callId: string;
  relPath: string;
  absPath: string;
  beforeContent: string;
  afterContent: string;
  isNewFile: boolean;
  explanation: string;
  resolveDecision?: (approved: boolean) => void;
}

interface PendingCommandApproval {
  callId: string;
  command: string;
  resolveDecision: (approved: boolean) => void;
}

export class RursorDiffContentProvider implements vscode.TextDocumentContentProvider {
  private readonly contents = new Map<string, string>();
  private readonly emitter = new vscode.EventEmitter<vscode.Uri>();
  public readonly onDidChange = this.emitter.event;

  setContent(uri: vscode.Uri, content: string): void {
    this.contents.set(uri.toString(), content);
    this.emitter.fire(uri);
  }

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.contents.get(uri.toString()) ?? '';
  }

  dispose(): void {
    this.emitter.dispose();
    this.contents.clear();
  }
}

export class AgentPanelProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'rursor.agentView';
  private view: vscode.WebviewView | undefined;
  private history: ChatTurn[] = [];
  private activeRequestId: number | undefined;
  private attachedFiles = new Set<string>();
  private readonly editSnapshots = new Map<string, PendingEditSnapshot>();
  private readonly commandApprovals = new Map<string, PendingCommandApproval>();

  constructor(
    private readonly client: SidecarClient,
    private readonly diffProvider: RursorDiffContentProvider,
    private readonly onModelChange: (model: string) => Promise<void>
  ) {}

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    webviewView.webview.options = { enableScripts: true };
    webviewView.webview.html = this.renderHtml();

    webviewView.webview.onDidReceiveMessage(
      async (msg: {
        type: string;
        text?: string;
        model?: string;
        mode?: 'agent' | 'ask';
        includeWorkspace?: boolean;
        autoApply?: boolean;
        callId?: string;
        filePath?: string;
        code?: string;
      }) => {
        if (msg.type === 'send' && msg.text) {
          await this.handleUserPrompt(
            msg.text,
            msg.mode ?? 'agent',
            Boolean(msg.includeWorkspace),
            Boolean(msg.autoApply),
            msg.model
          );
        } else if (msg.type === 'stop') {
          await this.cancelActiveRequest();
        } else if (msg.type === 'clear') {
          this.history = [];
        } else if (msg.type === 'setKey') {
          await vscode.commands.executeCommand('rursor.setApiKey');
        } else if (msg.type === 'modelChanged' && msg.model) {
          await this.onModelChange(msg.model);
        } else if (msg.type === 'pickFile') {
          await this.pickContextFile();
        } else if (msg.type === 'removeFile' && msg.filePath) {
          this.attachedFiles.delete(msg.filePath);
          this.syncContextBar();
        } else if (msg.type === 'editDecision' && msg.callId) {
          const snap = this.editSnapshots.get(msg.callId);
          snap?.resolveDecision?.(msg.text === 'approve');
        } else if (msg.type === 'viewDiff' && msg.callId) {
          await this.openDiffForCall(msg.callId);
        } else if (msg.type === 'revertEdit' && msg.callId) {
          await this.revertAppliedEdit(msg.callId);
        } else if (msg.type === 'commandDecision' && msg.callId) {
          const pending = this.commandApprovals.get(msg.callId);
          pending?.resolveDecision(msg.text === 'approve');
        } else if (msg.type === 'insertCode' && msg.code) {
          await this.insertSnippetAtCursor(msg.code);
        }
      }
    );

    this.syncContextBar();
  }

  private async cancelActiveRequest(): Promise<void> {
    for (const snap of this.editSnapshots.values()) {
      snap.resolveDecision?.(false);
      snap.resolveDecision = undefined;
    }
    for (const cmd of this.commandApprovals.values()) {
      cmd.resolveDecision(false);
    }
    this.commandApprovals.clear();

    if (this.activeRequestId !== undefined) {
      await this.client.cancel(this.activeRequestId);
      this.activeRequestId = undefined;
    }
    void this.view?.webview.postMessage({ type: 'done' });
  }

  private async pickContextFile(): Promise<void> {
    try {
      const { promise } = this.client.request<{ files: string[] }>('list_files', { limit: 600 });
      const res = await promise;
      const files = res.files ?? [];
      if (files.length === 0) {
        vscode.window.showInformationMessage('No indexed workspace files found yet.');
        return;
      }
      const chosen = await vscode.window.showQuickPick(files, {
        placeHolder: 'Attach file to Rursor context (@file)',
      });
      if (chosen) {
        this.attachedFiles.add(chosen);
        this.syncContextBar();
      }
    } catch {
      // Ignore if sidecar not ready
    }
  }

  private syncContextBar(): void {
    const active = vscode.window.activeTextEditor;
    const activeLabel = active ? vscode.workspace.asRelativePath(active.document.uri, false) : '';
    void this.view?.webview.postMessage({
      type: 'contextSync',
      activeFile: activeLabel,
      attachedFiles: Array.from(this.attachedFiles),
    });
  }

  private async handleUserPrompt(
    prompt: string,
    mode: 'agent' | 'ask',
    includeWorkspace: boolean,
    autoApply: boolean,
    model?: string
  ): Promise<void> {
    this.history.push({ role: 'user', content: prompt });

    const contextFiles: Array<{ path: string; content: string }> = [];
    const seenPaths = new Set<string>();

    const active = vscode.window.activeTextEditor;
    if (active) {
      const doc = active.document;
      const rel = vscode.workspace.asRelativePath(doc.uri, false);
      const content = active.selection.isEmpty
        ? doc.getText().slice(0, 16_000)
        : doc.getText(active.selection);
      contextFiles.push({ path: rel, content });
      seenPaths.add(rel);
    }

    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (root) {
      for (const rel of this.attachedFiles) {
        if (seenPaths.has(rel)) {
          continue;
        }
        try {
          const abs = resolveInsideWorkspace(root, rel);
          const content = (await fs.readFile(abs, 'utf8')).slice(0, 16_000);
          contextFiles.push({ path: rel, content });
          seenPaths.add(rel);
        } catch {
          // Skip missing or out-of-workspace file
        }
      }
    }

    const diagnostics = collectWorkspaceDiagnostics();

    const { id, promise } = this.client.request<{ text: string }>(
      'agent_chat',
      {
        messages: this.history,
        contextFiles,
        diagnostics,
        useWorkspaceSearch: includeWorkspace,
        mode,
        model,
      },
      {
        onStream: (delta) => {
          void this.view?.webview.postMessage({ type: 'delta', text: delta });
        },
        onToolEvent: (ev) => {
          this.handleToolEvent(ev);
        },
        onToolRequest: (req) => {
          void this.handleToolRequest(req, autoApply);
        },
      }
    );

    this.activeRequestId = id;
    try {
      const result = await promise;
      this.history.push({ role: 'assistant', content: result.text });
      void this.view?.webview.postMessage({ type: 'done' });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      void this.view?.webview.postMessage({ type: 'error', text: message });
    } finally {
      this.activeRequestId = undefined;
    }
  }

  private handleToolEvent(ev: ToolEventPayload): void {
    void this.view?.webview.postMessage({
      type: 'toolEvent',
      payload: ev,
    });
  }

  private async handleToolRequest(req: ToolRequestPayload, autoApply: boolean): Promise<void> {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!root) {
      await this.client.submitToolResult(req.callId, false, {
        error: 'No workspace folder is open.',
      });
      return;
    }

    if (req.name === 'edit_file' || req.name === 'write_file') {
      await this.handleFileMutationRequest(req, root, autoApply);
      return;
    }

    if (req.name === 'run_command') {
      await this.handleRunCommandRequest(req, root);
      return;
    }

    await this.client.submitToolResult(req.callId, false, {
      error: `Unsupported tool request: ${req.name}`,
    });
  }

  private async handleFileMutationRequest(
    req: ToolRequestPayload,
    root: string,
    autoApply: boolean
  ): Promise<void> {
    const relPath = String(req.args.path ?? '');
    const explanation = String(req.args.explanation ?? '');

    let absPath: string;
    try {
      absPath = resolveInsideWorkspace(root, relPath);
    } catch (err) {
      await this.client.submitToolResult(req.callId, false, {
        error: err instanceof Error ? err.message : String(err),
      });
      return;
    }

    let beforeContent = '';
    let isNewFile = false;
    try {
      beforeContent = await readLatestFileContent(absPath);
    } catch {
      isNewFile = true;
    }

    let afterContent: string;
    if (req.name === 'edit_file') {
      if (isNewFile) {
        await this.client.submitToolResult(req.callId, false, {
          error: `File '${relPath}' does not exist. Use write_file to create new files.`,
        });
        return;
      }
      const oldText = String(req.args.old_text ?? '');
      const newText = String(req.args.new_text ?? '');
      if (!oldText) {
        await this.client.submitToolResult(req.callId, false, {
          error: 'old_text cannot be empty in edit_file.',
        });
        return;
      }
      const firstIdx = beforeContent.indexOf(oldText);
      if (firstIdx === -1) {
        await this.client.submitToolResult(req.callId, false, {
          error: `old_text was not found verbatim in '${relPath}'. Read the file again and provide an exact match.`,
        });
        return;
      }
      if (beforeContent.indexOf(oldText, firstIdx + oldText.length) !== -1) {
        await this.client.submitToolResult(req.callId, false, {
          error: `old_text matched multiple locations in '${relPath}'. Include more surrounding lines.`,
        });
        return;
      }
      afterContent =
        beforeContent.slice(0, firstIdx) + newText + beforeContent.slice(firstIdx + oldText.length);
    } else {
      afterContent = String(req.args.content ?? '');
    }

    const beforeUri = vscode.Uri.parse(`rursor-diff:/${req.callId}/before/${relPath}`);
    const afterUri = vscode.Uri.parse(`rursor-diff:/${req.callId}/after/${relPath}`);
    this.diffProvider.setContent(beforeUri, beforeContent);
    this.diffProvider.setContent(afterUri, afterContent);

    const snapshot: PendingEditSnapshot = {
      callId: req.callId,
      relPath,
      absPath,
      beforeContent,
      afterContent,
      isNewFile,
      explanation,
    };
    this.editSnapshots.set(req.callId, snapshot);

    if (autoApply) {
      try {
        await applyFileWrite(absPath, afterContent);
        await this.refreshSidecarFile(absPath, afterContent);
        void this.view?.webview.postMessage({
          type: 'toolCard',
          card: {
            callId: req.callId,
            kind: 'edit',
            path: relPath,
            explanation,
            status: 'applied',
            stats: computeDiffStats(beforeContent, afterContent),
          },
        });
        await this.client.submitToolResult(req.callId, true, {
          applied: true,
          path: relPath,
        });
      } catch (err) {
        await this.client.submitToolResult(req.callId, false, {
          error: err instanceof Error ? err.message : String(err),
        });
      }
      return;
    }

    const approved = await new Promise<boolean>((resolve) => {
      snapshot.resolveDecision = resolve;
      void this.view?.webview.postMessage({
        type: 'toolCard',
        card: {
          callId: req.callId,
          kind: 'edit',
          path: relPath,
          explanation,
          status: 'pending',
          stats: computeDiffStats(beforeContent, afterContent),
        },
      });
    });

    snapshot.resolveDecision = undefined;

    if (!approved) {
      void this.view?.webview.postMessage({
        type: 'toolCardUpdate',
        callId: req.callId,
        status: 'rejected',
      });
      await this.client.submitToolResult(req.callId, false, {
        rejected: true,
        message: 'User rejected this file edit.',
      });
      return;
    }

    try {
      await applyFileWrite(absPath, afterContent);
      await this.refreshSidecarFile(absPath, afterContent);
      void this.view?.webview.postMessage({
        type: 'toolCardUpdate',
        callId: req.callId,
        status: 'applied',
      });
      await this.client.submitToolResult(req.callId, true, {
        applied: true,
        path: relPath,
      });
    } catch (err) {
      void this.view?.webview.postMessage({
        type: 'toolCardUpdate',
        callId: req.callId,
        status: 'error',
      });
      await this.client.submitToolResult(req.callId, false, {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  private async handleRunCommandRequest(req: ToolRequestPayload, root: string): Promise<void> {
    const command = String(req.args.command ?? '').trim();
    const reason = String(req.args.reason ?? '');
    if (!command) {
      await this.client.submitToolResult(req.callId, false, { error: 'Empty command.' });
      return;
    }

    const approved = await new Promise<boolean>((resolve) => {
      this.commandApprovals.set(req.callId, {
        callId: req.callId,
        command,
        resolveDecision: resolve,
      });
      void this.view?.webview.postMessage({
        type: 'toolCard',
        card: {
          callId: req.callId,
          kind: 'command',
          command,
          reason,
          status: 'pending',
        },
      });
    });

    this.commandApprovals.delete(req.callId);

    if (!approved) {
      void this.view?.webview.postMessage({
        type: 'toolCardUpdate',
        callId: req.callId,
        status: 'rejected',
      });
      await this.client.submitToolResult(req.callId, false, {
        rejected: true,
        message: 'User denied running this command.',
      });
      return;
    }

    void this.view?.webview.postMessage({
      type: 'toolCardUpdate',
      callId: req.callId,
      status: 'running',
    });

    const result = await executeShellCommand(command, root);
    void this.view?.webview.postMessage({
      type: 'toolCardUpdate',
      callId: req.callId,
      status: result.exitCode === 0 ? 'passed' : 'failed',
      output: result.combinedOutput,
      exitCode: result.exitCode,
    });

    await this.client.submitToolResult(req.callId, result.exitCode === 0, {
      exitCode: result.exitCode,
      output: result.combinedOutput,
    });
  }

  private async openDiffForCall(callId: string): Promise<void> {
    const snap = this.editSnapshots.get(callId);
    if (!snap) {
      return;
    }
    const beforeUri = vscode.Uri.parse(`rursor-diff:/${callId}/before/${snap.relPath}`);
    const afterUri = vscode.Uri.parse(`rursor-diff:/${callId}/after/${snap.relPath}`);
    await vscode.commands.executeCommand(
      'vscode.diff',
      beforeUri,
      afterUri,
      `Rursor Diff: ${snap.relPath}`
    );
  }

  private async revertAppliedEdit(callId: string): Promise<void> {
    const snap = this.editSnapshots.get(callId);
    if (!snap) {
      return;
    }
    try {
      if (snap.isNewFile) {
        await fs.unlink(snap.absPath).catch(() => undefined);
        const { promise } = this.client.request('remove_file', { path: snap.absPath });
        await promise.catch(() => undefined);
      } else {
        await applyFileWrite(snap.absPath, snap.beforeContent);
        await this.refreshSidecarFile(snap.absPath, snap.beforeContent);
      }
      void this.view?.webview.postMessage({
        type: 'toolCardUpdate',
        callId,
        status: 'reverted',
      });
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to revert ${snap.relPath}: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }

  private async refreshSidecarFile(absPath: string, content: string): Promise<void> {
    const { promise } = this.client.request('update_file', { path: absPath, content });
    await promise.catch(() => undefined);
  }

  private async insertSnippetAtCursor(code: string): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showWarningMessage('Open a file to insert code.');
      return;
    }
    await editor.edit((builder) => {
      builder.replace(editor.selection, code);
    });
  }

  private renderHtml(): string {
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<style>
  :root {
    --radius: 6px;
  }
  * { box-sizing: border-box; }
  body {
    font-family: var(--vscode-font-family);
    color: var(--vscode-foreground);
    background: var(--vscode-sideBar-background);
    margin: 0;
    display: flex;
    flex-direction: column;
    height: 100vh;
    font-size: 13px;
  }
  header {
    display: flex;
    gap: 6px;
    padding: 8px;
    border-bottom: 1px solid var(--vscode-panel-border);
    align-items: center;
    flex-wrap: wrap;
  }
  select, button {
    background: var(--vscode-button-secondaryBackground);
    color: var(--vscode-button-secondaryForeground);
    border: none;
    padding: 4px 8px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 11px;
  }
  button:hover { opacity: 0.9; }
  button.primary {
    background: var(--vscode-button-background);
    color: var(--vscode-button-foreground);
  }
  button.danger {
    background: rgba(215, 58, 73, 0.22);
    color: var(--vscode-errorForeground);
  }
  #messages {
    flex: 1;
    overflow-y: auto;
    padding: 10px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .msg {
    padding: 8px 10px;
    border-radius: var(--radius);
    font-size: 13px;
    line-height: 1.48;
    word-break: break-word;
  }
  .user {
    background: var(--vscode-input-background);
    border: 1px solid var(--vscode-panel-border);
    align-self: flex-end;
    max-width: 92%;
    white-space: pre-wrap;
  }
  .assistant {
    background: var(--vscode-editor-background);
    border: 1px solid var(--vscode-panel-border);
  }
  .assistant p { margin: 0 0 8px 0; }
  .assistant p:last-child { margin-bottom: 0; }
  .assistant ul { margin: 4px 0 8px 18px; padding: 0; }
  .assistant code {
    font-family: var(--vscode-editor-font-family, monospace);
    background: rgba(127, 127, 127, 0.16);
    padding: 1px 4px;
    border-radius: 3px;
    font-size: 12px;
  }
  .code-wrap {
    margin: 8px 0;
    border: 1px solid var(--vscode-panel-border);
    border-radius: 5px;
    overflow: hidden;
    background: var(--vscode-sideBar-background);
  }
  .code-bar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 8px;
    font-size: 11px;
    opacity: 0.85;
    border-bottom: 1px solid var(--vscode-panel-border);
  }
  .code-actions { display: flex; gap: 4px; }
  pre {
    margin: 0;
    padding: 8px;
    overflow-x: auto;
    font-family: var(--vscode-editor-font-family, monospace);
    font-size: 12px;
    line-height: 1.4;
  }
  .tool-pill {
    font-size: 11px;
    opacity: 0.85;
    padding: 3px 8px;
    border-radius: 4px;
    background: rgba(127, 127, 127, 0.12);
    align-self: flex-start;
  }
  .tool-card {
    border: 1px solid var(--vscode-panel-border);
    border-radius: var(--radius);
    background: var(--vscode-editor-background);
    padding: 8px 10px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
  }
  .tool-card-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-weight: 600;
  }
  .tool-card-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .cmd-out {
    max-height: 160px;
    overflow-y: auto;
    background: var(--vscode-sideBar-background);
    padding: 6px;
    border-radius: 4px;
    font-family: var(--vscode-editor-font-family, monospace);
    font-size: 11px;
    white-space: pre-wrap;
  }
  .err { color: var(--vscode-errorForeground); padding: 6px 8px; }
  footer {
    padding: 8px;
    border-top: 1px solid var(--vscode-panel-border);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .chips {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    align-items: center;
  }
  .chip {
    font-size: 11px;
    padding: 2px 6px;
    border-radius: 10px;
    background: rgba(127, 127, 127, 0.18);
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .chip button {
    padding: 0 3px;
    background: transparent;
    font-size: 11px;
  }
  textarea {
    width: 100%;
    min-height: 58px;
    resize: vertical;
    background: var(--vscode-input-background);
    color: var(--vscode-input-foreground);
    border: 1px solid var(--vscode-input-border);
    border-radius: 4px;
    padding: 6px;
    font-family: inherit;
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 11px;
    gap: 6px;
    flex-wrap: wrap;
  }
  .toggles { display: flex; gap: 8px; align-items: center; }
</style>
</head>
<body>
  <header>
    <select id="mode" title="Execution Mode">
      <option value="agent">Agent</option>
      <option value="ask">Ask</option>
    </select>
    <select id="model" title="Gemini Model">
      <option value="gemini-2.5-flash">gemini-2.5-flash</option>
      <option value="gemini-2.5-pro">gemini-2.5-pro</option>
      <option value="gemini-2.5-flash-lite">gemini-2.5-flash-lite</option>
    </select>
    <button id="keyBtn">API Key</button>
    <button id="clearBtn">Clear</button>
  </header>
  <div id="messages"></div>
  <footer>
    <div class="chips" id="chips">
      <button id="addFileBtn">+ File</button>
    </div>
    <textarea id="prompt" placeholder="Ask or instruct Rursor Agent (Enter to send, Shift+Enter for newline)…"></textarea>
    <div class="row">
      <div class="toggles">
        <label><input type="checkbox" id="wsCheck" checked /> @workspace</label>
        <label title="Automatically apply agent file edits without prompting"><input type="checkbox" id="autoApplyCheck" /> Auto-apply</label>
      </div>
      <div>
        <button id="stopBtn">Stop</button>
        <button id="sendBtn" class="primary">Send</button>
      </div>
    </div>
  </footer>
<script>
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const promptEl = document.getElementById('prompt');
  const modelEl = document.getElementById('model');
  const modeEl = document.getElementById('mode');
  const wsEl = document.getElementById('wsCheck');
  const autoApplyEl = document.getElementById('autoApplyCheck');
  const chipsEl = document.getElementById('chips');

  let activeAssistantEl = null;
  let activeAssistantRaw = '';
  const toolPills = new Map();
  const toolCards = new Map();

  function escHtml(s) {
    return String(s)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function renderMarkdown(md) {
    const segments = [];
    const fenceRe = /\`\`\`([a-zA-Z0-9_-]*)\\n([\\s\\S]*?)\`\`\`/g;
    let last = 0;
    let m;
    while ((m = fenceRe.exec(md)) !== null) {
      if (m.index > last) {
        segments.push({ type: 'text', value: md.slice(last, m.index) });
      }
      segments.push({ type: 'code', lang: m[1] || 'code', value: m[2] });
      last = fenceRe.lastIndex;
    }
    if (last < md.length) {
      segments.push({ type: 'text', value: md.slice(last) });
    }

    return segments.map((seg) => {
      if (seg.type === 'code') {
        const encoded = encodeURIComponent(seg.value);
        return '<div class="code-wrap">' +
          '<div class="code-bar"><span>' + escHtml(seg.lang) + '</span>' +
          '<div class="code-actions">' +
          '<button onclick="copyCode(\\'' + encoded + '\\')">Copy</button>' +
          '<button onclick="insertCode(\\'' + encoded + '\\')">Insert</button>' +
          '</div></div>' +
          '<pre><code>' + escHtml(seg.value) + '</code></pre></div>';
      }
      let html = escHtml(seg.value);
      html = html.replace(/\\*\\*(.+?)\\*\\*/g, '<strong>$1</strong>');
      html = html.replace(/\`([^\`\\n]+)\`/g, '<code>$1</code>');
      html = html.replace(/^### (.+)$/gm, '<strong>$1</strong>');
      html = html.replace(/^## (.+)$/gm, '<strong>$1</strong>');
      html = html.replace(/\\n\\n+/g, '</p><p>');
      html = html.replace(/\\n/g, '<br/>');
      return '<p>' + html + '</p>';
    }).join('');
  }

  window.copyCode = (enc) => navigator.clipboard?.writeText(decodeURIComponent(enc));
  window.insertCode = (enc) => vscode.postMessage({ type: 'insertCode', code: decodeURIComponent(enc) });

  function addMessage(cls, text) {
    const div = document.createElement('div');
    div.className = 'msg ' + cls;
    if (cls === 'assistant') {
      div.innerHTML = renderMarkdown(text);
    } else {
      div.textContent = text;
    }
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return div;
  }

  function send() {
    const text = promptEl.value.trim();
    if (!text) return;
    addMessage('user', text);
    activeAssistantEl = addMessage('assistant', '');
    activeAssistantRaw = '';
    promptEl.value = '';
    vscode.postMessage({
      type: 'send',
      text,
      mode: modeEl.value,
      model: modelEl.value,
      includeWorkspace: wsEl.checked,
      autoApply: autoApplyEl.checked,
    });
  }

  function renderChips(activeFile, attached) {
    const addBtn = document.getElementById('addFileBtn');
    chipsEl.innerHTML = '';
    chipsEl.appendChild(addBtn);
    if (activeFile) {
      const span = document.createElement('span');
      span.className = 'chip';
      span.textContent = 'Active: ' + activeFile;
      chipsEl.appendChild(span);
    }
    for (const f of attached || []) {
      const span = document.createElement('span');
      span.className = 'chip';
      span.innerHTML = '@' + escHtml(f) + ' ';
      const rm = document.createElement('button');
      rm.textContent = '×';
      rm.onclick = () => vscode.postMessage({ type: 'removeFile', filePath: f });
      span.appendChild(rm);
      chipsEl.appendChild(span);
    }
  }

  document.getElementById('sendBtn').onclick = send;
  document.getElementById('stopBtn').onclick = () => vscode.postMessage({ type: 'stop' });
  document.getElementById('clearBtn').onclick = () => {
    messagesEl.innerHTML = '';
    toolPills.clear();
    toolCards.clear();
    vscode.postMessage({ type: 'clear' });
  };
  document.getElementById('keyBtn').onclick = () => vscode.postMessage({ type: 'setKey' });
  document.getElementById('addFileBtn').onclick = () => vscode.postMessage({ type: 'pickFile' });
  modelEl.onchange = () => vscode.postMessage({ type: 'modelChanged', model: modelEl.value });
  promptEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });

  function renderToolCard(card) {
    const el = document.createElement('div');
    el.className = 'tool-card';
    el.dataset.callId = card.callId;
    updateToolCardElement(el, card);
    messagesEl.appendChild(el);
    toolCards.set(card.callId, { el, card });
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function updateToolCardElement(el, card) {
    if (card.kind === 'edit') {
      const stats = card.stats ? ' (+' + card.stats.added + ' / -' + card.stats.removed + ')' : '';
      let actions = '<button onclick="viewDiff(\\'' + card.callId + '\\')">View Diff</button>';
      if (card.status === 'pending') {
        actions += '<button class="primary" onclick="decideEdit(\\'' + card.callId + '\\', \\'approve\\')">Apply</button>';
        actions += '<button class="danger" onclick="decideEdit(\\'' + card.callId + '\\', \\'reject\\')">Reject</button>';
      } else if (card.status === 'applied') {
        actions += '<button onclick="revertEdit(\\'' + card.callId + '\\')">Revert</button>';
      }
      el.innerHTML =
        '<div class="tool-card-head"><span>Edit: ' + escHtml(card.path) + escHtml(stats) + '</span><span>' + escHtml(card.status) + '</span></div>' +
        (card.explanation ? '<div>' + escHtml(card.explanation) + '</div>' : '') +
        '<div class="tool-card-actions">' + actions + '</div>';
    } else if (card.kind === 'command') {
      let actions = '';
      if (card.status === 'pending') {
        actions =
          '<button class="primary" onclick="decideCmd(\\'' + card.callId + '\\', \\'approve\\')">Run</button>' +
          '<button class="danger" onclick="decideCmd(\\'' + card.callId + '\\', \\'reject\\')">Deny</button>';
      }
      const outHtml = card.output ? '<div class="cmd-out">' + escHtml(card.output) + '</div>' : '';
      el.innerHTML =
        '<div class="tool-card-head"><span>Command</span><span>' + escHtml(card.status) + '</span></div>' +
        '<div><code>' + escHtml(card.command) + '</code></div>' +
        (card.reason ? '<div>' + escHtml(card.reason) + '</div>' : '') +
        outHtml +
        (actions ? '<div class="tool-card-actions">' + actions + '</div>' : '');
    }
  }

  window.viewDiff = (callId) => vscode.postMessage({ type: 'viewDiff', callId });
  window.decideEdit = (callId, decision) => vscode.postMessage({ type: 'editDecision', callId, text: decision });
  window.revertEdit = (callId) => vscode.postMessage({ type: 'revertEdit', callId });
  window.decideCmd = (callId, decision) => vscode.postMessage({ type: 'commandDecision', callId, text: decision });

  window.addEventListener('message', (event) => {
    const msg = event.data;
    if (msg.type === 'delta') {
      if (!activeAssistantEl) {
        activeAssistantEl = addMessage('assistant', '');
        activeAssistantRaw = '';
      }
      activeAssistantRaw += msg.text;
      activeAssistantEl.innerHTML = renderMarkdown(activeAssistantRaw);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    } else if (msg.type === 'done') {
      if (activeAssistantEl && !activeAssistantRaw.trim()) {
        activeAssistantEl.remove();
      }
      activeAssistantEl = null;
      activeAssistantRaw = '';
    } else if (msg.type === 'error') {
      addMessage('err', msg.text);
      activeAssistantEl = null;
      activeAssistantRaw = '';
    } else if (msg.type === 'contextSync') {
      renderChips(msg.activeFile, msg.attachedFiles);
    } else if (msg.type === 'toolEvent') {
      const ev = msg.payload;
      if (ev.phase === 'start') {
        activeAssistantEl = null;
        activeAssistantRaw = '';
        const pill = document.createElement('div');
        pill.className = 'tool-pill';
        const argHint = ev.args?.path || ev.args?.query || ev.args?.prefix || '';
        pill.textContent = 'Running ' + ev.name + (argHint ? ': ' + argHint : '') + '…';
        messagesEl.appendChild(pill);
        toolPills.set(ev.callId, pill);
        messagesEl.scrollTop = messagesEl.scrollHeight;
      } else if (ev.phase === 'done') {
        const pill = toolPills.get(ev.callId);
        if (pill) {
          pill.textContent = pill.textContent.replace(/…$/, '') + (ev.ok ? ' ✓' : ' ✗');
        }
      }
    } else if (msg.type === 'toolCard') {
      activeAssistantEl = null;
      activeAssistantRaw = '';
      renderToolCard(msg.card);
    } else if (msg.type === 'toolCardUpdate') {
      const entry = toolCards.get(msg.callId);
      if (entry) {
        Object.assign(entry.card, msg);
        updateToolCardElement(entry.el, entry.card);
      }
    }
  });
</script>
</body>
</html>`;
  }
}

function resolveInsideWorkspace(root: string, requested: string): string {
  const resolvedRoot = path.resolve(root);
  const candidate = path.isAbsolute(requested)
    ? path.resolve(requested)
    : path.resolve(resolvedRoot, requested);
  if (candidate !== resolvedRoot && !candidate.startsWith(resolvedRoot + path.sep)) {
    throw new Error(`Access denied: '${requested}' is outside the workspace.`);
  }
  return candidate;
}

async function readLatestFileContent(absPath: string): Promise<string> {
  const targetUri = vscode.Uri.file(absPath).toString();
  const openDoc = vscode.workspace.textDocuments.find((d) => d.uri.toString() === targetUri);
  if (openDoc) {
    return openDoc.getText();
  }
  return fs.readFile(absPath, 'utf8');
}

async function applyFileWrite(absPath: string, content: string): Promise<void> {
  await fs.mkdir(path.dirname(absPath), { recursive: true });
  const uri = vscode.Uri.file(absPath);
  const openDoc = vscode.workspace.textDocuments.find((d) => d.uri.toString() === uri.toString());
  if (openDoc) {
    const fullRange = new vscode.Range(
      new vscode.Position(0, 0),
      openDoc.lineAt(Math.max(0, openDoc.lineCount - 1)).range.end
    );
    const edit = new vscode.WorkspaceEdit();
    edit.replace(uri, fullRange, content);
    await vscode.workspace.applyEdit(edit);
    await openDoc.save();
  } else {
    await fs.writeFile(absPath, content, 'utf8');
  }
}

function computeDiffStats(before: string, after: string): { added: number; removed: number } {
  const beforeLines = before ? before.split('\n') : [];
  const afterLines = after ? after.split('\n') : [];
  const beforeSet = new Set(beforeLines);
  const afterSet = new Set(afterLines);
  const added = afterLines.filter((l) => !beforeSet.has(l)).length;
  const removed = beforeLines.filter((l) => !afterSet.has(l)).length;
  return { added, removed };
}

function collectWorkspaceDiagnostics(): Array<{
  path: string;
  line: number;
  severity: string;
  message: string;
}> {
  const out: Array<{ path: string; line: number; severity: string; message: string }> = [];
  for (const [uri, diags] of vscode.languages.getDiagnostics()) {
    if (uri.scheme !== 'file') {
      continue;
    }
    const rel = vscode.workspace.asRelativePath(uri, false);
    for (const d of diags) {
      if (
        d.severity !== vscode.DiagnosticSeverity.Error &&
        d.severity !== vscode.DiagnosticSeverity.Warning
      ) {
        continue;
      }
      out.push({
        path: rel,
        line: d.range.start.line + 1,
        severity: d.severity === vscode.DiagnosticSeverity.Error ? 'error' : 'warning',
        message: d.message,
      });
      if (out.length >= 20) {
        return out;
      }
    }
  }
  return out;
}

function executeShellCommand(
  command: string,
  cwd: string
): Promise<{ exitCode: number; combinedOutput: string }> {
  return new Promise((resolve) => {
    const child = spawn('/bin/zsh', ['-lc', command], { cwd });
    let combined = '';
    const append = (chunk: Buffer) => {
      if (combined.length < 16_000) {
        combined += chunk.toString('utf8');
      }
    };
    child.stdout.on('data', append);
    child.stderr.on('data', append);

    const timer = setTimeout(() => {
      child.kill('SIGTERM');
      resolve({
        exitCode: 124,
        combinedOutput: `${combined}\n[Timed out after 60s]`,
      });
    }, 60_000);

    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({
        exitCode: code ?? 1,
        combinedOutput: combined.trim() || '(no output)',
      });
    });

    child.on('error', (err) => {
      clearTimeout(timer);
      resolve({
        exitCode: 1,
        combinedOutput: `Failed to execute command: ${err.message}`,
      });
    });
  });
}
