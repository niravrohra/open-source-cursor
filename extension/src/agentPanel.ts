import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import { spawn } from 'node:child_process';
import * as vscode from 'vscode';
import { buildInlineGenUiSrcdoc, INLINE_GENUI_HOST_RESIZE_SCRIPT } from './artifactStudio';
import {
  captureDiagnostics,
  TemporalIntentLedger,
  waitForLspDiagnosticsDelta,
} from './lspContext';
import { RursorSessionBaselineTracker } from './nativeForkFeatures';
import { SidecarClient, ToolEventPayload, ToolRequestPayload } from './sidecarClient';
import { TelemetryTracker } from './voiceAndTelemetry';
import { inspectWebUrl, RursorWebPreviewPanel } from './webPreview';

export type AgentMode = 'agent' | 'ask' | 'plan' | 'take-control';

export interface AgentPanelDeps {
  intentLedger?: TemporalIntentLedger;
  telemetry?: TelemetryTracker;
  baselineTracker?: RursorSessionBaselineTracker;
}

function normalizeAgentMode(mode: string | undefined): AgentMode {
  switch ((mode ?? 'agent').trim().toLowerCase()) {
    case 'ask':
      return 'ask';
    case 'plan':
      return 'plan';
    case 'take-control':
    case 'take_control':
    case 'computer':
    case 'computer-use':
    case 'pc':
      return 'take-control';
    default:
      return 'agent';
  }
}

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
    private readonly onModelChange: (model: string) => Promise<void>,
    private readonly deps?: AgentPanelDeps
  ) { }

  public async submitExternalPrompt(
    prompt: string,
    images: ImageAttachment[] = [],
    mode: AgentMode = 'agent'
  ): Promise<void> {
    await vscode.commands.executeCommand('rursor.openAgent');
    void this.view?.webview.postMessage({
      type: 'externalUserMessage',
      text: prompt,
      images,
    });
    const autoVerify = vscode.workspace
      .getConfiguration('rursor')
      .get<boolean>('agent.autoVerify', false);
    await this.handleUserPrompt(
      prompt,
      images,
      normalizeAgentMode(mode),
      true,
      false,
      false,
      autoVerify
    );
  }

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
        mode?: AgentMode | string;
        includeWorkspace?: boolean;
        includeGitDiff?: boolean;
        autoApply?: boolean;
        autoVerify?: boolean;
        userIndex?: number;
        callId?: string;
        filePath?: string;
        code?: string;
        sessionId?: string;
        checkpointId?: string;
        artifactPath?: string;
        external?: boolean;
        studioTarget?: string;
        tasks?: Array<{ id?: string; role?: string; prompt?: string }>;
      }) => {
        if (msg.type === 'webviewReady') {
          this.syncContextBar();
          this.syncReviewBar();
          await Promise.all([this.refreshSessions(), this.refreshStatus(), this.sendSkillsList()]);
        } else if (
          msg.type === 'send' &&
          (Boolean(msg.text) || (Array.isArray(msg.images) && msg.images.length > 0))
        ) {
          const mode = normalizeAgentMode(msg.mode);
          await this.handleUserPrompt(
            msg.text ?? '',
            msg.images ?? [],
            mode,
            Boolean(msg.includeWorkspace),
            Boolean(msg.includeGitDiff),
            Boolean(msg.autoApply),
            Boolean(msg.autoVerify),
            msg.model
          );
        } else if (msg.type === 'editResubmit' && typeof msg.userIndex === 'number' && msg.text) {
          await this.editAndResubmit(
            msg.userIndex,
            msg.text,
            normalizeAgentMode(msg.mode),
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
        } else if (msg.type === 'openSettings') {
          await vscode.commands.executeCommand('rursor.openSettings');
        } else if (msg.type === 'modelChanged' && msg.model) {
          await this.onModelChange(msg.model);
        } else if (msg.type === 'modeChanged' && msg.mode) {
          if (normalizeAgentMode(msg.mode) === 'take-control') {
            void vscode.commands.executeCommand('rursor.openBrowserStudio');
          }
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
        } else if (msg.type === 'openStudioArtifact' && msg.artifactPath) {
          const uri = vscode.Uri.file(msg.artifactPath);
          if (msg.external) {
            await vscode.env.openExternal(uri);
          } else {
            await vscode.commands.executeCommand('rursor.previewArtifact', uri);
          }
        } else if (msg.type === 'launchStudio' && msg.studioTarget) {
          switch (msg.studioTarget) {
            case 'swarm':
              await vscode.commands.executeCommand('rursor.missionControl', 'swarm');
              break;
            case 'arena':
              await vscode.commands.executeCommand('rursor.missionControl', 'arena');
              break;
            case 'browser':
              await vscode.commands.executeCommand('rursor.openBrowserStudio');
              break;
            case 'artifacts':
              await vscode.commands.executeCommand('rursor.openArtifactStudio');
              break;
            case 'canvas':
              await vscode.commands.executeCommand('rursor.openCanvas');
              break;
            case 'addDocs':
              await vscode.commands.executeCommand('rursor.addDocsSource');
              break;
            case 'mcpStore':
              await vscode.commands.executeCommand('rursor.installMcpPreset');
              break;
          }
        } else if (msg.type === 'spawnSubagents') {
          await this.spawnSubagentsFromUi(msg.tasks ?? [], msg.model, msg.text);
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

  private async cancelActiveRequest(opts?: { silent?: boolean }): Promise<void> {
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
    if (!opts?.silent) {
      void this.view?.webview.postMessage({ type: 'done' });
    }
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

  private async spawnSubagentsFromUi(
    tasksIn: Array<{ id?: string; role?: string; prompt?: string }>,
    model?: string,
    fallbackPrompt?: string
  ): Promise<void> {
    const cfg = vscode.workspace.getConfiguration('rursor');
    const activeModel = model ?? cfg.get<string>('defaultModel', 'gemini-3.8-flash');
    const basePrompt = (fallbackPrompt ?? '').trim();
    let tasks = tasksIn
      .map((t, i) => ({
        id: (t.id ?? `sub_${i + 1}`).trim() || `sub_${i + 1}`,
        role: (t.role ?? 'researcher').trim() || 'researcher',
        prompt: (t.prompt ?? '').trim(),
      }))
      .filter((t) => t.prompt.length > 0);

    if (tasks.length === 0 && basePrompt) {
      tasks = [
        { id: 'explore', role: 'explorer', prompt: `Map relevant files and entry points for: ${basePrompt}` },
        { id: 'risks', role: 'reviewer', prompt: `Identify risks, edge cases, and regressions for: ${basePrompt}` },
        { id: 'plan', role: 'planner', prompt: `Propose a concise implementation outline for: ${basePrompt}` },
      ];
    }

    if (tasks.length === 0) {
      void this.view?.webview.postMessage({
        type: 'error',
        text: 'Add a prompt (or task prompts) before spawning subagents.',
      });
      return;
    }

    void this.view?.webview.postMessage({
      type: 'subagentBatchStarted',
      count: tasks.length,
    });

    try {
      const { promise } = this.client.request<{
        results: Array<{ id: string; role: string; ok: boolean; output: string }>;
      }>(
        'run_parallel_subagents',
        { tasks, model: activeModel },
        {
          onSubagentEvent: (payload) => {
            void this.view?.webview.postMessage({ type: 'subagentEvent', payload });
          },
        }
      );
      const res = await promise;
      void this.view?.webview.postMessage({
        type: 'subagentBatchDone',
        results: res.results ?? [],
      });
    } catch (err) {
      void this.view?.webview.postMessage({
        type: 'error',
        text: `Subagent spawn failed: ${err instanceof Error ? err.message : String(err)}`,
      });
    }
  }

  private async editAndResubmit(
    userIndex: number,
    text: string,
    mode: AgentMode,
    includeWorkspace: boolean,
    includeGitDiff: boolean,
    autoApply: boolean,
    autoVerify: boolean,
    model?: string
  ): Promise<void> {
    await this.cancelActiveRequest({ silent: true });

    let seen = 0;
    let cutAt = -1;
    for (let i = 0; i < this.history.length; i++) {
      if (this.history[i].role !== 'user') {
        continue;
      }
      if (seen === userIndex) {
        cutAt = i;
        break;
      }
      seen += 1;
    }
    if (cutAt < 0) {
      void this.view?.webview.postMessage({
        type: 'error',
        text: 'Could not find that message to edit.',
      });
      return;
    }

    const priorImages = this.history[cutAt]?.images ?? [];
    this.history = this.history.slice(0, cutAt);
    await this.handleUserPrompt(
      text,
      priorImages,
      mode,
      includeWorkspace,
      includeGitDiff,
      autoApply,
      autoVerify,
      model
    );
  }

  private async handleUserPrompt(
    prompt: string,
    images: ImageAttachment[],
    mode: AgentMode,
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

    const docsMatch = prompt.match(/(?:^|\s)@docs(?::([a-zA-Z0-9_-]+))?(?:\s|$)/i);
    if (docsMatch) {
      const requestedSource = docsMatch[1]?.trim() || undefined;
      try {
        const docsRes = await this.client.request<{
          hits: Array<{
            sourceName: string;
            url: string;
            title: string;
            sectionHeading: string;
            content: string;
          }>;
        }>('search_docs', {
          query: prompt.replace(/@docs(?::[a-zA-Z0-9_-]+)?/gi, '').trim() || prompt,
          source: requestedSource,
          limit: 5,
        }).promise;
        const hits = docsRes.hits ?? [];
        if (hits.length > 0) {
          const formattedDocs = hits
            .map(
              (h) =>
                `### [${h.sourceName}] ${h.title} — ${h.sectionHeading}\nURL: ${h.url}\n${h.content}`
            )
            .join('\n\n---\n\n');
          contextFiles.push({
            path: `@docs/${requestedSource ?? 'all'}`,
            content: formattedDocs.slice(0, 16_000),
          });
        }
      } catch {
        // Ignore if no docs indexed yet
      }
    }

    const diagnostics = collectWorkspaceDiagnostics();
    const cfg = vscode.workspace.getConfiguration('rursor');
    const thinkingBudget = cfg.get<number>('agent.thinkingBudget', 0);
    const activeModel = model ?? cfg.get<string>('defaultModel', 'gemini-3.8-flash');
    const developerIntent = this.deps?.intentLedger?.formatRecentIntent() || undefined;
    const estInputChars =
      this.history.reduce((acc, m) => acc + m.content.length, 0) +
      contextFiles.reduce((acc, c) => acc + c.content.length, 0);
    const turnTimer = this.deps?.telemetry?.startTurn(
      mode,
      activeModel,
      Math.ceil(estInputChars / 4),
      0
    );

    void this.view?.webview.postMessage({ type: 'turnStarted', mode, model: activeModel });

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
        developerIntent,
        useWorkspaceSearch: includeWorkspace,
        includeGitDiff,
        autoVerify: autoVerify || cfg.get<boolean>('agent.autoVerify', false),
        thinkingBudget: thinkingBudget > 0 ? thinkingBudget : undefined,
        mode,
        model,
      },
      {
        onStream: (delta) => {
          turnTimer?.markFirstToken();
          void this.view?.webview.postMessage({ type: 'delta', text: delta });
        },
        onThinking: (delta) => {
          turnTimer?.markFirstToken();
          void this.view?.webview.postMessage({ type: 'thinking', text: delta });
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
        onSwarmEvent: (payload) => {
          void this.view?.webview.postMessage({ type: 'swarmEvent', payload });
        },
        onArtifactCreated: (payload) => {
          const kind = String(payload.kind ?? '');
          if (kind === 'gen_ui') {
            const iframeHtml = buildInlineGenUiSrcdoc(
              String(payload.title ?? 'Interactive Widget'),
              String(payload.html ?? '')
            );
            void this.view?.webview.postMessage({
              type: 'artifactCreated',
              artifact: {
                ...payload,
                iframeHtml,
              },
            });
          } else {
            void this.view?.webview.postMessage({
              type: 'artifactCreated',
              artifact: payload,
            });
          }
        },
      }
    );

    this.activeRequestId = id;
    try {
      const result = await promise;
      turnTimer?.finish(result.text.length);
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

    // Stage 1: Pre-write Tree-sitter syntax gate (<2ms in Rust sidecar) before touching disk.
    try {
      const { promise: syntaxPromise } = this.client.request<{
        validation?: { supported?: boolean; valid?: boolean; message?: string };
      }>('validate_syntax', {
        path: relPath,
        before: beforeContent,
        after: afterContent,
      });
      const syntaxRes = await syntaxPromise;
      if (syntaxRes.validation?.supported && syntaxRes.validation.valid === false) {
        await this.client.submitToolResult(req.callId, false, {
          syntaxError: true,
          error:
            syntaxRes.validation.message ||
            `Proposed edit to '${relPath}' introduced Tree-sitter syntax errors. Fix the syntax and retry.`,
        });
        return;
      }
    } catch {
      // Proceed if syntax gate RPC is unavailable
    }

    const fileUri = vscode.Uri.file(absPath);
    this.deps?.baselineTracker?.recordBaselineBeforeEdit(fileUri, beforeContent);

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
        const diagBefore = captureDiagnostics(fileUri);
        await applyFileWrite(absPath, afterContent);
        await this.refreshSidecarFile(absPath, afterContent);
        this.deps?.baselineTracker?.markFileModified(fileUri, afterContent);
        const diagDelta = await waitForLspDiagnosticsDelta(fileUri, diagBefore, 220);
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
          ...(diagDelta.introduced.length > 0
            ? { introducedDiagnostics: diagDelta.summary }
            : {}),
          resolvedDiagnostics: diagDelta.resolved.length,
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
      const diagBefore = captureDiagnostics(fileUri);
      await applyFileWrite(absPath, afterContent);
      await this.refreshSidecarFile(absPath, afterContent);
      this.deps?.baselineTracker?.markFileModified(fileUri, afterContent);
      const diagDelta = await waitForLspDiagnosticsDelta(fileUri, diagBefore, 220);
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
        ...(diagDelta.introduced.length > 0
          ? { introducedDiagnostics: diagDelta.summary }
          : {}),
        resolvedDiagnostics: diagDelta.resolved.length,
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
        this.deps?.baselineTracker?.markFileModified(snap.absPath, snap.beforeContent);
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
    --radius: 12px;
    --accent: #ececec;
    --accent-soft: rgba(255, 255, 255, 0.05);
    --accent-border: rgba(255, 255, 255, 0.10);
    --bg: #0c0c0c;
    --glass: #141414;
    --surface: #111111;
    --surface-2: #181818;
    --elevated: #1c1c1c;
    --line: #262626;
    --line-strong: #333333;
    --fg: #ececec;
    --fg-muted: #a1a1a1;
    --fg-dim: #737373;
    --bubble: #262626;
    --mode-agent: #ececec;
    --mode-ask: #a3a3a3;
    --mode-plan: #86efac;
    --mode-control: #fda4af;
  }
  * { box-sizing: border-box; }
  body {
    font-family: var(--vscode-font-family);
    color: var(--fg);
    background: var(--bg);
    margin: 0;
    height: 100vh;
    font-size: 13px;
    overflow: hidden;
  }
  .app {
    display: grid;
    grid-template-columns: 1fr 220px;
    height: 100vh;
    min-height: 0;
  }
  @media (max-width: 720px) {
    .app { grid-template-columns: 1fr 168px; }
  }
  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    position: relative;
  }
  .main-top {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .main-bottom {
    flex-shrink: 0;
  }
  body.empty .main-top {
    justify-content: center;
  }
  body.empty #messages:empty + .context-chips {
    display: none;
  }
  body.empty .main {
    justify-content: center;
  }
  body.empty .main-top {
    flex: 0 0 auto;
    max-height: 0;
    overflow: hidden;
  }
  body.empty .main-bottom {
    flex: 1;
    display: flex;
    flex-direction: column;
    justify-content: center;
    padding-bottom: 18vh;
  }
  .rail {
    border-left: 1px solid var(--line);
    background: var(--bg);
    padding: 10px 8px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow: auto;
    position: relative;
  }
  .rail-item {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: var(--fg);
    padding: 8px 10px;
    border-radius: 8px;
    font-size: 12.5px;
    cursor: pointer;
  }
  .rail-item:hover { background: var(--elevated); }
  .rail-ico {
    width: 16px;
    text-align: center;
    opacity: 0.75;
    font-size: 12px;
  }
  .rail-section-label {
    margin: 14px 10px 6px;
    font-size: 11px;
    font-weight: 600;
    color: var(--fg-dim);
  }
  .session-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .session-row {
    display: flex;
    align-items: center;
    gap: 6px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: var(--fg);
    padding: 7px 8px;
    border-radius: 8px;
    font-size: 12px;
    cursor: pointer;
  }
  .session-row:hover { background: var(--elevated); }
  .session-row.active { background: var(--bubble); color: #fff; }
  .session-row .title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .session-row .ago {
    color: var(--fg-dim);
    font-size: 11px;
    flex-shrink: 0;
  }
  .session-search {
    margin: 6px 4px 0;
    width: calc(100% - 8px);
    background: var(--elevated);
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    color: var(--fg);
    padding: 6px 8px;
    font-size: 12px;
  }
  header {
    display: flex;
    gap: 6px;
    padding: 10px 12px 8px;
    border-bottom: 1px solid var(--line);
    align-items: center;
    flex-wrap: wrap;
    background: var(--surface);
  }
  .brand-row {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
  }
  .brand {
    font-size: 12px;
    font-weight: 650;
    letter-spacing: -0.01em;
    color: #f5f5f5;
    text-transform: none;
  }
  .mode-badge {
    font-size: 10px;
    font-weight: 650;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--line);
    background: rgba(255,255,255,0.04);
    color: #d4d4d4;
  }
  .mode-badge[data-mode="agent"] { color: var(--mode-agent); }
  .mode-badge[data-mode="ask"] { color: var(--mode-ask); }
  .mode-badge[data-mode="plan"] { color: var(--mode-plan); border-color: rgba(134,239,172,0.35); }
  .mode-badge[data-mode="take-control"] { color: var(--mode-control); border-color: rgba(253,164,175,0.35); }
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
    background: rgba(127,127,127,0.14);
    color: var(--vscode-foreground);
    border: 1px solid transparent;
    padding: 4px 8px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 11px;
  }
  button:hover { background: rgba(127,127,127,0.22); }
  button:disabled { opacity: 0.5; cursor: default; }
  button.primary {
    background: #f5f5f5;
    color: #111;
    border: none;
    font-weight: 650;
  }
  button.danger {
    background: rgba(215, 58, 73, 0.18);
    color: var(--vscode-errorForeground);
  }
  #messages {
    flex: 1;
    min-height: 0;
    overflow-x: hidden;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 18px 18px 12px;
    display: flex;
    flex-direction: column;
    gap: 0;
    -webkit-overflow-scrolling: touch;
  }
  #messagesSpacer {
    flex: 1 1 auto;
    min-height: 0;
    pointer-events: none;
  }
  .msg {
    padding: 0;
    border-radius: 0;
    font-size: 13.5px;
    line-height: 1.65;
    word-break: break-word;
    overflow-wrap: anywhere;
    max-width: 100%;
    flex: 0 0 auto;
    margin: 0 0 14px 0;
  }
  .msg:last-child { margin-bottom: 4px; }
  .user {
    background: var(--bubble);
    border: none;
    align-self: flex-start;
    max-width: 92%;
    white-space: pre-wrap;
    padding: 10px 12px;
    border-radius: 14px;
    position: relative;
  }
  .user .msg-text { display: block; }
  .user-edit-btn {
    position: absolute;
    top: 6px;
    right: 6px;
    width: 24px;
    height: 24px;
    border-radius: 6px;
    border: none;
    background: rgba(0,0,0,0.35);
    color: var(--fg-muted);
    display: none;
    align-items: center;
    justify-content: center;
    padding: 0;
    cursor: pointer;
  }
  .user:hover .user-edit-btn { display: inline-flex; }
  .user-edit-btn:hover { background: rgba(255,255,255,0.12); color: #fff; }
  .user.editing {
    background: var(--elevated);
    border: 1px solid var(--line-strong);
    padding: 10px;
  }
  .user.editing .user-edit-btn,
  .user.editing .msg-text,
  .user.editing .msg-images { display: none !important; }
  .user-edit-box {
    width: 100%;
    min-height: 72px;
    resize: vertical;
    background: transparent;
    color: var(--fg);
    border: none;
    outline: none;
    font: inherit;
    line-height: 1.5;
    padding: 0;
  }
  .user-edit-actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    margin-top: 8px;
  }
  .user-edit-actions button {
    background: var(--bubble);
    border: 1px solid var(--line-strong);
    color: var(--fg);
    border-radius: 8px;
    padding: 5px 10px;
    font-size: 12px;
  }
  .user-edit-actions button.primary {
    background: #f4f4f4;
    color: #111;
    border: none;
    font-weight: 600;
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
    border-radius: 6px;
    border: 1px solid var(--line);
    object-fit: cover;
  }
  .assistant {
    background: transparent;
    border: none;
    align-self: stretch;
    color: var(--fg);
  }
  .assistant .msg-body {
    display: block;
    min-height: 0;
    white-space: normal;
  }
  .assistant p {
    margin: 0 0 0.85em 0;
  }
  .assistant p:last-child { margin-bottom: 0; }
  .assistant h1, .assistant h2, .assistant h3, .assistant h4 {
    margin: 1em 0 0.45em;
    font-size: 1.05em;
    font-weight: 650;
    line-height: 1.35;
    color: #f5f5f5;
  }
  .assistant h1:first-child, .assistant h2:first-child, .assistant h3:first-child { margin-top: 0; }
  .assistant ul, .assistant ol {
    margin: 0.35em 0 0.9em;
    padding: 0 0 0 1.35em;
  }
  .assistant li { margin: 0.25em 0; }
  .assistant li::marker { color: var(--fg-dim); }
  .assistant code {
    font-family: var(--vscode-editor-font-family, monospace);
    background: rgba(127, 127, 127, 0.16);
    padding: 1px 5px;
    border-radius: 4px;
    font-size: 12px;
  }
  .code-wrap {
    margin: 0.75em 0 1em;
    border: 1px solid var(--line);
    border-radius: 8px;
    overflow: hidden;
    background: rgba(0,0,0,0.22);
  }
  .code-bar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 8px;
    font-size: 11px;
    opacity: 0.85;
    border-bottom: 1px solid var(--line);
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
    opacity: 0.9;
    padding: 4px 9px;
    border-radius: 999px;
    background: rgba(127, 127, 127, 0.12);
    border: 1px solid var(--line);
    align-self: flex-start;
    display: inline-flex;
    align-items: center;
    gap: 6px;
    max-width: 100%;
  }
  .tool-pill.running {
    border-color: rgba(56, 189, 248, 0.35);
    background: rgba(56, 189, 248, 0.08);
  }
  .tool-pill.ok { color: #a7f3d0; }
  .tool-pill.fail { color: #fda4af; }
  .spin {
    width: 10px;
    height: 10px;
    border-radius: 50%;
    border: 1.5px solid rgba(255,255,255,0.2);
    border-top-color: #7dd3fc;
    animation: spin 0.7s linear infinite;
    flex-shrink: 0;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .thinking-row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-self: stretch;
    animation: fadeSlideIn 0.22s ease;
  }
  .thinking-status {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    color: var(--fg-muted);
    letter-spacing: 0.01em;
  }
  .thinking-status .shimmer-label {
    background: linear-gradient(90deg, #737373 0%, #e5e5e5 45%, #737373 90%);
    background-size: 200% 100%;
    -webkit-background-clip: text;
    background-clip: text;
    color: transparent;
    animation: shimmerText 1.4s ease-in-out infinite;
    font-weight: 500;
  }
  @keyframes shimmerText {
    0% { background-position: 100% 0; }
    100% { background-position: -100% 0; }
  }
  .thinking-dots span {
    display: inline-block;
    width: 3px;
    height: 3px;
    margin-left: 2px;
    border-radius: 50%;
    background: #a3a3a3;
    animation: thinkDot 1.2s ease-in-out infinite;
  }
  .thinking-dots span:nth-child(2) { animation-delay: 0.15s; }
  .thinking-dots span:nth-child(3) { animation-delay: 0.3s; }
  @keyframes thinkDot {
    0%, 80%, 100% { opacity: 0.25; transform: translateY(0); }
    40% { opacity: 1; transform: translateY(-2px); }
  }
  .thought-block {
    border: 1px solid #2f2f2f;
    border-radius: 10px;
    background: rgba(255,255,255,0.03);
    overflow: hidden;
    align-self: stretch;
    animation: fadeSlideIn 0.2s ease;
  }
  .thought-toggle {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 8px;
    background: transparent;
    border: none;
    color: var(--fg-muted);
    font-size: 12px;
    padding: 8px 10px;
    cursor: pointer;
    text-align: left;
  }
  .thought-toggle:hover { color: var(--fg); background: rgba(255,255,255,0.03); }
  .thought-toggle .chev {
    transition: transform 0.15s ease;
    opacity: 0.7;
    font-size: 10px;
  }
  .thought-block.open .thought-toggle .chev { transform: rotate(90deg); }
  .thought-body {
    display: none;
    padding: 0 12px 10px;
    color: #9ca3af;
    font-size: 12px;
    line-height: 1.5;
    white-space: pre-wrap;
    max-height: 220px;
    overflow-y: auto;
  }
  .thought-block.open .thought-body { display: block; }
  .assistant.streaming .stream-caret {
    display: inline-block;
    width: 7px;
    height: 1.05em;
    margin-left: 1px;
    vertical-align: text-bottom;
    background: #e5e5e5;
    animation: caretBlink 1s steps(1) infinite;
  }
  @keyframes caretBlink {
    0%, 45% { opacity: 1; }
    50%, 100% { opacity: 0; }
  }
  .assistant.streaming .msg-body {
    animation: streamIn 0.12s ease;
  }
  @keyframes streamIn {
    from { opacity: 0.86; }
    to { opacity: 1; }
  }
  @keyframes fadeSlideIn {
    from { opacity: 0; transform: translateY(4px); }
    to { opacity: 1; transform: translateY(0); }
  }
  .activity-bar {
    display: none;
    align-items: center;
    gap: 8px;
    padding: 0 4px 8px;
    font-size: 11.5px;
    color: var(--fg-muted);
  }
  body.generating .activity-bar { display: flex; }
  .activity-bar .pulse-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #38bdf8;
    box-shadow: 0 0 0 0 rgba(56, 189, 248, 0.55);
    animation: pulseDot 1.4s ease-out infinite;
  }
  @keyframes pulseDot {
    0% { box-shadow: 0 0 0 0 rgba(56, 189, 248, 0.45); }
    70% { box-shadow: 0 0 0 8px rgba(56, 189, 248, 0); }
    100% { box-shadow: 0 0 0 0 rgba(56, 189, 248, 0); }
  }
  .send-btn.stop-mode {
    background: #3f3f3f !important;
    color: #fff !important;
  }
  .send-btn.stop-mode svg { display: none; }
  .send-btn.stop-mode::before {
    content: '';
    width: 10px;
    height: 10px;
    border-radius: 2px;
    background: currentColor;
  }
  .tool-card {
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: rgba(127,127,127,0.06);
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
    background: rgba(0,0,0,0.2);
    padding: 6px;
    border-radius: 6px;
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
    padding: 8px 14px 6px;
    background: transparent;
    border-top: none;
    font-size: 12px;
    font-weight: 500;
  }
  .review-bar[hidden] { display: none; }
  .review-actions { display: flex; gap: 8px; align-items: center; }
  .files-toggle {
    background: transparent;
    border: none;
    color: var(--fg-muted);
    padding: 0;
    font-size: 12px;
  }
  .review-actions button {
    background: transparent;
    border: none;
    color: var(--fg);
    font-size: 12px;
    padding: 4px 8px;
    border-radius: 6px;
  }
  .review-actions button:hover { background: var(--bubble); }
  .review-primary {
    background: #2f2f2f !important;
    border: 1px solid #3f3f3f !important;
    color: #fff !important;
    font-weight: 600 !important;
  }
  .drop-overlay {
    position: absolute;
    inset: 8px;
    border: 1.5px dashed #666;
    border-radius: 14px;
    background: rgba(12,12,12,0.94);
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--fg);
    font-size: 13px;
    z-index: 30;
    pointer-events: none;
  }
  .drop-overlay[hidden] { display: none; }
  .composer-shell { position: relative; }
  .composer-shell.drag-over .composer-card {
    border-color: var(--line-strong);
    background: var(--bubble);
  }
  .top-slim {
    display: flex;
    gap: 6px;
    align-items: center;
    padding: 8px 10px;
    border-bottom: 1px solid var(--line);
    background: var(--surface);
    position: relative;
  }
  .top-slim #sessionSelect { flex: 1; min-width: 0; }
  .icon-btn {
    width: 28px;
    height: 28px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    background: transparent;
    border: none;
    color: var(--fg-muted);
  }
  .icon-btn:hover { background: var(--bubble); color: #fff; }
  .send-btn { color: #eee; }
  .composer-card {
    min-height: 92px;
  }
  footer {
    padding: 8px 10px 10px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    background: transparent;
  }
  .chips {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    align-items: center;
  }
  .chip {
    font-size: 11px;
    padding: 2px 7px;
    border-radius: 999px;
    background: rgba(127, 127, 127, 0.16);
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
  .composer-shell {
    position: relative;
    border-radius: 16px;
    padding: 0;
    background: transparent;
    box-shadow: 0 8px 24px rgba(0,0,0,0.35);
  }
  .composer-card {
    border-radius: 16px;
    background: var(--elevated);
    border: 1px solid var(--line-strong);
    padding: 12px 12px 10px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .composer-wrap { position: relative; }
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
  .mention-label { font-weight: 600; white-space: nowrap; }
  .mention-detail {
    font-size: 11px;
    opacity: 0.78;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  textarea {
    width: 100%;
    min-height: 64px;
    resize: vertical;
    background: transparent;
    color: var(--vscode-input-foreground);
    border: none;
    outline: none;
    padding: 4px 2px;
    font-family: inherit;
    font-size: 13px;
    line-height: 1.45;
  }
  .composer-toolbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .composer-left, .composer-right {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .mode-select, .model-select {
    appearance: none;
    border: 1px solid #3a3a3a;
    background: var(--bubble);
    border-radius: 999px;
    padding: 5px 11px;
    font-size: 12px;
    font-weight: 600;
    color: #eee;
  }
  .mode-select {
    color: #fff;
    border-color: #444;
    background: #2f2f2f;
  }
  .icon-btn {
    width: 28px;
    height: 28px;
    border-radius: 999px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0;
    font-size: 13px;
  }
  .row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 11px;
    gap: 6px;
    flex-wrap: wrap;
  }
  .toggles {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    opacity: 0.92;
  }
  .toggles label { display: inline-flex; gap: 4px; align-items: center; cursor: pointer; }
  .status-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 6px;
    font-size: 11px;
    opacity: 0.9;
  }
  .status-pills {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .status-pill {
    font-size: 10px;
    padding: 2px 7px;
    border-radius: 999px;
    background: rgba(127, 127, 127, 0.14);
    border: 1px solid var(--line);
  }
  .studio-strip {
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    align-items: center;
  }
  .studio-strip button {
    font-size: 10px;
    padding: 3px 8px;
    border-radius: 999px;
    background: var(--elevated);
    border: 1px solid var(--line-strong);
    color: var(--fg);
  }
  .studio-strip button:hover {
    background: var(--bubble);
  }
  .studio-card {
    border: 1px solid var(--line-strong);
    border-radius: 10px;
    padding: 9px 11px;
    background: var(--elevated);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .studio-card-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    font-size: 12px;
  }
  .studio-badge {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    padding: 2px 6px;
    border-radius: 999px;
    background: var(--bubble);
    color: var(--fg);
  }
  .studio-card-meta {
    font-size: 11px;
    opacity: 0.8;
    word-break: break-all;
  }
  .studio-card-actions {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .studio-genui-wrap {
    margin: 4px 0;
    width: 100%;
  }
  .subagents-dock {
    border: 1px solid var(--line);
    border-radius: 12px;
    background: rgba(127,127,127,0.06);
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .subagents-dock[hidden] { display: none; }
  .subagents-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    font-weight: 700;
  }
  .subagent-grid {
    display: grid;
    grid-template-columns: 1fr;
    gap: 6px;
  }
  .subagent-card {
    border: 1px solid var(--line);
    border-radius: 9px;
    padding: 7px 8px;
    background: rgba(0,0,0,0.16);
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .subagent-card .top {
    display: flex;
    justify-content: space-between;
    gap: 8px;
    font-size: 11px;
    font-weight: 600;
  }
  .subagent-card .role {
    opacity: 0.75;
    font-weight: 500;
  }
  .subagent-card .status {
    font-size: 10px;
    padding: 1px 6px;
    border-radius: 999px;
    background: rgba(127,127,127,0.18);
  }
  .subagent-card .status.running { color: #38bdf8; }
  .subagent-card .status.ok { color: #34d399; }
  .subagent-card .status.fail { color: #fb7185; }
  .subagent-card .out {
    font-size: 11px;
    opacity: 0.85;
    max-height: 72px;
    overflow: auto;
    white-space: pre-wrap;
  }
  .ghost {
    background: transparent;
    border: 1px solid var(--line);
  }
  .more-menu {
    position: absolute;
    top: 150px;
    left: 8px;
    right: 8px;
    width: auto;
    z-index: 40;
    background: var(--elevated);
    border: 1px solid var(--line-strong);
    border-radius: 12px;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    box-shadow: 0 12px 32px rgba(0,0,0,0.45);
  }
  .more-menu[hidden] { display: none; }
  .more-menu > button {
    text-align: left;
    background: transparent;
    border: none;
    color: var(--fg);
    padding: 7px 8px;
    border-radius: 8px;
    font-size: 12px;
  }
  .more-menu > button:hover { background: var(--bubble); }
  .more-toggles {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 6px;
    border-top: 1px solid #2f2f2f;
    border-bottom: 1px solid #2f2f2f;
    margin: 4px 0;
    font-size: 11px;
    color: var(--fg-muted);
  }
  .more-status {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
    padding: 4px 2px;
  }
  header { position: relative; }
  .context-chips {
    padding: 0 12px 6px;
    min-height: 0;
  }
  .context-chips:empty { display: none; }
  footer {
    border-top: none;
    background: transparent;
    padding: 8px 16px 16px;
    position: relative;
  }
  textarea {
    min-height: 44px;
    max-height: 140px;
    font-size: 13.5px;
  }
</style>
</head>
<body class="empty">
  <div class="app">
    <div class="main">
      <div class="main-top">
        <div id="messages"><div id="messagesSpacer" aria-hidden="true"></div></div>
        <div id="chips" class="chips context-chips"></div>
        <div id="subagentsDock" class="subagents-dock" hidden>
          <div class="subagents-head">
            <span>Subagents</span>
            <button type="button" class="ghost" id="hideSubsBtn">Hide</button>
          </div>
          <div id="subagentGrid" class="subagent-grid"></div>
        </div>
      </div>

      <div class="main-bottom">
        <div id="reviewBar" class="review-bar" hidden>
          <button type="button" class="files-toggle" id="reviewText">&gt; 0 Files</button>
          <div class="review-actions">
            <button id="revertAllBtn" type="button">Undo All</button>
            <button id="acceptAllBtn" type="button">Keep All</button>
            <button id="reviewBtn" type="button" class="review-primary">Review</button>
          </div>
        </div>

        <footer>
          <div id="activityBar" class="activity-bar" aria-live="polite">
            <span class="pulse-dot" aria-hidden="true"></span>
            <span id="activityLabel">Thinking</span>
          </div>
          <div id="dropOverlay" class="drop-overlay" hidden>Drop files or images</div>
          <div class="composer-shell" id="composerShell">
            <div class="composer-card">
              <div class="composer-wrap">
                <div id="mentionPopover" class="mention-popover" hidden></div>
                <textarea id="prompt" placeholder="Plan, Build, / for skills, @ for context" rows="2"></textarea>
              </div>
              <div class="composer-toolbar">
                <div class="composer-left">
                  <select id="mode" class="mode-select" title="Mode">
                    <option value="agent">∞ Agent</option>
                    <option value="plan">Plan</option>
                    <option value="ask">Ask</option>
                    <option value="take-control">Computer</option>
                  </select>
                  <select id="model" class="model-select" title="Model">
                    <option value="auto">Auto</option>
                    <option value="gemini-3.8-flash" selected>gemini-3.8-flash</option>
                    <option value="gemini-3.1-pro-preview">gemini-3.1-pro-preview</option>
                    <option value="gemini-3.5-flash-lite">gemini-3.5-flash-lite</option>
                    <option value="claude-sonnet-4-20250514">claude-sonnet-4</option>
                    <option value="claude-3-5-haiku-latest">claude-3.5-haiku</option>
                    <option value="gpt-4o-mini">gpt-4o-mini</option>
                    <option value="o3-mini">o3-mini</option>
                  </select>
                </div>
                <div class="composer-right">
                  <button id="addFileBtn" class="icon-btn" type="button" title="Attach (or drag &amp; drop)" aria-label="Attach">
                    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg>
                  </button>
                  <input type="file" id="imgFileInput" accept="image/*,*/*" multiple hidden />
                  <button id="sendBtn" class="icon-btn send-btn" type="button" title="Send" aria-label="Send">
                    <svg width="16" height="16" viewBox="0 0 24 24" fill="currentColor"><path d="M2.01 21L23 12 2.01 3 2 10l15 2-15 2z"/></svg>
                  </button>
                </div>
              </div>
            </div>
          </div>
          <button id="addImgBtn" type="button" hidden></button>
        </footer>
      </div>
    </div>

    <aside class="rail" id="historyRail">
      <button type="button" class="rail-item" id="newSessionBtn">
        <span class="rail-ico">↗</span><span>New Agent</span>
      </button>
      <button type="button" class="rail-item" id="searchSessionsBtn">
        <span class="rail-ico">⌕</span><span>Search</span>
      </button>
      <button type="button" class="rail-item" onclick="launchStudio('swarm')">
        <span class="rail-ico">◈</span><span>Automations</span>
      </button>
      <button type="button" class="rail-item" id="customizeBtn">
        <span class="rail-ico">▦</span><span>Customize</span>
      </button>
      <button type="button" class="rail-item rail-more" id="moreBtn">
        <span class="rail-ico">···</span><span>More</span>
      </button>

      <div id="moreMenu" class="more-menu" hidden>
        <button type="button" id="keyBtn">API Key</button>
        <button type="button" id="clearBtn">Clear chat</button>
        <button type="button" id="delSessionBtn">Delete session</button>
        <button type="button" id="spawnSubsBtn">Spawn subagents</button>
        <button type="button" onclick="launchStudio('browser')">Browser</button>
        <button type="button" onclick="launchStudio('artifacts')">Decks &amp; Video</button>
        <button type="button" onclick="launchStudio('canvas')">Canvas</button>
        <button type="button" onclick="launchStudio('addDocs')">+ @docs</button>
        <button type="button" onclick="launchStudio('mcpStore')">MCP+</button>
        <button type="button" id="previewBtn">Preview</button>
        <button type="button" id="reloadMcpBtn">Reload MCP</button>
        <button type="button" id="stopBtn">Stop</button>
        <div class="more-toggles">
          <label><input type="checkbox" id="wsCheck" checked /> @workspace</label>
          <label><input type="checkbox" id="gitCheck" /> @git</label>
          <label><input type="checkbox" id="autoApplyCheck" /> Auto-apply</label>
          <label><input type="checkbox" id="autoVerifyCheck" /> Auto-verify</label>
        </div>
        <div class="more-status">
          <span id="mcpPill" class="status-pill">MCP: 0</span>
          <span id="rulesPill" class="status-pill" hidden>Rules</span>
          <span id="modeBadge" class="mode-badge" data-mode="agent">Agent</span>
        </div>
      </div>

      <div class="rail-section-label">Today</div>
      <div id="sessionList" class="session-list"></div>
      <select id="sessionSelect" hidden></select>
      <input type="search" id="sessionSearch" class="session-search" placeholder="Search chats" hidden />
    </aside>
  </div>
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
  const modeBadgeEl = document.getElementById('modeBadge');
  const subagentsDockEl = document.getElementById('subagentsDock');
  const subagentGridEl = document.getElementById('subagentGrid');
  const spawnSubsBtn = document.getElementById('spawnSubsBtn');
  const hideSubsBtn = document.getElementById('hideSubsBtn');
  const moreBtn = document.getElementById('moreBtn');
  const moreMenu = document.getElementById('moreMenu');
  const sessionListEl = document.getElementById('sessionList');
  const sessionSearchEl = document.getElementById('sessionSearch');
  let cachedSessions = [];

  let activeAssistantEl = null;
  let activeAssistantRaw = '';
  let activeThinkingEl = null;
  let activeThoughtEl = null;
  let activeThoughtRaw = '';
  let isGenerating = false;
  let thoughtStartedAt = 0;
  const toolPills = new Map();
  const toolCards = new Map();
  const subagentCards = new Map();
  const sendBtnEl = document.getElementById('sendBtn');
  const activityLabelEl = document.getElementById('activityLabel');

  function setGenerating(on, label) {
    isGenerating = Boolean(on);
    document.body.classList.toggle('generating', isGenerating);
    if (sendBtnEl) {
      sendBtnEl.classList.toggle('stop-mode', isGenerating);
      sendBtnEl.title = isGenerating ? 'Stop' : 'Send';
      sendBtnEl.setAttribute('aria-label', isGenerating ? 'Stop' : 'Send');
    }
    if (activityLabelEl && label) {
      activityLabelEl.textContent = label;
    } else if (activityLabelEl && !isGenerating) {
      activityLabelEl.textContent = 'Thinking';
    }
  }

  function setActivity(label) {
    if (activityLabelEl) activityLabelEl.textContent = label || 'Working';
  }

  function clearThinkingStatus() {
    if (activeThinkingEl) {
      activeThinkingEl.remove();
      activeThinkingEl = null;
    }
  }

  function ensureThinkingStatus() {
    if (activeThinkingEl && activeThinkingEl.isConnected) return activeThinkingEl;
    const row = document.createElement('div');
    row.className = 'thinking-row';
    row.innerHTML =
      '<div class="thinking-status">' +
      '<span class="shimmer-label">Thinking</span>' +
      '<span class="thinking-dots" aria-hidden="true"><span></span><span></span><span></span></span>' +
      '</div>';
    messagesEl.appendChild(row);
    scrollMessagesToBottom(true);
    activeThinkingEl = row;
    syncEmptyState();
    return row;
  }

  function ensureThoughtBlock() {
    if (activeThoughtEl && activeThoughtEl.isConnected) return activeThoughtEl;
    clearThinkingStatus();
    const block = document.createElement('div');
    block.className = 'thought-block open';
    block.innerHTML =
      '<button type="button" class="thought-toggle">' +
      '<span class="chev">▶</span><span class="thought-label">Thought</span>' +
      '</button><div class="thought-body"></div>';
    const toggle = block.querySelector('.thought-toggle');
    toggle.addEventListener('click', () => {
      block.classList.toggle('open');
    });
    messagesEl.appendChild(block);
    scrollMessagesToBottom(true);
    activeThoughtEl = block;
    activeThoughtRaw = '';
    thoughtStartedAt = Date.now();
    syncEmptyState();
    return block;
  }

  function appendThought(text) {
    const block = ensureThoughtBlock();
    activeThoughtRaw += text;
    const body = block.querySelector('.thought-body');
    body.textContent = activeThoughtRaw;
    const secs = Math.max(1, Math.round((Date.now() - thoughtStartedAt) / 1000));
    block.querySelector('.thought-label').textContent = 'Thought for ' + secs + 's';
    setActivity('Thinking');
    scrollMessagesToBottom(true);
  }

  function finalizeThought() {
    if (!activeThoughtEl) return;
    const secs = Math.max(1, Math.round((Date.now() - (thoughtStartedAt || Date.now())) / 1000));
    const label = activeThoughtEl.querySelector('.thought-label');
    if (label) label.textContent = 'Thought for ' + secs + 's';
    activeThoughtEl.classList.remove('open');
    activeThoughtEl = null;
    activeThoughtRaw = '';
    thoughtStartedAt = 0;
  }

  function ensureAssistantBubble() {
    if (activeAssistantEl && activeAssistantEl.isConnected) return activeAssistantEl;
    activeAssistantEl = addMessage('assistant', '');
    activeAssistantRaw = '';
    activeAssistantEl.classList.add('streaming');
    return activeAssistantEl;
  }

  function renderAssistantStream() {
    const el = ensureAssistantBubble();
    el.classList.add('streaming');
    el.innerHTML =
      '<div class="msg-body">' +
      renderMarkdown(activeAssistantRaw) +
      '<span class="stream-caret" aria-hidden="true"></span></div>';
    scrollMessagesToBottom(false);
  }

  function finishAssistantStream() {
    if (!activeAssistantEl) return;
    activeAssistantEl.classList.remove('streaming');
    if (!activeAssistantRaw.trim()) {
      activeAssistantEl.remove();
    } else {
      activeAssistantEl.innerHTML = '<div class="msg-body">' + renderMarkdown(activeAssistantRaw) + '</div>';
    }
    activeAssistantEl = null;
    activeAssistantRaw = '';
  }

  function beginTurnUi() {
    setGenerating(true, 'Thinking');
    clearThinkingStatus();
    finalizeThought();
    finishAssistantStream();
    ensureThinkingStatus();
  }

  function endTurnUi() {
    clearThinkingStatus();
    finalizeThought();
    finishAssistantStream();
    setGenerating(false);
  }

  const MODE_META = {
    agent: { label: 'Agent', placeholder: 'Plan, Build, / for skills, @ for context.' },
    plan: { label: 'Plan', placeholder: 'Describe the goal — Plan mode explores read-only and drafts steps.' },
    ask: { label: 'Ask', placeholder: 'Ask anything about the codebase (read-only).' },
    'take-control': { label: 'Computer', placeholder: 'Tell Computer mode what to do in the browser / terminal.' },
  };

  function syncModeUi() {
    const mode = modeEl.value || 'agent';
    const meta = MODE_META[mode] || MODE_META.agent;
    modeBadgeEl.dataset.mode = mode;
    modeBadgeEl.textContent = meta.label;
    promptEl.placeholder = meta.placeholder;
  }

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
    const NL = String.fromCharCode(10);
    const src = String(md || '').replace(/\\r\\n/g, NL).replace(/\\r/g, NL);
    // Regex literal so \\n / \\s survive the outer template → webview correctly.
    const fenceRe = /\`\`\`([a-zA-Z0-9_-]*)\\n([\\s\\S]*?)\`\`\`/g;
    const segments = [];
    let last = 0;
    let m;
    while ((m = fenceRe.exec(src)) !== null) {
      if (m.index > last) {
        segments.push({ type: 'text', value: src.slice(last, m.index) });
      }
      segments.push({ type: 'code', lang: m[1] || 'code', value: m[2] });
      last = fenceRe.lastIndex;
    }
    if (last < src.length) {
      segments.push({ type: 'text', value: src.slice(last) });
    }
    if (segments.length === 0) {
      segments.push({ type: 'text', value: '' });
    }

    function renderInline(text) {
      let html = escHtml(text);
      html = html.replace(/\\*\\*([^*]+)\\*\\*/g, '<strong>$1</strong>');
      html = html.replace(/(^|[^*])\\*([^*]+)\\*(?!\\*)/g, '$1<em>$2</em>');
      html = html.replace(/\`([^ \`]+)\`/g, '<code>$1</code>');
      return html;
    }

    function renderTextBlock(text) {
      const normalized = String(text || '').split(NL + NL + NL).join(NL + NL).trim();
      if (!normalized) return '';
      const blocks = normalized.split(NL + NL);
      return blocks.map((block) => {
        const lines = block.split(NL);
        if (lines.length === 1 && /^###\\s+/.test(lines[0])) {
          return '<h3>' + renderInline(lines[0].replace(/^###\\s+/, '')) + '</h3>';
        }
        if (lines.length === 1 && /^##\\s+/.test(lines[0])) {
          return '<h2>' + renderInline(lines[0].replace(/^##\\s+/, '')) + '</h2>';
        }
        if (lines.length === 1 && /^#\\s+/.test(lines[0])) {
          return '<h1>' + renderInline(lines[0].replace(/^#\\s+/, '')) + '</h1>';
        }
        const bulletLines = lines.filter((ln) => /^\\s*[-*]\\s+/.test(ln));
        if (bulletLines.length === lines.length && lines.length > 0) {
          return '<ul>' + lines.map((ln) =>
            '<li>' + renderInline(ln.replace(/^\\s*[-*]\\s+/, '')) + '</li>'
          ).join('') + '</ul>';
        }
        const numbered = lines.filter((ln) => /^\\s*\\d+\\.\\s+/.test(ln));
        if (numbered.length === lines.length && lines.length > 0) {
          return '<ol>' + lines.map((ln) =>
            '<li>' + renderInline(ln.replace(/^\\s*\\d+\\.\\s+/, '')) + '</li>'
          ).join('') + '</ol>';
        }
        return '<p>' + lines.map(renderInline).join('<br/>') + '</p>';
      }).join('');
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
      return renderTextBlock(seg.value);
    }).join('');
  }

  function scrollMessagesToBottom(force) {
    const nearBottom =
      messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < 120;
    if (force || nearBottom) {
      scrollMessagesToBottom(true);
    }
  }

  function ensureMessagesSpacer() {
    let spacer = document.getElementById('messagesSpacer');
    if (!spacer) {
      spacer = document.createElement('div');
      spacer.id = 'messagesSpacer';
      spacer.setAttribute('aria-hidden', 'true');
      messagesEl.insertBefore(spacer, messagesEl.firstChild);
    }
    return spacer;
  }

  function resetMessages() {
    messagesEl.innerHTML = '';
    ensureMessagesSpacer();
    syncEmptyState();
  }

  window.copyCode = (enc) => navigator.clipboard?.writeText(decodeURIComponent(enc));
  window.insertCode = (enc) => vscode.postMessage({ type: 'insertCode', code: decodeURIComponent(enc) });

  function syncEmptyState() {
    const hasMsgs = messagesEl.children.length > 0;
    document.body.classList.toggle('empty', !hasMsgs);
  }

  function formatAgo(ts) {
    const n = Number(ts) || 0;
    if (!n) return '';
    const mins = Math.max(0, Math.round((Date.now() - n) / 60000));
    if (mins < 1) return 'now';
    if (mins < 60) return mins + 'm';
    const hrs = Math.round(mins / 60);
    if (hrs < 48) return hrs + 'h';
    return Math.round(hrs / 24) + 'd';
  }

  function countUserMessagesBefore(el) {
    let n = 0;
    for (const child of messagesEl.children) {
      if (child === el) break;
      if (child.classList && child.classList.contains('user')) n += 1;
    }
    return n;
  }

  function cancelActiveUserEdit() {
    const editing = messagesEl.querySelector('.user.editing');
    if (!editing) return;
    const form = editing.querySelector('.user-edit-form');
    if (form) form.remove();
    editing.classList.remove('editing');
  }

  function startEditUserMessage(div) {
    if (isGenerating) {
      vscode.postMessage({ type: 'stop' });
      setActivity('Stopping…');
    }
    cancelActiveUserEdit();
    const textEl = div.querySelector('.msg-text');
    const original = textEl ? textEl.textContent : '';
    div.classList.add('editing');
    const form = document.createElement('div');
    form.className = 'user-edit-form';
    form.innerHTML =
      '<textarea class="user-edit-box"></textarea>' +
      '<div class="user-edit-actions">' +
      '<button type="button" class="cancel">Cancel</button>' +
      '<button type="button" class="primary save">Save &amp; Submit</button>' +
      '</div>';
    const box = form.querySelector('.user-edit-box');
    box.value = original;
    form.querySelector('.cancel').onclick = () => {
      form.remove();
      div.classList.remove('editing');
    };
    form.querySelector('.save').onclick = () => {
      const next = String(box.value || '').trim();
      if (!next) return;
      const userIndex = countUserMessagesBefore(div);
      if (textEl) textEl.textContent = next;
      form.remove();
      div.classList.remove('editing');
      // Drop everything after this user turn in the UI.
      let node = div.nextSibling;
      while (node) {
        const nextNode = node.nextSibling;
        node.remove();
        node = nextNode;
      }
      toolPills.clear();
      toolCards.clear();
      subagentCards.clear();
      if (subagentGridEl) subagentGridEl.innerHTML = '';
      if (subagentsDockEl) subagentsDockEl.hidden = true;
      activeAssistantEl = null;
      activeAssistantRaw = '';
      activeThinkingEl = null;
      activeThoughtEl = null;
      activeThoughtRaw = '';
      thoughtStartedAt = 0;
      beginTurnUi();
      vscode.postMessage({
        type: 'editResubmit',
        userIndex,
        text: next,
        mode: modeEl.value,
        model: modelEl.value,
        includeWorkspace: Boolean(wsEl.checked),
        includeGitDiff: Boolean(gitEl.checked),
        autoApply: autoApplyEl.checked,
        autoVerify: autoVerifyEl.checked,
      });
    };
    div.appendChild(form);
    box.focus();
    box.setSelectionRange(box.value.length, box.value.length);
  }

  function addMessage(cls, text, images) {
    const div = document.createElement('div');
    div.className = 'msg ' + cls;
    if (cls === 'assistant') {
      div.innerHTML = renderMarkdown(text);
    } else if (cls === 'user') {
      if (Array.isArray(images) && images.length > 0) {
        const imgsDiv = document.createElement('div');
        imgsDiv.className = 'msg-images';
        for (const img of images) {
          if (typeof img.mimeType === 'string' && img.mimeType.startsWith('audio/')) {
            const badge = document.createElement('span');
            badge.className = 'chip';
            badge.textContent = '🎤 ' + (img.name || 'Voice Recording');
            imgsDiv.appendChild(badge);
          } else {
            const el = document.createElement('img');
            el.src = 'data:' + img.mimeType + ';base64,' + img.data;
            el.alt = img.name || 'image';
            el.title = img.name || 'image';
            imgsDiv.appendChild(el);
          }
        }
        div.appendChild(imgsDiv);
      }
      const span = document.createElement('span');
      span.className = 'msg-text';
      span.textContent = text || '';
      div.appendChild(span);
      const editBtn = document.createElement('button');
      editBtn.type = 'button';
      editBtn.className = 'user-edit-btn';
      editBtn.title = 'Edit message';
      editBtn.setAttribute('aria-label', 'Edit message');
      editBtn.innerHTML =
        '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">' +
        '<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>';
      editBtn.onclick = (e) => {
        e.preventDefault();
        e.stopPropagation();
        startEditUserMessage(div);
      };
      div.appendChild(editBtn);
    } else {
      if (text) {
        const span = document.createElement('span');
        span.textContent = text;
        div.appendChild(span);
      }
    }
    messagesEl.appendChild(div);
    scrollMessagesToBottom(true);
    syncEmptyState();
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
    chipsEl.innerHTML = '';

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
      { kind: 'docs', label: '@docs', detail: 'Search indexed documentation sites' },
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
    } else if (item.kind === 'docs') {
      inserted = '@docs ';
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
    const slashMatch = beforeCursor.match(new RegExp('^/([a-zA-Z0-9_-]*)$'));
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
    if (isGenerating) {
      vscode.postMessage({ type: 'stop' });
      setActivity('Stopping…');
      return;
    }
    const text = promptEl.value.trim();
    const imagesToSend = pendingImages.slice();
    if (!text && imagesToSend.length === 0) return;

    const includeGitDiff = Boolean(gitEl.checked) || /(?:^|\\s)@git(?:\\s|$)/.test(text);
    const includeWorkspace = Boolean(wsEl.checked) || /(?:^|\\s)@workspace(?:\\s|$)/.test(text);

    addMessage('user', text, imagesToSend);
    beginTurnUi();
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

  function upsertSubagentCard(ev) {
    const id = ev.taskId || ('task_' + Date.now());
    let card = subagentCards.get(id);
    if (!card) {
      card = document.createElement('div');
      card.className = 'subagent-card';
      card.innerHTML =
        '<div class="top"><span class="id"></span><span class="status"></span></div>' +
        '<div class="role"></div><div class="out" hidden></div>';
      subagentGridEl.appendChild(card);
      subagentCards.set(id, card);
    }
    subagentsDockEl.hidden = false;
    card.querySelector('.id').textContent = id;
    card.querySelector('.role').textContent = ev.role ? ('role: ' + ev.role) : '';
    const statusEl = card.querySelector('.status');
    const status = String(ev.status || '');
    statusEl.textContent = status;
    statusEl.className = 'status' +
      (status === 'started' || status === 'running' ? ' running' :
       status === 'completed' ? ' ok' :
       status.indexOf('fail') >= 0 ? ' fail' : '');
    if (ev.output) {
      const outEl = card.querySelector('.out');
      outEl.hidden = false;
      outEl.textContent = String(ev.output).slice(0, 1200);
    }
    scrollMessagesToBottom(true);
  }

  sendBtnEl.onclick = send;
  document.getElementById('stopBtn').onclick = () => {
    vscode.postMessage({ type: 'stop' });
    setActivity('Stopping…');
  };
  document.getElementById('clearBtn').onclick = () => {
    resetMessages();
    toolPills.clear();
    toolCards.clear();
    subagentCards.clear();
    subagentGridEl.innerHTML = '';
    subagentsDockEl.hidden = true;
    activeAssistantEl = null;
    activeAssistantRaw = '';
    activeThinkingEl = null;
    activeThoughtEl = null;
    activeThoughtRaw = '';
    thoughtStartedAt = 0;
    setGenerating(false);
    syncEmptyState();
    vscode.postMessage({ type: 'clear' });
  };
  spawnSubsBtn.onclick = () => {
    const text = promptEl.value.trim();
    subagentsDockEl.hidden = false;
    moreMenu.hidden = true;
    vscode.postMessage({
      type: 'spawnSubagents',
      text,
      model: modelEl.value,
    });
  };
  hideSubsBtn.onclick = () => { subagentsDockEl.hidden = true; };
  moreBtn.onclick = (e) => {
    e.stopPropagation();
    moreMenu.hidden = !moreMenu.hidden;
  };
  document.addEventListener('click', (e) => {
    if (!moreMenu.hidden && !moreMenu.contains(e.target) && e.target !== moreBtn) {
      moreMenu.hidden = true;
    }
  });
  modeEl.onchange = () => {
    syncModeUi();
    vscode.postMessage({ type: 'modeChanged', mode: modeEl.value });
  };
  syncModeUi();
  document.getElementById('newSessionBtn').onclick = () => vscode.postMessage({ type: 'newSession' });
  document.getElementById('delSessionBtn').onclick = () => vscode.postMessage({ type: 'deleteSession' });
  document.getElementById('customizeBtn').onclick = () => vscode.postMessage({ type: 'openSettings' });
  document.getElementById('searchSessionsBtn').onclick = () => {
    sessionSearchEl.hidden = !sessionSearchEl.hidden;
    if (!sessionSearchEl.hidden) sessionSearchEl.focus();
  };
  sessionSearchEl.addEventListener('input', () => {
    renderSessions(cachedSessions, sessionSelectEl.value);
  });
  sessionSelectEl.onchange = () => {
    if (sessionSelectEl.value) {
      vscode.postMessage({ type: 'switchSession', sessionId: sessionSelectEl.value });
    }
  };
  document.getElementById('keyBtn').onclick = () => vscode.postMessage({ type: 'setKey' });
  document.getElementById('addFileBtn').onclick = () => imgFileInput.click();
  document.getElementById('addImgBtn').onclick = () => imgFileInput.click();
  document.getElementById('previewBtn').onclick = () => vscode.postMessage({ type: 'openPreview' });
  imgFileInput.onchange = () => {
    for (const file of imgFileInput.files || []) {
      if (file.type && file.type.startsWith('image/')) {
        readImageFile(file);
      } else if (file.name) {
        // Non-image: ask host to attach by relative path when possible via pick dialog fallback
        vscode.postMessage({ type: 'pickFile' });
      }
    }
    imgFileInput.value = '';
  };
  document.getElementById('acceptAllBtn').onclick = () => vscode.postMessage({ type: 'acceptAllEdits' });
  document.getElementById('revertAllBtn').onclick = () => vscode.postMessage({ type: 'revertAllEdits' });
  const reviewBtn = document.getElementById('reviewBtn');
  if (reviewBtn) {
    reviewBtn.onclick = () => vscode.postMessage({ type: 'acceptAllEdits' });
  }

  // Drag & drop onto composer (Cursor-style)
  const composerShell = document.getElementById('composerShell');
  const dropOverlay = document.getElementById('dropOverlay');
  let dragDepth = 0;
  function isFileDrag(e) {
    return e.dataTransfer && Array.from(e.dataTransfer.types || []).includes('Files');
  }
  ['dragenter', 'dragover'].forEach((evt) => {
    composerShell.addEventListener(evt, (e) => {
      if (!isFileDrag(e)) return;
      e.preventDefault();
      e.stopPropagation();
      composerShell.classList.add('drag-over');
      dropOverlay.hidden = false;
    });
  });
  composerShell.addEventListener('dragenter', () => { dragDepth += 1; });
  composerShell.addEventListener('dragleave', () => {
    dragDepth = Math.max(0, dragDepth - 1);
    if (dragDepth === 0) {
      composerShell.classList.remove('drag-over');
      dropOverlay.hidden = true;
    }
  });
  composerShell.addEventListener('drop', (e) => {
    e.preventDefault();
    e.stopPropagation();
    dragDepth = 0;
    composerShell.classList.remove('drag-over');
    dropOverlay.hidden = true;
    const files = Array.from((e.dataTransfer && e.dataTransfer.files) || []);
    for (const file of files) {
      if (file.type && file.type.startsWith('image/')) {
        readImageFile(file);
      } else {
        vscode.postMessage({ type: 'pickFile' });
        break;
      }
    }
  });
  document.addEventListener('paste', (e) => {
    const items = Array.from((e.clipboardData && e.clipboardData.items) || []);
    for (const item of items) {
      if (item.type && item.type.startsWith('image/')) {
        const file = item.getAsFile();
        if (file) readImageFile(file);
      }
    }
  });
  reloadMcpBtn.onclick = () => {
    reloadMcpBtn.disabled = true;
    reloadMcpBtn.textContent = 'Reloading…';
    vscode.postMessage({ type: 'reloadMcp' });
  };
  modelEl.onchange = () => vscode.postMessage({ type: 'modelChanged', model: modelEl.value });
  // modeEl.onchange already bound above for badge/placeholder sync

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
    scrollMessagesToBottom(true);
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
    cachedSessions = list;
    const q = (sessionSearchEl.value || '').trim().toLowerCase();
    const filtered = q
      ? list.filter((s) => String(s.title || s.sessionId || '').toLowerCase().indexOf(q) >= 0)
      : list;

    sessionSelectEl.innerHTML = '';
    const hasCurrent = list.some((s) => s.sessionId === currentId);
    if (!hasCurrent && currentId) {
      const opt = document.createElement('option');
      opt.value = currentId;
      opt.textContent = 'New Agent';
      sessionSelectEl.appendChild(opt);
    }
    for (const s of list) {
      const opt = document.createElement('option');
      opt.value = s.sessionId;
      opt.textContent = s.title || s.sessionId;
      sessionSelectEl.appendChild(opt);
    }
    if (currentId) {
      sessionSelectEl.value = currentId;
    }

    sessionListEl.innerHTML = '';
    const rows = filtered.length > 0
      ? filtered
      : (currentId ? [{ sessionId: currentId, title: 'New Agent', updatedAt: Date.now() }] : []);
    for (const s of rows) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'session-row' + (s.sessionId === currentId ? ' active' : '');
      btn.innerHTML =
        '<span class="title">' + escHtml(s.title || 'New Agent') + '</span>' +
        '<span class="ago">' + escHtml(formatAgo(s.updatedAt)) + '</span>';
      btn.onclick = () => {
        vscode.postMessage({ type: 'switchSession', sessionId: s.sessionId });
      };
      sessionListEl.appendChild(btn);
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
  window.launchStudio = (studioTarget) => vscode.postMessage({ type: 'launchStudio', studioTarget });
  window.openStudioArtifact = (encPath, external) =>
    vscode.postMessage({
      type: 'openStudioArtifact',
      artifactPath: decodeURIComponent(encPath),
      external: Boolean(external),
    });

  function renderStudioArtifactCard(art) {
    if (!art) return;
    activeAssistantEl = null;
    activeAssistantRaw = '';
    if (art.kind === 'gen_ui' && art.iframeHtml) {
      const wrap = document.createElement('div');
      wrap.className = 'studio-genui-wrap';
      wrap.innerHTML = art.iframeHtml;
      messagesEl.appendChild(wrap);
      scrollMessagesToBottom(true);
      return;
    }

    const card = document.createElement('div');
    card.className = 'studio-card';
    const kindLabel = String(art.kind || 'artifact').toUpperCase();
    const primaryPath =
      art.htmlDeckPath ||
      art.pptxPath ||
      art.videoPath ||
      art.imagePath ||
      art.xlsxPath ||
      art.docxPath ||
      '';
    const exportPath =
      art.pptxPath ||
      art.videoPath ||
      art.xlsxPath ||
      art.docxPath ||
      art.imagePath ||
      primaryPath;
    const titleText =
      art.title ||
      (primaryPath ? primaryPath.split('/').pop().split('\\\\').pop() : 'Studio Artifact');

    let metaParts = [];
    if (art.slideCount) metaParts.push(art.slideCount + ' slides');
    if (art.durationSecs) metaParts.push(art.durationSecs + 's');
    if (art.sheetCount) metaParts.push(art.sheetCount + ' sheet(s)');
    if (art.sectionCount) metaParts.push(art.sectionCount + ' section(s)');
    if (exportPath) metaParts.push(exportPath);

    let buttonsHtml = '';
    if (primaryPath) {
      const encPrimary = encodeURIComponent(primaryPath);
      buttonsHtml +=
        '<button class="primary" onclick="openStudioArtifact(\\'' +
        encPrimary +
        '\\', false)">Preview in Rursor</button>';
    }
    if (exportPath) {
      const encExport = encodeURIComponent(exportPath);
      buttonsHtml +=
        '<button onclick="openStudioArtifact(\\'' +
        encExport +
        '\\', true)">Open Externally</button>';
    }

    card.innerHTML =
      '<div class="studio-card-head"><span>' +
      escHtml(titleText) +
      '</span><span class="studio-badge">' +
      escHtml(kindLabel) +
      '</span></div>' +
      (metaParts.length > 0
        ? '<div class="studio-card-meta">' + escHtml(metaParts.join(' · ')) + '</div>'
        : '') +
      (buttonsHtml ? '<div class="studio-card-actions">' + buttonsHtml + '</div>' : '');

    messagesEl.appendChild(card);
    scrollMessagesToBottom(true);
  }

  window.addEventListener('message', (event) => {
    const msg = event.data;
    if (msg.type === 'turnStarted') {
      if (!isGenerating) beginTurnUi();
      else {
        setGenerating(true, 'Thinking');
        ensureThinkingStatus();
      }
    } else if (msg.type === 'thinking') {
      setGenerating(true, 'Thinking');
      appendThought(msg.text || '');
    } else if (msg.type === 'delta') {
      clearThinkingStatus();
      if (activeThoughtEl) {
        // Keep thought visible but collapse once answer streams.
        const secs = Math.max(1, Math.round((Date.now() - (thoughtStartedAt || Date.now())) / 1000));
        const label = activeThoughtEl.querySelector('.thought-label');
        if (label) label.textContent = 'Thought for ' + secs + 's';
        activeThoughtEl.classList.remove('open');
        activeThoughtEl = null;
        activeThoughtRaw = '';
        thoughtStartedAt = 0;
      }
      setActivity('Streaming');
      activeAssistantRaw += msg.text || '';
      renderAssistantStream();
    } else if (msg.type === 'externalUserMessage') {
      addMessage('user', msg.text || '', msg.images || []);
      beginTurnUi();
    } else if (msg.type === 'done') {
      endTurnUi();
    } else if (msg.type === 'error') {
      endTurnUi();
      addMessage('err', msg.text);
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
      scrollMessagesToBottom(true);
    } else if (msg.type === 'checkpointRestored') {
      const pill = document.createElement('div');
      pill.className = 'tool-pill';
      pill.textContent = 'Restored checkpoint ' + (msg.checkpointId || '') + ' (' + (msg.restoredCount || 0) + ' change(s)) ✓';
      messagesEl.appendChild(pill);
      scrollMessagesToBottom(true);
    } else if (msg.type === 'subagentEvent' && msg.payload) {
      upsertSubagentCard(msg.payload);
      const status = String(msg.payload.status || '');
      const running = status === 'started' || status === 'running';
      const pill = document.createElement('div');
      pill.className = 'tool-pill' + (running ? ' running' : status === 'completed' ? ' ok' : status.indexOf('fail') >= 0 ? ' fail' : '');
      pill.innerHTML =
        (running ? '<span class="spin" aria-hidden="true"></span>' : '') +
        '<span class="tool-label">Subagent [' + escHtml(msg.payload.taskId || 'task') + ']' +
        (msg.payload.role ? ' · ' + escHtml(msg.payload.role) : '') +
        ': ' + escHtml(status) + '</span>';
      messagesEl.appendChild(pill);
      setActivity(running ? 'Subagents running' : 'Subagent ' + status);
      scrollMessagesToBottom(true);
    } else if (msg.type === 'subagentBatchStarted') {
      subagentsDockEl.hidden = false;
      const pill = document.createElement('div');
      pill.className = 'tool-pill running';
      pill.innerHTML =
        '<span class="spin" aria-hidden="true"></span>' +
        '<span class="tool-label">Spawning ' + (msg.count || 0) + ' subagent(s)…</span>';
      messagesEl.appendChild(pill);
      setActivity('Spawning subagents');
    } else if (msg.type === 'subagentBatchDone') {
      const results = Array.isArray(msg.results) ? msg.results : [];
      for (const r of results) {
        upsertSubagentCard({
          taskId: r.id,
          role: r.role,
          status: r.ok ? 'completed' : 'failed',
          output: r.output,
        });
      }
      const pill = document.createElement('div');
      pill.className = 'tool-pill';
      pill.textContent = 'Subagents finished (' + results.length + ')';
      messagesEl.appendChild(pill);
      scrollMessagesToBottom(true);
    } else if (msg.type === 'swarmEvent' && msg.payload) {
      const ev = msg.payload;
      if (ev.kind === 'wave_started' || ev.kind === 'worker_done' || ev.kind === 'mission_done') {
        const pill = document.createElement('div');
        const running = ev.kind === 'wave_started';
        pill.className = 'tool-pill' + (running ? ' running' : ev.kind === 'mission_done' ? ' ok' : '');
        const detail =
          ev.kind === 'wave_started'
            ? 'Swarm Wave ' + ((ev.waveIndex || 0) + 1) + ' started'
            : ev.kind === 'worker_done'
              ? 'Worker [' + (ev.workerId || 'worker') + '] ' + (ev.status || 'done')
              : 'Swarm mission completed';
        pill.innerHTML =
          (running ? '<span class="spin" aria-hidden="true"></span>' : '') +
          '<span class="tool-label">' + escHtml(detail) + '</span>';
        messagesEl.appendChild(pill);
        setActivity(detail);
        scrollMessagesToBottom(true);
      }
    } else if (msg.type === 'artifactCreated' && msg.artifact) {
      setActivity('Artifact ready');
      renderStudioArtifactCard(msg.artifact);
    } else if (msg.type === 'sessionsSync') {
      renderSessions(msg.sessions, msg.currentSessionId);
    } else if (msg.type === 'sessionLoaded') {
      resetMessages();
      toolPills.clear();
      toolCards.clear();
      subagentCards.clear();
      subagentGridEl.innerHTML = '';
      subagentsDockEl.hidden = true;
      activeAssistantEl = null;
      activeAssistantRaw = '';
      activeThinkingEl = null;
      activeThoughtEl = null;
      activeThoughtRaw = '';
      thoughtStartedAt = 0;
      setGenerating(false);
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
          opt.textContent = 'New Agent';
          sessionSelectEl.insertBefore(opt, sessionSelectEl.firstChild);
        }
        sessionSelectEl.value = msg.sessionId;
      }
      syncEmptyState();
    } else if (msg.type === 'reviewSync') {
      const count = msg.fileCount || 0;
      if (count > 0) {
        reviewTextEl.textContent = '> ' + count + ' File' + (count === 1 ? '' : 's');
        reviewBarEl.hidden = false;
      } else {
        reviewBarEl.hidden = true;
      }
    } else if (msg.type === 'statusSync') {
      renderStatus(msg);
    } else if (msg.type === 'toolEvent') {
      const ev = msg.payload;
      if (ev.phase === 'start') {
        clearThinkingStatus();
        if (activeAssistantEl) {
          activeAssistantEl.classList.remove('streaming');
          activeAssistantEl.innerHTML =
            '<div class="msg-body">' + renderMarkdown(activeAssistantRaw) + '</div>';
          activeAssistantEl = null;
          activeAssistantRaw = '';
        }
        const argHint = ev.args?.path || ev.args?.query || ev.args?.prefix || ev.args?.command || '';
        const pill = document.createElement('div');
        pill.className = 'tool-pill running';
        pill.innerHTML =
          '<span class="spin" aria-hidden="true"></span>' +
          '<span class="tool-label">Running ' + escHtml(ev.name) +
          (argHint ? ': ' + escHtml(String(argHint)) : '') + '</span>';
        messagesEl.appendChild(pill);
        toolPills.set(ev.callId, pill);
        setActivity('Running ' + (ev.name || 'tool'));
        scrollMessagesToBottom(true);
      } else if (ev.phase === 'done') {
        const pill = toolPills.get(ev.callId);
        if (pill) {
          pill.classList.remove('running');
          pill.classList.add(ev.ok ? 'ok' : 'fail');
          const label = pill.querySelector('.tool-label');
          const base = label ? label.textContent : pill.textContent;
          pill.innerHTML =
            '<span class="tool-label">' +
            escHtml(String(base || ev.name || 'tool').replace(/…$/, '')) +
            (ev.ok ? ' ✓' : ' ✗') +
            '</span>';
        }
        setActivity(ev.ok ? 'Continuing' : 'Tool failed — retrying');
        ensureThinkingStatus();
      }
    } else if (msg.type === 'toolCard') {
      clearThinkingStatus();
      if (activeAssistantEl) {
        activeAssistantEl.classList.remove('streaming');
        activeAssistantEl.innerHTML =
          '<div class="msg-body">' + renderMarkdown(activeAssistantRaw) + '</div>';
        activeAssistantEl = null;
        activeAssistantRaw = '';
      }
      setActivity('Reviewing edit');
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
        scrollMessagesToBottom(true);
      }
    } else if (msg.type === 'toolCardUpdate') {
      const entry = toolCards.get(msg.callId);
      if (entry) {
        Object.assign(entry.card, msg);
        updateToolCardElement(entry.el, entry.card);
      }
    }
  });

  ${INLINE_GENUI_HOST_RESIZE_SCRIPT}

  ensureMessagesSpacer();
  syncEmptyState();
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
