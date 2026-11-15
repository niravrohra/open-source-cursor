import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { SidecarClient, ToolEventPayload, ToolRequestPayload } from './sidecarClient';
import { inspectWebUrl, RursorWebPreviewPanel } from './webPreview';

interface ImageAttachment {
  mimeType: string;
  data: string;
  name?: string;
}

interface ChatTurn {
  role: 'user' | 'assistant';
  content: string;
  images?: ImageAttachment[];
}

interface SessionSummary {
  sessionId: string;
  title: string;
  updatedAt: number;
  messageCount: number;
}

interface StoredSession {
  sessionId: string;
  title: string;
  updatedAt: number;
  messages: ChatTurn[];
}

interface McpServerStatus {
  name: string;
  connected: boolean;
  toolCount: number;
  error?: string;
}

interface SidecarStatusPayload {
  hasWorkspaceRules?: boolean;
  mcpServers?: McpServerStatus[];
  indexedFiles?: number;
}

interface PendingEditSnapshot {
  callId: string;
  relPath: string;
  absPath: string;
  beforeContent: string;
  afterContent: string;
  isNewFile: boolean;
  explanation: string;
  status: 'pending' | 'applied' | 'accepted' | 'rejected' | 'reverted' | 'error';
  autoAcceptOnApply?: boolean;
  resolveDecision?: (approved: boolean) => void;
}

interface PendingCommandApproval {
  callId: string;
  command: string;
  resolveDecision: (approved: boolean) => void;
}

function createSessionId(): string {
  return `sess_${Date.now()}`;
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
  private currentSessionId: string = createSessionId();
  private history: ChatTurn[] = [];
  private activeRequestId: number | undefined;
  private attachedFiles = new Set<string>();
  private readonly editSnapshots = new Map<string, PendingEditSnapshot>();
  private readonly commandApprovals = new Map<string, PendingCommandApproval>();

  constructor(
    private readonly client: SidecarClient,
    private readonly diffProvider: RursorDiffContentProvider,
    private readonly extensionUri: vscode.Uri,
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
        images?: ImageAttachment[];
        model?: string;
        mode?: 'agent' | 'ask';
        includeWorkspace?: boolean;
        includeGitDiff?: boolean;
        autoApply?: boolean;
        autoVerify?: boolean;
        callId?: string;
        filePath?: string;
        code?: string;
        sessionId?: string;
        checkpointId?: string;
      }) => {
        if (msg.type === 'webviewReady') {
          this.syncContextBar();
          this.syncReviewBar();
          await Promise.all([this.refreshSessions(), this.refreshStatus(), this.sendSkillsList()]);
        } else if (
          msg.type === 'send' &&
          (Boolean(msg.text) || (Array.isArray(msg.images) && msg.images.length > 0))
        ) {
          await this.handleUserPrompt(
            msg.text ?? '',
            msg.images ?? [],
            msg.mode ?? 'agent',
            Boolean(msg.includeWorkspace),
            Boolean(msg.includeGitDiff),
            Boolean(msg.autoApply),
            Boolean(msg.autoVerify),
            msg.model
          );
        } else if (msg.type === 'stop') {
          await this.cancelActiveRequest();
        } else if (msg.type === 'clear') {
          await this.clearCurrentConversation();
        } else if (msg.type === 'newSession') {
          await this.createNewSession();
        } else if (msg.type === 'switchSession' && msg.sessionId) {
          await this.switchSession(msg.sessionId);
        } else if (msg.type === 'deleteSession') {
          await this.deleteCurrentSession();
        } else if (msg.type === 'setKey') {
          await vscode.commands.executeCommand('rursor.setApiKey');
        } else if (msg.type === 'modelChanged' && msg.model) {
          await this.onModelChange(msg.model);
        } else if (msg.type === 'pickFile') {
          await this.pickContextFile();
        } else if (msg.type === 'attachFile' && msg.filePath) {
          this.attachedFiles.add(msg.filePath);
          this.syncContextBar();
        } else if (msg.type === 'removeFile' && msg.filePath) {
          this.attachedFiles.delete(msg.filePath);
          this.syncContextBar();
        } else if (msg.type === 'listFiles') {
          await this.sendFilesList();
        } else if (msg.type === 'listSkills') {
          await this.sendSkillsList();
        } else if (msg.type === 'openPreview') {
          RursorWebPreviewPanel.createOrShow(this.extensionUri);
        } else if (msg.type === 'restoreCheckpoint' && msg.checkpointId) {
          await this.restoreCheckpoint(msg.checkpointId);
        } else if (msg.type === 'editDecision' && msg.callId) {
          const snap = this.editSnapshots.get(msg.callId);
          snap?.resolveDecision?.(msg.text === 'approve');
        } else if (msg.type === 'viewDiff' && msg.callId) {
          await this.openDiffForCall(msg.callId);
        } else if (msg.type === 'revertEdit' && msg.callId) {
          await this.revertAppliedEdit(msg.callId);
        } else if (msg.type === 'acceptAllEdits') {
          await this.acceptAllEdits();
        } else if (msg.type === 'revertAllEdits') {
          await this.revertAllEdits();
        } else if (msg.type === 'commandDecision' && msg.callId) {
          const pending = this.commandApprovals.get(msg.callId);
          pending?.resolveDecision(msg.text === 'approve');
        } else if (msg.type === 'insertCode' && msg.code) {
          await this.insertSnippetAtCursor(msg.code);
        } else if (msg.type === 'reloadMcp') {
          await this.reloadMcp();
        }
      }
    );

    this.syncContextBar();
    this.syncReviewBar();
    void this.refreshSessions();
    void this.refreshStatus();
  }

  public async refreshStatus(): Promise<void> {
    if (!this.view) {
      return;
    }
    try {
      const { promise } = this.client.request<SidecarStatusPayload>('status');
      const status = await promise;
      let mcpServers = status.mcpServers;
      if (!Array.isArray(mcpServers)) {
        const mcpRes = await this.client
          .request<{ servers: McpServerStatus[] }>('mcp_status')
          .promise.catch(() => ({ servers: [] as McpServerStatus[] }));
        mcpServers = mcpRes.servers ?? [];
      }
      void this.view?.webview.postMessage({
        type: 'statusSync',
        mcpServers,
        hasWorkspaceRules: Boolean(status.hasWorkspaceRules),
        indexedFiles: status.indexedFiles ?? 0,
      });
    } catch {
      // Ignore if sidecar is not ready yet
    }
  }

  private async reloadMcp(): Promise<void> {
    try {
      const { promise } = this.client.request<{ servers: McpServerStatus[] }>('reload_mcp');
      await promise;
      await this.refreshStatus();
    } catch (err) {
      void this.view?.webview.postMessage({
        type: 'error',
        text: `Failed to reload MCP: ${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }

  private async refreshSessions(): Promise<void> {
    if (!this.view) {
      return;
    }
    try {
      const { promise } = this.client.request<{ sessions: SessionSummary[] }>('list_sessions');
      const res = await promise;
      void this.view?.webview.postMessage({
        type: 'sessionsSync',
        sessions: res.sessions ?? [],
        currentSessionId: this.currentSessionId,
      });
    } catch {
      // Ignore if sidecar is not ready yet
    }
  }

  private async createNewSession(): Promise<void> {
    await this.cancelActiveRequest();
    this.history = [];
    this.editSnapshots.clear();
    this.currentSessionId = createSessionId();
    this.syncReviewBar();
    void this.view?.webview.postMessage({
      type: 'sessionLoaded',
      sessionId: this.currentSessionId,
      messages: [],
    });
    await this.refreshSessions();
  }

  private async clearCurrentConversation(): Promise<void> {
    await this.cancelActiveRequest();
    this.history = [];
    this.editSnapshots.clear();
    this.syncReviewBar();
  }

  private async switchSession(sessionId: string): Promise<void> {
    if (!sessionId || sessionId === this.currentSessionId) {
      return;
    }
    await this.cancelActiveRequest();
    try {
      const { promise } = this.client.request<{ session: StoredSession }>('load_session', {
        sessionId,
      });
      const res = await promise;
      const session = res.session;
      this.currentSessionId = session.sessionId;
      this.history = (session.messages ?? []).map((m) => ({
        role: m.role === 'assistant' ? 'assistant' : 'user',
        content: m.content ?? '',
        ...(Array.isArray(m.images) && m.images.length > 0 ? { images: m.images } : {}),
      }));
      this.editSnapshots.clear();
      this.syncReviewBar();
      void this.view?.webview.postMessage({
        type: 'sessionLoaded',
        sessionId: this.currentSessionId,
        messages: this.history,
      });
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to load session: ${err instanceof Error ? err.message : String(err)}`
      );
      await this.refreshSessions();
    }
  }

  private async deleteCurrentSession(): Promise<void> {
    const targetId = this.currentSessionId;
    try {
      const { promise } = this.client.request<{ deleted: boolean }>('delete_session', {
        sessionId: targetId,
      });
      await promise.catch(() => undefined);
    } finally {
      await this.createNewSession();
    }
  }

  private async cancelActiveRequest(): Promise<void> {
    for (const snap of this.editSnapshots.values()) {
      if (snap.status === 'pending') {
        snap.status = 'rejected';
      }
      snap.resolveDecision?.(false);
      snap.resolveDecision = undefined;
    }
    for (const cmd of this.commandApprovals.values()) {
      cmd.resolveDecision(false);
    }
    this.commandApprovals.clear();
    this.syncReviewBar();

    if (this.activeRequestId !== undefined) {
      await this.client.cancel(this.activeRequestId);
      this.activeRequestId = undefined;
    }
    void this.view?.webview.postMessage({ type: 'done' });
  }

  private async sendSkillsList(): Promise<void> {
    try {
      const { promise } = this.client.request<{
        skills: Array<{ name: string; description: string; source: string }>;
      }>('list_skills');
      const res = await promise;
      void this.view?.webview.postMessage({
        type: 'skillsList',
        skills: res.skills ?? [],
      });
    } catch {
      void this.view?.webview.postMessage({
        type: 'skillsList',
        skills: [],
      });
    }
  }

  private async restoreCheckpoint(checkpointId: string): Promise<void> {
    try {
      const { promise } = this.client.request<{
        restored: { checkpointId: string; restoredFiles: string[]; removedFiles: string[] };
      }>('restore_checkpoint', { checkpointId });
      const res = await promise;
      const restoredCount = (res.restored?.restoredFiles?.length ?? 0) + (res.restored?.removedFiles?.length ?? 0);
      this.editSnapshots.clear();
      this.syncReviewBar();
      vscode.window.showInformationMessage(
        `Rursor restored checkpoint ${checkpointId} (${restoredCount} file change(s) reverted).`
      );
      void this.view?.webview.postMessage({
        type: 'checkpointRestored',
        checkpointId,
        restoredCount,
      });
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to restore checkpoint: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }

  private async sendFilesList(): Promise<void> {
    try {
      const { promise } = this.client.request<{ files: string[] }>('list_files', { limit: 600 });
      const res = await promise;
      void this.view?.webview.postMessage({
        type: 'filesList',
        files: res.files ?? [],
      });
    } catch {
      void this.view?.webview.postMessage({
        type: 'filesList',
        files: [],
      });
    }
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
    const diagnostics = collectWorkspaceDiagnostics();
    void this.view?.webview.postMessage({
      type: 'contextSync',
      activeFile: activeLabel,
      attachedFiles: Array.from(this.attachedFiles),
      diagnostics,
    });
  }

  private syncReviewBar(): void {
    const reviewable = Array.from(this.editSnapshots.values()).filter(
      (snap) => snap.status === 'pending' || snap.status === 'applied'
    );
    const fileCount = new Set(reviewable.map((snap) => snap.relPath)).size;
    void this.view?.webview.postMessage({
      type: 'reviewSync',
      fileCount,
      editCount: reviewable.length,
    });
  }

  private async acceptAllEdits(): Promise<void> {
    for (const snap of this.editSnapshots.values()) {
      if (snap.status === 'applied') {
        snap.status = 'accepted';
      } else if (snap.status === 'pending') {
        if (snap.resolveDecision) {
          snap.autoAcceptOnApply = true;
          const resolve = snap.resolveDecision;
          snap.resolveDecision = undefined;
          resolve(true);
        } else {
          snap.status = 'accepted';
        }
      }
    }
    this.syncReviewBar();
  }

  private async revertAllEdits(): Promise<void> {
    const ordered = Array.from(this.editSnapshots.values()).reverse();
    for (const snap of ordered) {
      if (snap.status === 'pending') {
        snap.status = 'rejected';
        const resolve = snap.resolveDecision;
        snap.resolveDecision = undefined;
        resolve?.(false);
      } else if (snap.status === 'applied') {
        await this.revertAppliedEdit(snap.callId);
      }
    }
    this.syncReviewBar();
  }

  private async handleUserPrompt(
    prompt: string,
    images: ImageAttachment[],
    mode: 'agent' | 'ask',
    includeWorkspace: boolean,
    includeGitDiff: boolean,
    autoApply: boolean,
    autoVerify: boolean,
    model?: string
  ): Promise<void> {
    const turn: ChatTurn = {
      role: 'user',
      content: prompt,
      ...(images.length > 0 ? { images } : {}),
    };
    this.history.push(turn);

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
    const cfg = vscode.workspace.getConfiguration('rursor');
    const thinkingBudget = cfg.get<number>('agent.thinkingBudget', 0);

    const { id, promise } = this.client.request<{ text: string }>(
      'agent_chat',
      {
        sessionId: this.currentSessionId,
        messages: this.history.map((m) => ({
          role: m.role,
          content: m.content,
          ...(m.images && m.images.length > 0 ? { images: m.images } : {}),
        })),
        contextFiles,
        diagnostics,
        useWorkspaceSearch: includeWorkspace,
        includeGitDiff,
        autoVerify: autoVerify || cfg.get<boolean>('agent.autoVerify', false),
        thinkingBudget: thinkingBudget > 0 ? thinkingBudget : undefined,
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
        onCheckpointCreated: (checkpoint) => {
          void this.view?.webview.postMessage({ type: 'checkpointCreated', checkpoint });
        },
        onSubagentEvent: (payload) => {
          void this.view?.webview.postMessage({ type: 'subagentEvent', payload });
        },
      }
    );

    this.activeRequestId = id;
    try {
      const result = await promise;
      this.history.push({ role: 'assistant', content: result.text });
      void this.view?.webview.postMessage({ type: 'done' });
      void this.refreshSessions();
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

    if (req.name === 'inspect_web_page') {
      await this.handleInspectWebPageRequest(req);
      return;
    }

    await this.client.submitToolResult(req.callId, false, {
      error: `Unsupported tool request: ${req.name}`,
    });
  }

  private async handleInspectWebPageRequest(req: ToolRequestPayload): Promise<void> {
    const rawUrl = String(req.args.url ?? 'http://localhost:3000');
    const openPreview = Boolean(req.args.open_preview ?? true);

    void this.view?.webview.postMessage({
      type: 'toolEvent',
      payload: {
        callId: req.callId,
        phase: 'start',
        name: 'inspect_web_page',
        args: { path: rawUrl },
      },
    });

    try {
      if (openPreview) {
        RursorWebPreviewPanel.createOrShow(this.extensionUri, rawUrl);
      }
      const inspection = await inspectWebUrl(rawUrl);
      void this.view?.webview.postMessage({
        type: 'toolEvent',
        payload: {
          callId: req.callId,
          phase: 'done',
          name: 'inspect_web_page',
          ok: inspection.ok,
          output: { status: inspection.status, title: inspection.title },
        },
      });
      await this.client.submitToolResult(req.callId, true, {
        url: inspection.url,
        status: inspection.status,
        title: inspection.title,
        errors: inspection.errors,
        textSummary: inspection.textSummary,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      void this.view?.webview.postMessage({
        type: 'toolEvent',
        payload: {
          callId: req.callId,
          phase: 'done',
          name: 'inspect_web_page',
          ok: false,
          output: { error: message },
        },
      });
      await this.client.submitToolResult(req.callId, false, { error: message });
    }
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
      status: autoApply ? 'applied' : 'pending',
    };
    this.editSnapshots.set(req.callId, snapshot);

    if (autoApply) {
      try {
        await applyFileWrite(absPath, afterContent);
        await this.refreshSidecarFile(absPath, afterContent);
        snapshot.status = 'applied';
        this.syncReviewBar();
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
        snapshot.status = 'error';
        this.syncReviewBar();
        await this.client.submitToolResult(req.callId, false, {
          error: err instanceof Error ? err.message : String(err),
        });
      }
      return;
    }

    const approved = await new Promise<boolean>((resolve) => {
      snapshot.resolveDecision = resolve;
      this.syncReviewBar();
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
      snapshot.status = 'rejected';
      this.syncReviewBar();
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
      snapshot.status = snapshot.autoAcceptOnApply ? 'accepted' : 'applied';
      this.syncReviewBar();
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
      snapshot.status = 'error';
      this.syncReviewBar();
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

    const usePty = vscode.workspace
      .getConfiguration('rursor')
      .get<boolean>('agent.usePty', true);

    let result: { exitCode: number; combinedOutput: string };
    if (usePty) {
      try {
        const { promise } = this.client.request<{ exitCode: number; output: string }>(
          'run_pty_command',
          { command, cwd: root, timeoutSecs: 60 },
          {
            onPtyChunk: (chunk) => {
              void this.view?.webview.postMessage({
                type: 'toolCardStream',
                callId: req.callId,
                chunk,
              });
            },
          }
        );
        const ptyRes = await promise;
        result = {
          exitCode: ptyRes.exitCode ?? 1,
          combinedOutput: ptyRes.output ?? '(no output)',
        };
      } catch {
        result = await executeShellCommand(command, root, (chunk) => {
          void this.view?.webview.postMessage({
            type: 'toolCardStream',
            callId: req.callId,
            chunk,
          });
        });
      }
    } else {
      result = await executeShellCommand(command, root, (chunk) => {
        void this.view?.webview.postMessage({
          type: 'toolCardStream',
          callId: req.callId,
          chunk,
        });
      });
    }

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
      snap.status = 'reverted';
      this.syncReviewBar();
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
  .session-bar {
    display: flex;
    gap: 4px;
    width: 100%;
    align-items: center;
  }
  #sessionSelect {
    flex: 1;
    min-width: 0;
    text-overflow: ellipsis;
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
  button:disabled { opacity: 0.5; cursor: default; }
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
  .msg-images {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 6px;
  }
  .msg-images img {
    max-width: 120px;
    max-height: 90px;
    border-radius: 4px;
    border: 1px solid var(--vscode-panel-border);
    object-fit: cover;
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
  .review-bar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    background: var(--vscode-editor-background);
    border-top: 1px solid var(--vscode-panel-border);
    font-size: 11px;
    font-weight: 600;
  }
  .review-bar[hidden] { display: none; }
  .review-actions { display: flex; gap: 6px; }
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
  .img-thumb {
    width: 18px;
    height: 18px;
    border-radius: 3px;
    object-fit: cover;
  }
  .composer-wrap {
    position: relative;
  }
  .mention-popover {
    position: absolute;
    bottom: calc(100% + 4px);
    left: 0;
    right: 0;
    max-height: 210px;
    overflow-y: auto;
    background: var(--vscode-editorWidget-background, var(--vscode-editor-background));
    border: 1px solid var(--vscode-widget-border, var(--vscode-panel-border));
    border-radius: var(--radius);
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
    z-index: 20;
    padding: 4px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .mention-popover[hidden] { display: none; }
  .mention-item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    padding: 5px 8px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 12px;
  }
  .mention-item.active, .mention-item:hover {
    background: var(--vscode-list-activeSelectionBackground, rgba(127, 127, 127, 0.25));
    color: var(--vscode-list-activeSelectionForeground, inherit);
  }
  .mention-label {
    font-weight: 600;
    white-space: nowrap;
  }
  .mention-detail {
    font-size: 11px;
    opacity: 0.78;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
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
  .toggles { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
  .status-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    opacity: 0.9;
    padding-top: 2px;
    border-top: 1px solid var(--vscode-panel-border);
  }
  .status-pills {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .status-pill {
    font-size: 10px;
    padding: 2px 6px;
    border-radius: 10px;
    background: rgba(127, 127, 127, 0.16);
  }
</style>
</head>
<body>
  <header>
    <div class="session-bar">
      <select id="sessionSelect" title="Chat Sessions"></select>
      <button id="newSessionBtn" title="Start New Session">+ New</button>
      <button id="delSessionBtn" class="danger" title="Delete Current Session">Del</button>
    </div>
    <select id="mode" title="Execution Mode">
      <option value="agent">Agent</option>
      <option value="ask">Ask</option>
    </select>
    <select id="model" title="Model / Smart Router">
      <option value="auto">Auto (Smart Router)</option>
      <option value="gemini-2.5-flash" selected>gemini-2.5-flash</option>
      <option value="gemini-2.5-pro">gemini-2.5-pro</option>
      <option value="gemini-2.5-flash-lite">gemini-2.5-flash-lite</option>
      <option value="claude-sonnet-4-20250514">claude-sonnet-4</option>
      <option value="claude-3-5-haiku-latest">claude-3.5-haiku</option>
      <option value="gpt-4o-mini">gpt-4o-mini</option>
      <option value="o3-mini">o3-mini</option>
    </select>
    <button id="keyBtn">API Key</button>
    <button id="clearBtn">Clear</button>
  </header>
  <div id="messages"></div>
  <div id="reviewBar" class="review-bar" hidden>
    <span id="reviewText">0 file(s) changed</span>
    <div class="review-actions">
      <button id="acceptAllBtn" class="primary">Accept All</button>
      <button id="revertAllBtn" class="danger">Revert All</button>
    </div>
  </div>
  <footer>
    <div class="chips" id="chips">
      <button id="addFileBtn" type="button">+ File</button>
      <button id="addImgBtn" type="button">+ Img</button>
      <button id="previewBtn" type="button" title="Open Web Preview">Preview</button>
      <input type="file" id="imgFileInput" accept="image/*" multiple hidden />
    </div>
    <div class="composer-wrap">
      <div id="mentionPopover" class="mention-popover" hidden></div>
      <textarea id="prompt" placeholder="Ask or instruct Rursor Agent (type @ for context, / for skills, paste images)…"></textarea>
    </div>
    <div class="row">
      <div class="toggles">
        <label><input type="checkbox" id="wsCheck" checked /> @workspace</label>
        <label title="Include git status and diff in this turn"><input type="checkbox" id="gitCheck" /> @git</label>
        <label title="Automatically apply agent file edits without prompting"><input type="checkbox" id="autoApplyCheck" /> Auto-apply</label>
        <label title="Run project tests/typecheck after edits and auto-fix failures"><input type="checkbox" id="autoVerifyCheck" /> Auto-verify</label>
      </div>
      <div>
        <button id="stopBtn">Stop</button>
        <button id="sendBtn" class="primary">Send</button>
      </div>
    </div>
    <div class="status-row">
      <div class="status-pills">
        <span id="mcpPill" class="status-pill" title="MCP Servers">MCP: 0 servers</span>
        <span id="rulesPill" class="status-pill" hidden title="Project rules loaded">Rules: On</span>
      </div>
      <button id="reloadMcpBtn" type="button">Reload MCP</button>
    </div>
  </footer>
<script>
  const vscode = acquireVsCodeApi();
  const messagesEl = document.getElementById('messages');
  const promptEl = document.getElementById('prompt');
  const modelEl = document.getElementById('model');
  const modeEl = document.getElementById('mode');
  const wsEl = document.getElementById('wsCheck');
  const gitEl = document.getElementById('gitCheck');
  const autoApplyEl = document.getElementById('autoApplyCheck');
  const autoVerifyEl = document.getElementById('autoVerifyCheck');
  const chipsEl = document.getElementById('chips');
  const imgFileInput = document.getElementById('imgFileInput');
  const sessionSelectEl = document.getElementById('sessionSelect');
  const mentionPopoverEl = document.getElementById('mentionPopover');
  const reviewBarEl = document.getElementById('reviewBar');
  const reviewTextEl = document.getElementById('reviewText');
  const mcpPillEl = document.getElementById('mcpPill');
  const rulesPillEl = document.getElementById('rulesPill');
  const reloadMcpBtn = document.getElementById('reloadMcpBtn');

  let activeAssistantEl = null;
  let activeAssistantRaw = '';
  const toolPills = new Map();
  const toolCards = new Map();

  let lastActiveFile = '';
  let lastAttachedFiles = [];
  let latestDiagnostics = [];
  let pendingImages = [];

  let cachedFiles = [];
  let cachedSkills = [];
  let filesRequested = false;
  let mentionOpen = false;
  let mentionStart = -1;
  let mentionItems = [];
  let mentionIndex = 0;

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

  function addMessage(cls, text, images) {
    const div = document.createElement('div');
    div.className = 'msg ' + cls;
    if (cls === 'assistant') {
      div.innerHTML = renderMarkdown(text);
    } else {
      if (Array.isArray(images) && images.length > 0) {
        const imgsDiv = document.createElement('div');
        imgsDiv.className = 'msg-images';
        for (const img of images) {
          const el = document.createElement('img');
          el.src = 'data:' + img.mimeType + ';base64,' + img.data;
          el.alt = img.name || 'image';
          el.title = img.name || 'image';
          imgsDiv.appendChild(el);
        }
        div.appendChild(imgsDiv);
      }
      if (text) {
        const span = document.createElement('span');
        span.textContent = text;
        div.appendChild(span);
      }
    }
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return div;
  }

  function readImageFile(file) {
    if (!file || !file.type || !file.type.startsWith('image/')) return;
    const reader = new FileReader();
    reader.onload = () => {
      const result = typeof reader.result === 'string' ? reader.result : '';
      const commaIdx = result.indexOf(',');
      if (commaIdx === -1) return;
      const header = result.slice(0, commaIdx);
      const data = result.slice(commaIdx + 1);
      const match = header.match(/data:(.*?);/);
      const mimeType = file.type || (match ? match[1] : 'image/png');
      pendingImages.push({
        mimeType,
        data,
        name: file.name || ('image-' + (pendingImages.length + 1) + '.png'),
      });
      renderChips(lastActiveFile, lastAttachedFiles);
    };
    reader.readAsDataURL(file);
  }

  function renderChips(activeFile, attached) {
    lastActiveFile = activeFile || '';
    lastAttachedFiles = Array.isArray(attached) ? attached : [];

    const addFileBtn = document.getElementById('addFileBtn');
    const addImgBtn = document.getElementById('addImgBtn');
    const previewBtn = document.getElementById('previewBtn');
    chipsEl.innerHTML = '';
    chipsEl.appendChild(addFileBtn);
    chipsEl.appendChild(addImgBtn);
    chipsEl.appendChild(previewBtn);
    chipsEl.appendChild(imgFileInput);

    if (lastActiveFile) {
      const span = document.createElement('span');
      span.className = 'chip';
      span.textContent = 'Active: ' + lastActiveFile;
      chipsEl.appendChild(span);
    }
    for (const f of lastAttachedFiles) {
      const span = document.createElement('span');
      span.className = 'chip';
      span.innerHTML = '@' + escHtml(f) + ' ';
      const rm = document.createElement('button');
      rm.type = 'button';
      rm.textContent = '×';
      rm.onclick = () => vscode.postMessage({ type: 'removeFile', filePath: f });
      span.appendChild(rm);
      chipsEl.appendChild(span);
    }
    pendingImages.forEach((img, idx) => {
      const span = document.createElement('span');
      span.className = 'chip';
      const thumb = document.createElement('img');
      thumb.className = 'img-thumb';
      thumb.src = 'data:' + img.mimeType + ';base64,' + img.data;
      thumb.alt = img.name || 'image';
      span.appendChild(thumb);

      const label = document.createElement('span');
      label.textContent = img.name || 'image';
      span.appendChild(label);

      const rm = document.createElement('button');
      rm.type = 'button';
      rm.textContent = '×';
      rm.onclick = () => {
        pendingImages.splice(idx, 1);
        renderChips(lastActiveFile, lastAttachedFiles);
      };
      span.appendChild(rm);
      chipsEl.appendChild(span);
    });
  }

  function fuzzyScore(query, target) {
    if (!query) return 1;
    const q = query.toLowerCase();
    const t = target.toLowerCase();
    if (t === q) return 1000;
    if (t.startsWith(q)) return 600 - t.length;
    const subIdx = t.indexOf(q);
    if (subIdx !== -1) return 400 - subIdx - t.length * 0.1;
    let qi = 0;
    let score = 100;
    let lastMatch = -1;
    for (let ti = 0; ti < t.length && qi < q.length; ti++) {
      if (t[ti] === q[qi]) {
        if (lastMatch === ti - 1) score += 8;
        lastMatch = ti;
        qi++;
      }
    }
    return qi === q.length ? score - t.length * 0.1 : -1;
  }

  function buildErrorsInsertText() {
    if (!latestDiagnostics || latestDiagnostics.length === 0) {
      return 'Check and fix any active workspace diagnostics. ';
    }
    const lines = latestDiagnostics.slice(0, 10).map((d) =>
      '[' + d.severity + '] ' + d.path + ':' + d.line + ' ' + d.message
    );
    return 'Fix these diagnostics:\\n' + lines.join('\\n') + '\\n';
  }

  function getFilteredMentions(query) {
    const builtins = [
      { kind: 'workspace', label: '@workspace', detail: 'Toggle workspace search on' },
      { kind: 'git', label: '@git', detail: 'Include git diff in this turn' },
      { kind: 'errors', label: '@errors', detail: 'Insert active diagnostics (' + latestDiagnostics.length + ')' },
    ];

    const matchedBuiltins = builtins
      .map((item) => ({ item, score: fuzzyScore(query, item.label.slice(1)) }))
      .filter((x) => x.score > 0)
      .sort((a, b) => b.score - a.score)
      .map((x) => x.item);

    const matchedFiles = cachedFiles
      .map((filePath) => ({
        item: { kind: 'file', label: '@' + filePath, detail: 'Attach file', path: filePath },
        score: fuzzyScore(query, filePath),
      }))
      .filter((x) => x.score > 0)
      .sort((a, b) => b.score - a.score)
      .slice(0, 20)
      .map((x) => x.item);

    return matchedBuiltins.concat(matchedFiles);
  }

  function renderMentionPopover() {
    if (!mentionOpen || mentionItems.length === 0) {
      mentionPopoverEl.hidden = true;
      mentionPopoverEl.innerHTML = '';
      return;
    }
    mentionPopoverEl.hidden = false;
    mentionPopoverEl.innerHTML = '';
    mentionItems.forEach((item, idx) => {
      const row = document.createElement('div');
      row.className = 'mention-item' + (idx === mentionIndex ? ' active' : '');
      const lbl = document.createElement('span');
      lbl.className = 'mention-label';
      lbl.textContent = item.label;
      const det = document.createElement('span');
      det.className = 'mention-detail';
      det.textContent = item.detail;
      row.appendChild(lbl);
      row.appendChild(det);
      row.onmousedown = (e) => {
        e.preventDefault();
        selectMention(item);
      };
      mentionPopoverEl.appendChild(row);
    });
    const activeRow = mentionPopoverEl.children[mentionIndex];
    if (activeRow) {
      activeRow.scrollIntoView({ block: 'nearest' });
    }
  }

  function closeMentionPopover() {
    mentionOpen = false;
    mentionStart = -1;
    mentionItems = [];
    mentionIndex = 0;
    mentionPopoverEl.hidden = true;
  }

  function selectMention(item) {
    if (!item) return;
    const cursor = promptEl.selectionStart ?? promptEl.value.length;
    const before = promptEl.value.slice(0, mentionStart >= 0 ? mentionStart : cursor);
    const after = promptEl.value.slice(cursor);
    let inserted = '';
    if (item.kind === 'workspace') {
      wsEl.checked = true;
      inserted = '@workspace ';
    } else if (item.kind === 'git') {
      gitEl.checked = true;
      inserted = '@git ';
    } else if (item.kind === 'errors') {
      inserted = buildErrorsInsertText();
    } else if (item.kind === 'file') {
      vscode.postMessage({ type: 'attachFile', filePath: item.path });
      inserted = '';
    } else if (item.kind === 'skill') {
      inserted = item.label + ' ';
    }
    promptEl.value = before + inserted + after;
    const nextPos = before.length + inserted.length;
    promptEl.focus();
    promptEl.setSelectionRange(nextPos, nextPos);
    closeMentionPopover();
  }

  function updateMentionState() {
    const cursor = promptEl.selectionStart ?? promptEl.value.length;
    const beforeCursor = promptEl.value.slice(0, cursor);
    const slashMatch = beforeCursor.match(/^\\/([a-zA-Z0-9_-]*)$/);
    if (slashMatch) {
      const q = (slashMatch[1] || '').toLowerCase();
      mentionStart = 0;
      mentionOpen = true;
      const skills = cachedSkills.length > 0 ? cachedSkills : [
        { name: 'review', description: 'Code review' },
        { name: 'test', description: 'Write & run tests' },
        { name: 'fix', description: 'Fix diagnostics/bugs' },
        { name: 'commit', description: 'Summarize git changes' },
        { name: 'explain', description: 'Explain code' },
      ];
      mentionItems = skills
        .map((s) => ({
          item: { kind: 'skill', label: '/' + s.name, detail: s.description || 'Skill' },
          score: fuzzyScore(q, s.name),
        }))
        .filter((x) => x.score > 0)
        .sort((a, b) => b.score - a.score)
        .map((x) => x.item);
      if (mentionIndex >= mentionItems.length) {
        mentionIndex = 0;
      }
      renderMentionPopover();
      return;
    }

    const match = beforeCursor.match(/(?:^|\\s)@([^\\s@]*)$/);
    if (!match) {
      closeMentionPopover();
      return;
    }
    if (!filesRequested) {
      filesRequested = true;
      vscode.postMessage({ type: 'listFiles' });
    }
    const query = match[1] || '';
    mentionStart = beforeCursor.lastIndexOf('@');
    mentionOpen = true;
    mentionItems = getFilteredMentions(query);
    if (mentionIndex >= mentionItems.length) {
      mentionIndex = 0;
    }
    renderMentionPopover();
  }

  function send() {
    const text = promptEl.value.trim();
    const imagesToSend = pendingImages.slice();
    if (!text && imagesToSend.length === 0) return;

    const includeGitDiff = Boolean(gitEl.checked) || /(?:^|\\s)@git(?:\\s|$)/.test(text);
    const includeWorkspace = Boolean(wsEl.checked) || /(?:^|\\s)@workspace(?:\\s|$)/.test(text);

    addMessage('user', text, imagesToSend);
    activeAssistantEl = addMessage('assistant', '');
    activeAssistantRaw = '';
    promptEl.value = '';
    pendingImages = [];
    gitEl.checked = false;
    closeMentionPopover();
    renderChips(lastActiveFile, lastAttachedFiles);

    vscode.postMessage({
      type: 'send',
      text,
      images: imagesToSend,
      mode: modeEl.value,
      model: modelEl.value,
      includeWorkspace,
      includeGitDiff,
      autoApply: autoApplyEl.checked,
      autoVerify: autoVerifyEl.checked,
    });
  }

  document.getElementById('sendBtn').onclick = send;
  document.getElementById('stopBtn').onclick = () => vscode.postMessage({ type: 'stop' });
  document.getElementById('clearBtn').onclick = () => {
    messagesEl.innerHTML = '';
    toolPills.clear();
    toolCards.clear();
    vscode.postMessage({ type: 'clear' });
  };
  document.getElementById('newSessionBtn').onclick = () => vscode.postMessage({ type: 'newSession' });
  document.getElementById('delSessionBtn').onclick = () => vscode.postMessage({ type: 'deleteSession' });
  sessionSelectEl.onchange = () => {
    if (sessionSelectEl.value) {
      vscode.postMessage({ type: 'switchSession', sessionId: sessionSelectEl.value });
    }
  };
  document.getElementById('keyBtn').onclick = () => vscode.postMessage({ type: 'setKey' });
  document.getElementById('addFileBtn').onclick = () => vscode.postMessage({ type: 'pickFile' });
  document.getElementById('addImgBtn').onclick = () => imgFileInput.click();
  document.getElementById('previewBtn').onclick = () => vscode.postMessage({ type: 'openPreview' });
  imgFileInput.onchange = () => {
    for (const file of imgFileInput.files || []) {
      readImageFile(file);
    }
    imgFileInput.value = '';
  };
  document.getElementById('acceptAllBtn').onclick = () => vscode.postMessage({ type: 'acceptAllEdits' });
  document.getElementById('revertAllBtn').onclick = () => vscode.postMessage({ type: 'revertAllEdits' });
  reloadMcpBtn.onclick = () => {
    reloadMcpBtn.disabled = true;
    reloadMcpBtn.textContent = 'Reloading…';
    vscode.postMessage({ type: 'reloadMcp' });
  };
  modelEl.onchange = () => vscode.postMessage({ type: 'modelChanged', model: modelEl.value });

  promptEl.addEventListener('input', updateMentionState);
  promptEl.addEventListener('click', () => {
    if (mentionOpen) updateMentionState();
  });
  promptEl.addEventListener('paste', (e) => {
    const items = e.clipboardData && e.clipboardData.items ? Array.from(e.clipboardData.items) : [];
    const imgItems = items.filter((it) => it.kind === 'file' && it.type.startsWith('image/'));
    if (imgItems.length > 0) {
      const hasText = Boolean(e.clipboardData.getData('text/plain'));
      if (!hasText) {
        e.preventDefault();
      }
      for (const it of imgItems) {
        const f = it.getAsFile();
        if (f) readImageFile(f);
      }
    }
  });
  promptEl.addEventListener('keydown', (e) => {
    if (mentionOpen && mentionItems.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        mentionIndex = (mentionIndex + 1) % mentionItems.length;
        renderMentionPopover();
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        mentionIndex = (mentionIndex - 1 + mentionItems.length) % mentionItems.length;
        renderMentionPopover();
        return;
      }
      if ((e.key === 'Enter' && !e.shiftKey) || e.key === 'Tab') {
        e.preventDefault();
        selectMention(mentionItems[mentionIndex]);
        return;
      }
      if (e.key === 'Escape') {
        e.preventDefault();
        closeMentionPopover();
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
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

  function renderSessions(sessions, currentId) {
    const list = Array.isArray(sessions) ? sessions : [];
    sessionSelectEl.innerHTML = '';
    const hasCurrent = list.some((s) => s.sessionId === currentId);
    if (!hasCurrent && currentId) {
      const opt = document.createElement('option');
      opt.value = currentId;
      opt.textContent = 'New Session';
      sessionSelectEl.appendChild(opt);
    }
    for (const s of list) {
      const opt = document.createElement('option');
      opt.value = s.sessionId;
      opt.textContent = (s.title || s.sessionId) + ' (' + (s.messageCount || 0) + ')';
      sessionSelectEl.appendChild(opt);
    }
    if (currentId) {
      sessionSelectEl.value = currentId;
    }
  }

  function renderStatus(msg) {
    reloadMcpBtn.disabled = false;
    reloadMcpBtn.textContent = 'Reload MCP';
    const servers = Array.isArray(msg.mcpServers) ? msg.mcpServers : [];
    const connected = servers.filter((s) => s.connected).length;
    if (servers.length === 0) {
      mcpPillEl.textContent = 'MCP: 0 servers';
      mcpPillEl.title = 'No MCP servers configured (.rursor/mcp.json)';
    } else if (connected === servers.length) {
      mcpPillEl.textContent = 'MCP: ' + servers.length + ' server' + (servers.length === 1 ? '' : 's');
      mcpPillEl.title = servers.map((s) => s.name + ': ' + s.toolCount + ' tools').join('\\n');
    } else {
      mcpPillEl.textContent = 'MCP: ' + connected + '/' + servers.length + ' servers';
      mcpPillEl.title = servers
        .map((s) => s.name + ': ' + (s.connected ? s.toolCount + ' tools' : (s.error || 'disconnected')))
        .join('\\n');
    }
    rulesPillEl.hidden = !msg.hasWorkspaceRules;
  }

  window.viewDiff = (callId) => vscode.postMessage({ type: 'viewDiff', callId });
  window.decideEdit = (callId, decision) => vscode.postMessage({ type: 'editDecision', callId, text: decision });
  window.revertEdit = (callId) => vscode.postMessage({ type: 'revertEdit', callId });
  window.decideCmd = (callId, decision) => vscode.postMessage({ type: 'commandDecision', callId, text: decision });
  window.restoreCheckpoint = (checkpointId) => vscode.postMessage({ type: 'restoreCheckpoint', checkpointId });

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
      reloadMcpBtn.disabled = false;
      reloadMcpBtn.textContent = 'Reload MCP';
    } else if (msg.type === 'contextSync') {
      latestDiagnostics = Array.isArray(msg.diagnostics) ? msg.diagnostics : [];
      renderChips(msg.activeFile, msg.attachedFiles);
    } else if (msg.type === 'filesList') {
      cachedFiles = Array.isArray(msg.files) ? msg.files : [];
      if (mentionOpen) {
        updateMentionState();
      }
    } else if (msg.type === 'skillsList') {
      cachedSkills = Array.isArray(msg.skills) ? msg.skills : [];
    } else if (msg.type === 'checkpointCreated' && msg.checkpoint) {
      const cp = msg.checkpoint;
      const pill = document.createElement('div');
      pill.className = 'tool-pill';
      pill.innerHTML =
        '<span>Checkpoint: ' + escHtml(cp.label || cp.checkpointId) + ' (' + (cp.fileCount || 0) + ' files)</span> ' +
        '<button onclick="restoreCheckpoint(\\'' + escHtml(cp.checkpointId) + '\\')">Restore</button>';
      messagesEl.appendChild(pill);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    } else if (msg.type === 'checkpointRestored') {
      const pill = document.createElement('div');
      pill.className = 'tool-pill';
      pill.textContent = 'Restored checkpoint ' + (msg.checkpointId || '') + ' (' + (msg.restoredCount || 0) + ' change(s)) ✓';
      messagesEl.appendChild(pill);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    } else if (msg.type === 'subagentEvent' && msg.payload) {
      const ev = msg.payload;
      const pill = document.createElement('div');
      pill.className = 'tool-pill';
      pill.textContent = 'Subagent [' + (ev.taskId || 'task') + ']: ' + (ev.status || '');
      messagesEl.appendChild(pill);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    } else if (msg.type === 'sessionsSync') {
      renderSessions(msg.sessions, msg.currentSessionId);
    } else if (msg.type === 'sessionLoaded') {
      messagesEl.innerHTML = '';
      toolPills.clear();
      toolCards.clear();
      activeAssistantEl = null;
      activeAssistantRaw = '';
      for (const m of msg.messages || []) {
        addMessage(m.role === 'assistant' ? 'assistant' : 'user', m.content || '', m.images);
      }
      if (msg.sessionId) {
        let optExists = false;
        for (const opt of sessionSelectEl.options) {
          if (opt.value === msg.sessionId) optExists = true;
        }
        if (!optExists) {
          const opt = document.createElement('option');
          opt.value = msg.sessionId;
          opt.textContent = 'New Session';
          sessionSelectEl.insertBefore(opt, sessionSelectEl.firstChild);
        }
        sessionSelectEl.value = msg.sessionId;
      }
    } else if (msg.type === 'reviewSync') {
      const count = msg.fileCount || 0;
      if (count > 0) {
        reviewTextEl.textContent = count + ' file(s) changed';
        reviewBarEl.hidden = false;
      } else {
        reviewBarEl.hidden = true;
      }
    } else if (msg.type === 'statusSync') {
      renderStatus(msg);
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
    } else if (msg.type === 'toolCardStream') {
      const entry = toolCards.get(msg.callId);
      if (entry) {
        entry.card.output = (entry.card.output || '') + (msg.chunk || '');
        if (entry.card.output.length > 16000) {
          entry.card.output = entry.card.output.slice(-16000);
        }
        let outEl = entry.el.querySelector('.cmd-out');
        if (!outEl) {
          outEl = document.createElement('div');
          outEl.className = 'cmd-out';
          const actionsEl = entry.el.querySelector('.tool-card-actions');
          if (actionsEl) {
            entry.el.insertBefore(outEl, actionsEl);
          } else {
            entry.el.appendChild(outEl);
          }
        }
        outEl.textContent = entry.card.output;
        outEl.scrollTop = outEl.scrollHeight;
        messagesEl.scrollTop = messagesEl.scrollHeight;
      }
    } else if (msg.type === 'toolCardUpdate') {
      const entry = toolCards.get(msg.callId);
      if (entry) {
        Object.assign(entry.card, msg);
        updateToolCardElement(entry.el, entry.card);
      }
    }
  });

  vscode.postMessage({ type: 'webviewReady' });
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
  cwd: string,
  onOutput?: (chunk: string) => void
): Promise<{ exitCode: number; combinedOutput: string }> {
  return new Promise((resolve) => {
    const child = spawn('/bin/zsh', ['-lc', command], { cwd });
    let combined = '';
    const append = (chunk: Buffer) => {
      const text = chunk.toString('utf8');
      if (combined.length < 16_000) {
        combined += text;
      }
      onOutput?.(text);
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
