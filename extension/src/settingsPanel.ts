import * as crypto from 'node:crypto';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const SECRET_GEMINI_KEY = 'rursor.geminiApiKey';
const SECRET_ANTHROPIC_KEY = 'rursor.anthropicApiKey';
const SECRET_OPENAI_KEY = 'rursor.openaiApiKey';

export type ProviderId = 'gemini' | 'anthropic' | 'openai';
export type PermissionProfile = 'safe-auto' | 'read-only' | 'full-auto';
export type AutocompleteBackend = 'gemini' | 'auto' | 'ollama' | 'mlx';

interface KeyStatusInfo {
  hasKeychainKey: boolean;
  hasSidecarKey: boolean;
  maskedPreview: string;
}

interface McpServerStatusItem {
  name: string;
  connected: boolean;
  toolCount: number;
  commandSummary?: string;
  error?: string;
}

interface SkillFileItem {
  name: string;
  description: string;
  source: string;
  fileName?: string;
  absPath?: string;
}

interface MemoryItem {
  id: string;
  scope: string;
  category: string;
  content: string;
  createdAt: number;
}

interface WorktreeItem {
  path: string;
  branch?: string;
  isMain: boolean;
}

interface CheckpointItem {
  checkpointId: string;
  label: string;
  fileCount: number;
}

interface SidecarStatusResult {
  ready?: boolean;
  hasApiKey?: boolean;
  hasAnthropicKey?: boolean;
  hasOpenAiKey?: boolean;
  model?: string;
  autocompleteModel?: string;
  autocompleteBackend?: string;
  thinkingBudget?: number | null;
  autoVerify?: boolean;
  indexedFiles?: number;
  hasWorkspaceRules?: boolean;
  mcpServers?: Array<{
    name: string;
    connected: boolean;
    toolCount: number;
    error?: string;
  }>;
  skillsCount?: number;
  memoriesCount?: number;
  backgroundProcesses?: number;
  workspaceRoot?: string | null;
}

interface LayoutState {
  windowLayout: 'agent' | 'editor';
  conversationDensity: 'compact' | 'detailed' | 'verbose';
  showTitleBar: boolean;
  showStatusBar: boolean;
  reviewControlLocation: 'breadcrumb' | 'floating';
  autoHideEditorWhenEmpty: boolean;
  openChatAsEditorTabs: boolean;
  customizeBannerDismissed: boolean;
}

interface DashboardStatePayload {
  workspaceRoot: string | null;
  sidecarReady: boolean;
  accountEmail: string;
  accountPlan: string;
  keys: Record<ProviderId, KeyStatusInfo>;
  models: {
    defaultModel: string;
    thinkingBudget: number;
    permissionProfile: PermissionProfile;
    promptCache: boolean;
    autoVerify: boolean;
  };
  layout: LayoutState;
  tab: {
    enabled: boolean;
    nesEnabled: boolean;
    backend: AutocompleteBackend;
    autocompleteModel: string;
    localEndpoint: string;
    localModel: string;
  };
  rulesAndMemory: {
    hasWorkspaceRules: boolean;
    rursorRulesContent: string;
    rursorRulesPath: string | null;
    workspaceSkills: SkillFileItem[];
    allSkillsCount: number;
    memories: MemoryItem[];
    rawMemoryJson: string;
    memoryJsonPath: string | null;
  };
  mcp: {
    mcpJsonPath: string | null;
    servers: McpServerStatusItem[];
  };
  indexAndSandbox: {
    indexedFiles: number;
    skillsCount: number;
    memoriesCount: number;
    backgroundProcesses: number;
    usePty: boolean;
    worktrees: WorktreeItem[];
    checkpoints: CheckpointItem[];
  };
}

interface WebviewIncomingMessage {
  type: string;
  provider?: ProviderId;
  value?: string;
  key?: string;
  boolValue?: boolean;
  numValue?: number;
  defaultModel?: string;
  thinkingBudget?: number;
  permissionProfile?: PermissionProfile;
  enabled?: boolean;
  nesEnabled?: boolean;
  backend?: AutocompleteBackend;
  autocompleteModel?: string;
  localEndpoint?: string;
  localModel?: string;
  rulesContent?: string;
  skillName?: string;
  filePath?: string;
  memoryScope?: string;
  memoryCategory?: string;
  memoryContent?: string;
  memoryId?: string;
  command?: string;
  url?: string;
}

const STARTER_RULES_TEMPLATE = `# Rursor Workspace Engineering Rules

- Preserve strict type safety and idiomatic error handling across all edits.
- Prefer minimal, surgical changes that respect existing module boundaries.
- Add or update focused unit tests alongside behavioral changes.
- Never hardcode secrets, tokens, or environment-specific absolute paths.
`;

const STARTER_MCP_JSON = `{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-filesystem",
        "."
      ],
      "env": {}
    }
  }
}
`;

function maskSecret(raw: string | undefined): string {
  const trimmed = raw?.trim() ?? '';
  if (!trimmed) {
    return '';
  }
  if (trimmed.length <= 8) {
    return '••••••••';
  }
  return `${trimmed.slice(0, 4)}••••••••${trimmed.slice(-4)}`;
}

function sanitizeSkillSlug(input: string): string {
  const cleaned = input
    .trim()
    .toLowerCase()
    .replace(/^\/+/, '')
    .replace(/\.md$/i, '')
    .replace(/[^a-z0-9_-]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return cleaned || 'custom-skill';
}

function parseSkillFrontmatterSummary(
  fileName: string,
  raw: string
): { name: string; description: string } {
  const fallbackName = fileName.replace(/\.md$/i, '');
  const normalized = raw.replace(/^\uFEFF/, '').trim();
  if (normalized.startsWith('---')) {
    const endIdx = normalized.indexOf('\n---', 3);
    if (endIdx !== -1) {
      const header = normalized.slice(3, endIdx);
      const body = normalized.slice(endIdx + 4).trim();
      let parsedName = '';
      let parsedDesc = '';
      for (const line of header.split(/\r?\n/)) {
        const sep = line.indexOf(':');
        if (sep === -1) {
          continue;
        }
        const k = line.slice(0, sep).trim().toLowerCase();
        const v = line
          .slice(sep + 1)
          .trim()
          .replace(/^['"]|['"]$/g, '');
        if (k === 'name' && v) {
          parsedName = v.replace(/^\/+/, '');
        } else if (k === 'description' && v) {
          parsedDesc = v;
        }
      }
      if (!parsedDesc && body) {
        const firstLine = body
          .split(/\r?\n/)
          .map((l: string) => l.trim().replace(/^#+\s*/, ''))
          .find((l: string) => l.length > 0);
        parsedDesc = firstLine ? firstLine.slice(0, 120) : 'Workspace custom skill';
      }
      return {
        name: parsedName || fallbackName,
        description: parsedDesc || 'Workspace custom skill',
      };
    }
  }

  const firstContentLine = normalized
    .split(/\r?\n/)
    .map((l: string) => l.trim().replace(/^#+\s*/, ''))
    .find((l: string) => l.length > 0);
  return {
    name: fallbackName,
    description: firstContentLine ? firstContentLine.slice(0, 120) : 'Workspace custom skill',
  };
}

export class RursorSettingsPanel implements vscode.Disposable {
  public static readonly viewType = 'rursor.settings';
  private static currentPanel: RursorSettingsPanel | undefined;

  private readonly disposables: vscode.Disposable[] = [];
  private isDisposed = false;

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly context: vscode.ExtensionContext,
    private readonly client: SidecarClient,
    private onConfigUpdated: () => Promise<void>
  ) {
    this.panel.webview.html = this.renderHtml(this.panel.webview);

    this.panel.onDidDispose(
      () => {
        this.dispose();
      },
      null,
      this.disposables
    );

    this.panel.webview.onDidReceiveMessage(
      (msg: WebviewIncomingMessage) => {
        void this.handleMessage(msg);
      },
      null,
      this.disposables
    );

    this.disposables.push(
      vscode.workspace.onDidChangeConfiguration((e: vscode.ConfigurationChangeEvent) => {
        if (e.affectsConfiguration('rursor')) {
          void this.refreshState();
        }
      }),
      vscode.workspace.onDidSaveTextDocument((doc: vscode.TextDocument) => {
        const fsPath = doc.uri.fsPath;
        if (
          fsPath.endsWith('.rursorrules') ||
          fsPath.includes(`${path.sep}.rursor${path.sep}`)
        ) {
          void this.refreshState();
        }
      })
    );
  }

  public static openOrShow(
    context: vscode.ExtensionContext,
    client: SidecarClient,
    onConfigUpdated: () => Promise<void>
  ): void {
    if (RursorSettingsPanel.currentPanel) {
      RursorSettingsPanel.currentPanel.onConfigUpdated = onConfigUpdated;
      RursorSettingsPanel.currentPanel.panel.reveal(vscode.ViewColumn.One);
      void RursorSettingsPanel.currentPanel.refreshState();
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      'rursor.settings',
      'Settings',
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );

    RursorSettingsPanel.currentPanel = new RursorSettingsPanel(
      panel,
      context,
      client,
      onConfigUpdated
    );
  }

  public dispose(): void {
    if (this.isDisposed) {
      return;
    }
    this.isDisposed = true;
    if (RursorSettingsPanel.currentPanel === this) {
      RursorSettingsPanel.currentPanel = undefined;
    }
    while (this.disposables.length > 0) {
      const item = this.disposables.pop();
      item?.dispose();
    }
    this.panel.dispose();
  }

  private getWorkspaceRoot(): string | null {
    return vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null;
  }

  private postToast(message: string, kind: 'success' | 'error' | 'info' = 'success'): void {
    if (this.isDisposed) {
      return;
    }
    void this.panel.webview.postMessage({
      type: 'toast',
      message,
      kind,
    });
  }

  private async updateRursorSetting(key: string, value: unknown): Promise<void> {
    const cfg = vscode.workspace.getConfiguration('rursor');
    await cfg.update(key, value, vscode.ConfigurationTarget.Global);
  }

  private async handleMessage(msg: WebviewIncomingMessage): Promise<void> {
    try {
      switch (msg.type) {
        case 'ready':
        case 'refresh': {
          await this.refreshState();
          break;
        }

        case 'saveApiKey': {
          if (!msg.provider) {
            return;
          }
          const trimmed = (msg.value ?? '').trim();
          if (!trimmed) {
            this.postToast('Enter a non-empty API key to save, or click Clear.', 'error');
            return;
          }
          const secretId = this.secretIdForProvider(msg.provider);
          await this.context.secrets.store(secretId, trimmed);
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast(`${this.providerLabel(msg.provider)} API key saved to OS Keychain.`);
          break;
        }

        case 'clearApiKey': {
          if (!msg.provider) {
            return;
          }
          const secretId = this.secretIdForProvider(msg.provider);
          await this.context.secrets.delete(secretId);
          const clearField =
            msg.provider === 'gemini'
              ? { apiKey: '' }
              : msg.provider === 'anthropic'
                ? { anthropicApiKey: '' }
                : { openaiApiKey: '' };
          await this.client.request('set_config', clearField).promise.catch(() => undefined);
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast(`${this.providerLabel(msg.provider)} API key cleared.`, 'info');
          break;
        }

        case 'saveModelSettings': {
          if (typeof msg.defaultModel === 'string' && msg.defaultModel.trim()) {
            await this.updateRursorSetting('defaultModel', msg.defaultModel.trim());
          }
          if (typeof msg.thinkingBudget === 'number' && Number.isFinite(msg.thinkingBudget)) {
            const clamped = Math.max(0, Math.min(32768, Math.round(msg.thinkingBudget)));
            await this.updateRursorSetting('agent.thinkingBudget', clamped);
          }
          if (msg.permissionProfile) {
            await this.updateRursorSetting('agent.permissionProfile', msg.permissionProfile);
          }
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast('Model & Smart Router settings updated.');
          break;
        }

        case 'toggleSetting': {
          if (!msg.key) {
            return;
          }
          if (typeof msg.boolValue === 'boolean') {
            await this.updateRursorSetting(msg.key, msg.boolValue);
          } else if (typeof msg.value === 'string' && msg.value.length > 0) {
            await this.updateRursorSetting(msg.key, msg.value);
          } else {
            return;
          }
          await this.onConfigUpdated();
          await this.refreshState();
          break;
        }

        case 'saveTabSettings': {
          if (typeof msg.enabled === 'boolean') {
            await this.updateRursorSetting('autocomplete.enabled', msg.enabled);
          }
          if (typeof msg.nesEnabled === 'boolean') {
            await this.updateRursorSetting('autocomplete.nesEnabled', msg.nesEnabled);
          }
          if (msg.backend) {
            await this.updateRursorSetting('autocomplete.backend', msg.backend);
          }
          if (typeof msg.autocompleteModel === 'string' && msg.autocompleteModel.trim()) {
            await this.updateRursorSetting('autocomplete.model', msg.autocompleteModel.trim());
          }
          if (typeof msg.localEndpoint === 'string' && msg.localEndpoint.trim()) {
            await this.updateRursorSetting('autocomplete.localEndpoint', msg.localEndpoint.trim());
          }
          if (typeof msg.localModel === 'string' && msg.localModel.trim()) {
            await this.updateRursorSetting('autocomplete.localModel', msg.localModel.trim());
          }
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast('Tab & Next-Edit Prediction settings saved.');
          break;
        }

        case 'saveRules': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first to save .rursorrules.', 'error');
            return;
          }
          const rulesPath = path.join(root, '.rursorrules');
          await fs.writeFile(rulesPath, msg.rulesContent ?? '', 'utf8');
          await this.client
            .request('set_config', { workspaceRoot: root })
            .promise.catch(() => undefined);
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast('Saved .rursorrules and reloaded sidecar rules.');
          break;
        }

        case 'openRulesInEditor': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first.', 'error');
            return;
          }
          const rulesPath = path.join(root, '.rursorrules');
          await this.ensureFileExists(rulesPath, STARTER_RULES_TEMPLATE);
          await this.openFileInEditor(rulesPath);
          break;
        }

        case 'createSkillTemplate': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first to create a skill.', 'error');
            return;
          }
          const slug = sanitizeSkillSlug(msg.skillName ?? '');
          const skillsDir = path.join(root, '.rursor', 'skills');
          await fs.mkdir(skillsDir, { recursive: true });
          const targetPath = path.join(skillsDir, `${slug}.md`);
          const template = `---
name: ${slug}
description: Custom workspace workflow for /${slug}
---

Execute the /${slug} workflow carefully on the target context:

1. Inspect the relevant modules and types before proposing changes.
2. Apply clean, minimal edits for: {{args}}
3. Verify compilation and tests pass before concluding.
`;
          await this.ensureFileExists(targetPath, template);
          await this.refreshState();
          await this.openFileInEditor(targetPath);
          this.postToast(`Skill /${slug} ready in .rursor/skills/${slug}.md`);
          break;
        }

        case 'openSkillFile': {
          if (!msg.filePath) {
            return;
          }
          await this.openFileInEditor(msg.filePath);
          break;
        }

        case 'addMemory': {
          const content = (msg.memoryContent ?? '').trim();
          if (!content) {
            this.postToast('Memory content cannot be empty.', 'error');
            return;
          }
          const scope = msg.memoryScope === 'global' ? 'global' : 'workspace';
          const category = (msg.memoryCategory ?? 'general').trim() || 'general';
          await this.client.request('save_memory', {
            scope,
            category,
            content,
          }).promise;
          await this.refreshState();
          this.postToast(`Saved ${scope} memory (${category}).`);
          break;
        }

        case 'deleteMemory': {
          if (!msg.memoryId) {
            return;
          }
          await this.client.request('delete_memory', { id: msg.memoryId }).promise;
          await this.refreshState();
          this.postToast('Memory entry deleted.', 'info');
          break;
        }

        case 'openMemoryJson': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first.', 'error');
            return;
          }
          const memPath = path.join(root, '.rursor', 'memory.json');
          await fs.mkdir(path.dirname(memPath), { recursive: true });
          await this.ensureFileExists(memPath, '[]\n');
          await this.openFileInEditor(memPath);
          break;
        }

        case 'reloadMcp': {
          const res = await this.client.request<{
            servers?: Array<{ name: string; connected: boolean; toolCount: number; error?: string }>;
          }>('reload_mcp').promise;
          const servers = res.servers ?? [];
          const connectedCount = servers.filter((s) => s.connected).length;
          await this.onConfigUpdated();
          await this.refreshState();
          this.postToast(
            `Reloaded MCP servers (${connectedCount}/${servers.length} connected).`
          );
          break;
        }

        case 'editMcpJson': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first to configure .rursor/mcp.json.', 'error');
            return;
          }
          const mcpPath = path.join(root, '.rursor', 'mcp.json');
          await fs.mkdir(path.dirname(mcpPath), { recursive: true });
          await this.ensureFileExists(mcpPath, STARTER_MCP_JSON);
          await this.openFileInEditor(mcpPath);
          break;
        }

        case 'reindexWorkspace': {
          const root = this.getWorkspaceRoot();
          if (!root) {
            this.postToast('Open a workspace folder first to index.', 'error');
            return;
          }
          const res = await this.client.request<{ indexedFiles: number }>('index_workspace', {
            workspaceRoot: root,
          }).promise;
          await this.refreshState();
          this.postToast(`Indexed ${res.indexedFiles} workspace files.`);
          break;
        }

        case 'manageWorktrees': {
          await vscode.commands.executeCommand('rursor.manageWorktrees');
          await this.refreshState();
          break;
        }

        case 'restoreCheckpoint': {
          await vscode.commands.executeCommand('rursor.restoreCheckpoint');
          await this.refreshState();
          break;
        }

        case 'openVscodeSettings': {
          await vscode.commands.executeCommand('workbench.action.openSettings');
          break;
        }

        case 'openEditorSettings': {
          await vscode.commands.executeCommand('workbench.action.openSettings', 'editor.');
          break;
        }

        case 'openKeyboardShortcuts': {
          await vscode.commands.executeCommand('workbench.action.openGlobalKeybindings');
          break;
        }

        case 'importFromVsCode': {
          await vscode.commands.executeCommand('workbench.action.openSettingsSync');
          this.postToast('Open Settings Sync / profiles to import VS Code settings.', 'info');
          break;
        }

        case 'resetDontAskAgain': {
          await this.context.globalState.update('rursor.dismissedDialogs', []);
          this.postToast('Hidden dialogs and tips have been restored.');
          break;
        }

        case 'dismissCustomizeBanner': {
          await this.updateRursorSetting('ui.customizeBannerDismissed', true);
          await this.refreshState();
          break;
        }

        case 'openExternal': {
          if (msg.url) {
            await vscode.env.openExternal(vscode.Uri.parse(msg.url));
          }
          break;
        }

        case 'runCommand': {
          if (msg.command) {
            await vscode.commands.executeCommand(msg.command);
          }
          break;
        }

        default:
          break;
      }
    } catch (err: unknown) {
      const detail = err instanceof Error ? err.message : String(err);
      this.postToast(detail, 'error');
    }
  }

  private secretIdForProvider(provider: ProviderId): string {
    switch (provider) {
      case 'gemini':
        return SECRET_GEMINI_KEY;
      case 'anthropic':
        return SECRET_ANTHROPIC_KEY;
      case 'openai':
        return SECRET_OPENAI_KEY;
    }
  }

  private providerLabel(provider: ProviderId): string {
    switch (provider) {
      case 'gemini':
        return 'Gemini';
      case 'anthropic':
        return 'Anthropic';
      case 'openai':
        return 'OpenAI';
    }
  }

  private async ensureFileExists(filePath: string, starterContent: string): Promise<void> {
    try {
      await fs.access(filePath);
    } catch {
      await fs.mkdir(path.dirname(filePath), { recursive: true });
      await fs.writeFile(filePath, starterContent, 'utf8');
    }
  }

  private async openFileInEditor(filePath: string): Promise<void> {
    const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(filePath));
    await vscode.window.showTextDocument(doc, {
      viewColumn: vscode.ViewColumn.Beside,
      preview: false,
    });
  }

  public async refreshState(): Promise<void> {
    if (this.isDisposed) {
      return;
    }

    const cfg = vscode.workspace.getConfiguration('rursor');
    const root = this.getWorkspaceRoot();

    const [geminiSecret, anthropicSecret, openaiSecret, statusRes] = await Promise.all([
      this.context.secrets.get(SECRET_GEMINI_KEY),
      this.context.secrets.get(SECRET_ANTHROPIC_KEY),
      this.context.secrets.get(SECRET_OPENAI_KEY),
      this.client
        .request<SidecarStatusResult>('status')
        .promise.catch((): SidecarStatusResult => ({ ready: false })),
    ]);

    const [mcpStatusRes, memoriesRes, skillsRes, worktreesRes, checkpointsRes] = await Promise.all([
      Array.isArray(statusRes.mcpServers)
        ? Promise.resolve({ servers: statusRes.mcpServers })
        : this.client
            .request<{
              servers?: Array<{
                name: string;
                connected: boolean;
                toolCount: number;
                error?: string;
              }>;
            }>('mcp_status')
            .promise.catch(() => ({ servers: [] })),
      this.client
        .request<{ memories?: MemoryItem[] }>('list_memories')
        .promise.catch(() => ({ memories: [] as MemoryItem[] })),
      this.client
        .request<{
          skills?: Array<{ name: string; description: string; source: string }>;
        }>('list_skills')
        .promise.catch(() => ({ skills: [] })),
      this.client
        .request<{ worktrees?: WorktreeItem[] }>('list_worktrees')
        .promise.catch(() => ({ worktrees: [] as WorktreeItem[] })),
      this.client
        .request<{ checkpoints?: CheckpointItem[] }>('list_checkpoints')
        .promise.catch(() => ({ checkpoints: [] as CheckpointItem[] })),
    ]);

    let rursorRulesContent = '';
    const rursorRulesPath = root ? path.join(root, '.rursorrules') : null;
    if (rursorRulesPath) {
      rursorRulesContent = await fs.readFile(rursorRulesPath, 'utf8').catch(() => '');
    }

    const workspaceSkills: SkillFileItem[] = [];
    if (root) {
      const skillsDir = path.join(root, '.rursor', 'skills');
      const entries = await fs.readdir(skillsDir, { withFileTypes: true }).catch(() => []);
      const mdNames = entries
        .filter((e) => e.isFile() && e.name.toLowerCase().endsWith('.md'))
        .map((e) => e.name)
        .sort();
      for (const fileName of mdNames) {
        const absPath = path.join(skillsDir, fileName);
        const raw = await fs.readFile(absPath, 'utf8').catch(() => '');
        const summary = parseSkillFrontmatterSummary(fileName, raw);
        workspaceSkills.push({
          name: summary.name,
          description: summary.description,
          source: 'workspace',
          fileName,
          absPath,
        });
      }
    }

    const memoryJsonPath = root ? path.join(root, '.rursor', 'memory.json') : null;
    let rawMemoryJson = '[]';
    if (memoryJsonPath) {
      rawMemoryJson = await fs.readFile(memoryJsonPath, 'utf8').catch(() => '[]');
    }

    const mcpJsonPath = root ? path.join(root, '.rursor', 'mcp.json') : null;
    const configuredMcpCommands = new Map<string, string>();
    if (mcpJsonPath) {
      const rawMcp = await fs.readFile(mcpJsonPath, 'utf8').catch(() => '');
      if (rawMcp.trim()) {
        try {
          const parsed = JSON.parse(rawMcp) as {
            mcpServers?: Record<string, { command?: string; args?: string[] }>;
          };
          if (parsed.mcpServers && typeof parsed.mcpServers === 'object') {
            for (const [serverName, spec] of Object.entries(parsed.mcpServers)) {
              const cmd = spec?.command ?? '';
              const args = Array.isArray(spec?.args) ? spec.args.join(' ') : '';
              configuredMcpCommands.set(serverName, `${cmd} ${args}`.trim());
            }
          }
        } catch {
          // Ignore malformed user mcp.json while editing
        }
      }
    }

    const liveServers = mcpStatusRes.servers ?? [];
    const mergedServersMap = new Map<string, McpServerStatusItem>();
    for (const s of liveServers) {
      mergedServersMap.set(s.name, {
        name: s.name,
        connected: Boolean(s.connected),
        toolCount: s.toolCount ?? 0,
        commandSummary: configuredMcpCommands.get(s.name),
        error: s.error,
      });
    }
    for (const [cfgName, cmdSummary] of configuredMcpCommands.entries()) {
      if (!mergedServersMap.has(cfgName)) {
        mergedServersMap.set(cfgName, {
          name: cfgName,
          connected: false,
          toolCount: 0,
          commandSummary: cmdSummary,
          error: 'Not loaded yet — click Reload MCP',
        });
      }
    }

    const rawProfile = cfg.get<string>('agent.permissionProfile', 'safe-auto');
    const permissionProfile: PermissionProfile =
      rawProfile === 'read-only' || rawProfile === 'full-auto' ? rawProfile : 'safe-auto';

    const rawBackend = cfg.get<string>('autocomplete.backend', 'gemini');
    const backend: AutocompleteBackend =
      rawBackend === 'auto' || rawBackend === 'ollama' || rawBackend === 'mlx'
        ? rawBackend
        : 'gemini';

    const rawLayout = cfg.get<string>('layout.windowLayout', 'editor');
    const windowLayout: 'agent' | 'editor' = rawLayout === 'agent' ? 'agent' : 'editor';
    const rawDensity = cfg.get<string>('layout.conversationDensity', 'detailed');
    const conversationDensity: LayoutState['conversationDensity'] =
      rawDensity === 'compact' || rawDensity === 'verbose' ? rawDensity : 'detailed';
    const rawReviewLoc = cfg.get<string>('layout.reviewControlLocation', 'breadcrumb');
    const reviewControlLocation: LayoutState['reviewControlLocation'] =
      rawReviewLoc === 'floating' ? 'floating' : 'breadcrumb';

    const statePayload: DashboardStatePayload = {
      workspaceRoot: statusRes.workspaceRoot ?? root,
      sidecarReady: Boolean(statusRes.ready),
      accountEmail: process.env.USER
        ? `${process.env.USER}@local`
        : 'rursor@local',
      accountPlan: 'Pro+ Plan',
      keys: {
        gemini: {
          hasKeychainKey: Boolean(geminiSecret?.trim()),
          hasSidecarKey: Boolean(statusRes.hasApiKey),
          maskedPreview: maskSecret(geminiSecret),
        },
        anthropic: {
          hasKeychainKey: Boolean(anthropicSecret?.trim()),
          hasSidecarKey: Boolean(statusRes.hasAnthropicKey),
          maskedPreview: maskSecret(anthropicSecret),
        },
        openai: {
          hasKeychainKey: Boolean(openaiSecret?.trim()),
          hasSidecarKey: Boolean(statusRes.hasOpenAiKey),
          maskedPreview: maskSecret(openaiSecret),
        },
      },
      models: {
        defaultModel: cfg.get<string>('defaultModel', 'gemini-3.8-flash'),
        thinkingBudget: cfg.get<number>('agent.thinkingBudget', 0),
        permissionProfile,
        promptCache: cfg.get<boolean>('agent.promptCache', true),
        autoVerify: cfg.get<boolean>('agent.autoVerify', false),
      },
      layout: {
        windowLayout,
        conversationDensity,
        showTitleBar: cfg.get<boolean>('layout.showTitleBar', true),
        showStatusBar: cfg.get<boolean>('layout.showStatusBar', true),
        reviewControlLocation,
        autoHideEditorWhenEmpty: cfg.get<boolean>('layout.autoHideEditorWhenEmpty', false),
        openChatAsEditorTabs: cfg.get<boolean>('layout.openChatAsEditorTabs', true),
        customizeBannerDismissed: cfg.get<boolean>('ui.customizeBannerDismissed', false),
      },
      tab: {
        enabled: cfg.get<boolean>('autocomplete.enabled', true),
        nesEnabled: cfg.get<boolean>('autocomplete.nesEnabled', true),
        backend,
        autocompleteModel: cfg.get<string>('autocomplete.model', 'gemini-3.5-flash-lite'),
        localEndpoint: cfg.get<string>('autocomplete.localEndpoint', 'http://127.0.0.1:11434'),
        localModel: cfg.get<string>('autocomplete.localModel', 'qwen2.5-coder:7b'),
      },
      rulesAndMemory: {
        hasWorkspaceRules: Boolean(statusRes.hasWorkspaceRules || rursorRulesContent.trim()),
        rursorRulesContent,
        rursorRulesPath,
        workspaceSkills,
        allSkillsCount: skillsRes.skills?.length ?? statusRes.skillsCount ?? workspaceSkills.length,
        memories: memoriesRes.memories ?? [],
        rawMemoryJson,
        memoryJsonPath,
      },
      mcp: {
        mcpJsonPath,
        servers: Array.from(mergedServersMap.values()),
      },
      indexAndSandbox: {
        indexedFiles: statusRes.indexedFiles ?? 0,
        skillsCount: statusRes.skillsCount ?? skillsRes.skills?.length ?? 0,
        memoriesCount: statusRes.memoriesCount ?? memoriesRes.memories?.length ?? 0,
        backgroundProcesses: statusRes.backgroundProcesses ?? 0,
        usePty: cfg.get<boolean>('agent.usePty', true),
        worktrees: worktreesRes.worktrees ?? [],
        checkpoints: checkpointsRes.checkpoints ?? [],
      },
    };

    void this.panel.webview.postMessage({
      type: 'state',
      payload: statePayload,
    });
  }

  private renderHtml(webview: vscode.Webview): string {
    const nonce = crypto.randomBytes(16).toString('base64');

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta
    http-equiv="Content-Security-Policy"
    content="default-src 'none'; style-src ${webview.cspSource} 'unsafe-inline'; script-src 'nonce-${nonce}';"
  />
  <title>Rursor Settings</title>
  <style>
    :root {
      --obs-bg: #0c0c0c;
      --obs-surface: #0c0c0c;
      --obs-card: #141414;
      --obs-card-hover: #1a1a1a;
      --obs-border: #262626;
      --obs-input-bg: #141414;
      --obs-input-border: #333333;
      --obs-fg: #ececec;
      --obs-muted: #a1a1a1;
      --obs-cyan: #d4d4d4;
      --obs-cyan-dim: rgba(255, 255, 255, 0.08);
      --obs-violet: #d4d4d4;
      --obs-violet-dim: rgba(255, 255, 255, 0.08);
      --obs-emerald: #86efac;
      --obs-emerald-dim: rgba(134, 239, 172, 0.12);
      --obs-rose: #fda4af;
      --obs-rose-dim: rgba(253, 164, 175, 0.12);
      --obs-amber: #fbbf24;
      --radius-lg: 10px;
      --radius-md: 7px;
      --radius-sm: 5px;
    }

    * {
      box-sizing: border-box;
    }

    body {
      margin: 0;
      padding: 0;
      background: var(--obs-bg);
      color: var(--obs-fg);
      font-family: var(--vscode-font-family, -apple-system, BlinkMacSystemFont, "Inter", sans-serif);
      font-size: 13px;
      line-height: 1.5;
      height: 100vh;
      display: flex;
      flex-direction: column;
      overflow: hidden;
    }

    /* Main Split Shell — Cursor-like Settings */
    .shell {
      display: grid;
      grid-template-columns: 240px 1fr;
      flex: 1;
      min-height: 0;
      height: 100vh;
    }

    @media (max-width: 760px) {
      .shell {
        grid-template-columns: 1fr;
      }
      .nav-rail {
        max-height: 42vh;
        border-right: none;
        border-bottom: 1px solid var(--obs-border);
      }
    }

    .nav-rail {
      background: #141414;
      border-right: 1px solid #2a2a2a;
      padding: 14px 10px 12px;
      display: flex;
      flex-direction: column;
      gap: 2px;
      overflow-y: auto;
    }

    .profile-card-nav {
      padding: 8px 10px 14px;
      display: flex;
      gap: 10px;
      align-items: center;
    }
    .profile-avatar {
      width: 32px;
      height: 32px;
      border-radius: 999px;
      background: #3f3f46;
      color: #fff;
      display: flex;
      align-items: center;
      justify-content: center;
      font-size: 12px;
      font-weight: 700;
      flex-shrink: 0;
    }
    .profile-meta { min-width: 0; }
    .profile-email {
      font-size: 12px;
      font-weight: 600;
      color: #e8e8e8;
      word-break: break-all;
    }
    .profile-plan {
      margin-top: 4px;
      display: inline-block;
      font-size: 10px;
      font-weight: 700;
      padding: 2px 7px;
      border-radius: 999px;
      background: rgba(255,255,255,0.08);
      color: #d4d4d4;
    }
    .section-label {
      font-size: 12px;
      font-weight: 600;
      color: #8a8a8a;
      margin: 18px 2px 8px;
    }
    .btn-open {
      background: #2a2a2a !important;
      border: 1px solid #3a3a3a !important;
      color: #eee !important;
      border-radius: 8px !important;
      padding: 5px 12px !important;
      font-size: 12px !important;
    }
    .search-wrap {
      padding: 0 6px 10px;
    }
    .search-wrap input {
      width: 100%;
      background: #1c1c1c;
      border: 1px solid #333;
      border-radius: 8px;
      color: #ddd;
      padding: 7px 10px;
      font-size: 12px;
    }
    .nav-divider {
      height: 1px;
      background: #2a2a2a;
      margin: 8px 8px;
    }
    .ext-ico {
      opacity: 0.55;
      font-size: 11px;
    }
    .nav-footer {
      display: flex;
      flex-direction: column;
      gap: 8px;
      padding: 10px 6px 4px;
      border-top: 1px solid #2a2a2a;
      margin-top: 8px;
    }

    .nav-btn {
      appearance: none;
      border: none;
      background: transparent;
      color: #b5b5b5;
      text-align: left;
      padding: 8px 10px;
      border-radius: 8px;
      font-size: 13px;
      font-weight: 500;
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
    }

    .nav-btn:hover {
      background: rgba(255,255,255,0.05);
      color: #f3f3f3;
    }

    .nav-btn.active {
      background: rgba(255,255,255,0.08);
      color: #fff;
      font-weight: 600;
    }

    .nav-badge {
      font-size: 10px;
      padding: 1px 6px;
      border-radius: 999px;
      background: rgba(148, 163, 184, 0.16);
      color: var(--obs-fg);
    }

    .nav-btn.active .nav-badge {
      background: rgba(255,255,255,0.18);
      color: #fff;
      font-weight: 700;
    }

    .notice-banner {
      display: flex;
      justify-content: space-between;
      gap: 12px;
      align-items: center;
      padding: 12px 14px;
      border: 1px solid #333;
      border-radius: 10px;
      background: #1a1a1a;
      margin-bottom: 18px;
    }
    .notice-banner[hidden] { display: none; }
    .notice-text { font-size: 12.5px; color: #cfcfcf; line-height: 1.45; }
    .notice-actions { display: flex; gap: 8px; flex-shrink: 0; }

    .settings-group {
      border: 1px solid #2c2c2c;
      border-radius: 12px;
      background: #171717;
      margin-bottom: 16px;
      overflow: hidden;
    }
    .group-label {
      font-size: 11px;
      font-weight: 700;
      letter-spacing: 0.04em;
      text-transform: uppercase;
      color: #8a8a8a;
      padding: 12px 14px 4px;
    }
    .settings-row {
      display: flex;
      justify-content: space-between;
      align-items: center;
      gap: 16px;
      padding: 14px 16px;
      border-top: 1px solid #262626;
    }
    .settings-group > .settings-row:first-child { border-top: none; }
    .settings-copy { min-width: 0; }
    .settings-title {
      font-size: 13.5px;
      font-weight: 600;
      color: #f2f2f2;
      display: flex;
      align-items: center;
      gap: 8px;
    }
    .settings-sub {
      margin-top: 3px;
      font-size: 12px;
      color: #8f8f8f;
      line-height: 1.4;
    }
    .new-pill {
      font-size: 9px;
      font-weight: 800;
      letter-spacing: 0.06em;
      padding: 2px 6px;
      border-radius: 999px;
      background: rgba(52,211,153,0.18);
      color: #6ee7b7;
    }
    .inline-select {
      min-width: 140px;
      background: #222 !important;
      border: 1px solid #3a3a3a !important;
      border-radius: 8px !important;
      color: #eee !important;
      padding: 6px 10px !important;
    }
    .layout-picker {
      display: flex;
      gap: 8px;
    }
    .layout-card {
      width: 78px;
      background: #1d1d1d;
      border: 1px solid #333;
      border-radius: 10px;
      padding: 8px 6px 6px;
      cursor: pointer;
      color: #bbb;
    }
    .layout-card.selected {
      border-color: #5b9dff;
      box-shadow: 0 0 0 1px rgba(91,157,255,0.35);
      color: #fff;
    }
    .layout-preview {
      height: 42px;
      border-radius: 6px;
      background: #111;
      border: 1px solid #2f2f2f;
      display: grid;
      gap: 2px;
      padding: 4px;
      margin-bottom: 6px;
    }
    .layout-preview span {
      background: #2a2a2a;
      border-radius: 2px;
      display: block;
    }
    .layout-preview.agent {
      grid-template-columns: 1.1fr 0.7fr;
    }
    .layout-preview.agent span:first-child { grid-row: 1 / 3; }
    .layout-preview.editor {
      grid-template-columns: 0.35fr 1fr 0.55fr;
    }
    .layout-preview.editor span { min-height: 100%; }
    .layout-name { font-size: 11px; text-align: center; font-weight: 600; }
    .workspace-hint {
      font-size: 11px;
      color: #777;
      margin: 8px 2px 20px;
    }

    /* Content Viewport */
    .content {
      overflow-y: auto;
      padding: 28px 36px 56px;
      background: #181818;
    }

    .tab-pane {
      display: none;
      max-width: 760px;
      animation: fadeIn 0.14s ease;
    }

    .tab-pane.active {
      display: block;
    }

    @keyframes fadeIn {
      from { opacity: 0; transform: translateY(3px); }
      to { opacity: 1; transform: translateY(0); }
    }

    .pane-header {
      margin-bottom: 18px;
    }

    .pane-title {
      font-size: 22px;
      font-weight: 650;
      color: #f5f5f5;
      margin: 0 0 4px;
      letter-spacing: -0.02em;
    }

    .pane-desc {
      margin: 0;
      color: var(--obs-muted);
      font-size: 12.5px;
    }

    /* Cards & Controls */
    .card {
      background: var(--obs-card);
      border: 1px solid var(--obs-border);
      border-radius: var(--radius-lg);
      padding: 18px 20px;
      margin-bottom: 16px;
    }

    .card-head {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 12px;
      margin-bottom: 14px;
    }

    .card-title {
      font-size: 14px;
      font-weight: 600;
      color: #F8FAFC;
      margin: 0 0 2px;
    }

    .card-subtitle {
      font-size: 12px;
      color: var(--obs-muted);
      margin: 0;
    }

    /* Pills & Badges */
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      font-size: 11px;
      font-weight: 600;
      padding: 3px 9px;
      border-radius: 999px;
      white-space: nowrap;
    }

    .badge-ok {
      background: var(--obs-emerald-dim);
      color: var(--obs-emerald);
      border: 1px solid rgba(52, 211, 153, 0.3);
    }

    .badge-warn {
      background: rgba(148, 163, 184, 0.12);
      color: var(--obs-muted);
      border: 1px solid var(--obs-border);
    }

    .badge-err {
      background: var(--obs-rose-dim);
      color: var(--obs-rose);
      border: 1px solid rgba(251, 113, 133, 0.3);
    }

    .badge-violet {
      background: var(--obs-violet-dim);
      color: #DDD6FE;
      border: 1px solid rgba(167, 139, 250, 0.32);
    }

    /* Form Rows */
    .field-grid {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 14px;
    }

    @media (max-width: 680px) {
      .field-grid {
        grid-template-columns: 1fr;
      }
    }

    .field {
      display: flex;
      flex-direction: column;
      gap: 6px;
      margin-bottom: 14px;
    }

    .field:last-child {
      margin-bottom: 0;
    }

    .field label {
      font-size: 12px;
      font-weight: 600;
      color: #CBD5E1;
      display: flex;
      align-items: center;
      justify-content: space-between;
    }

    .field-hint {
      font-size: 11.5px;
      color: var(--obs-muted);
    }

    input[type="text"],
    input[type="password"],
    input[type="number"],
    select,
    textarea {
      background: var(--obs-input-bg);
      color: var(--obs-fg);
      border: 1px solid var(--obs-input-border);
      border-radius: var(--radius-sm);
      padding: 7px 10px;
      font-size: 12.5px;
      font-family: inherit;
      outline: none;
      transition: border-color 0.12s ease, box-shadow 0.12s ease;
    }

    input:focus,
    select:focus,
    textarea:focus {
      border-color: var(--obs-cyan);
      box-shadow: 0 0 0 2px rgba(56, 189, 248, 0.18);
    }

    textarea.code-area {
      font-family: var(--vscode-editor-font-family, "JetBrains Mono", Menlo, monospace);
      font-size: 12px;
      min-height: 185px;
      resize: vertical;
      line-height: 1.55;
    }

    /* Buttons */
    .btn-row {
      display: flex;
      align-items: center;
      gap: 8px;
      flex-wrap: wrap;
    }

    button.btn {
      appearance: none;
      border: 1px solid var(--obs-border);
      background: #161D2B;
      color: #E2E8F0;
      padding: 6px 13px;
      border-radius: var(--radius-sm);
      font-size: 12px;
      font-weight: 600;
      cursor: pointer;
      display: inline-flex;
      align-items: center;
      gap: 6px;
      transition: all 0.12s ease;
    }

    button.btn:hover {
      background: #1E283B;
      border-color: #2E3D59;
    }

    button.btn-primary {
      background: var(--obs-cyan);
      color: #06080C;
      border-color: var(--obs-cyan);
    }

    button.btn-primary:hover {
      background: #7DD3FC;
      border-color: #7DD3FC;
    }

    button.btn-violet {
      background: var(--obs-violet-dim);
      color: #DDD6FE;
      border-color: rgba(167, 139, 250, 0.4);
    }

    button.btn-violet:hover {
      background: rgba(167, 139, 250, 0.28);
    }

    button.btn-danger {
      background: transparent;
      color: var(--obs-rose);
      border-color: rgba(251, 113, 133, 0.35);
    }

    button.btn-danger:hover {
      background: var(--obs-rose-dim);
    }

    /* API Key Rows */
    .key-row {
      padding: 13px 0;
      border-top: 1px solid var(--obs-border);
      display: flex;
      flex-direction: column;
      gap: 8px;
    }

    .key-row:first-of-type {
      border-top: none;
      padding-top: 2px;
    }

    .key-meta {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      flex-wrap: wrap;
    }

    .key-title {
      font-weight: 600;
      color: #F1F5F9;
      display: flex;
      align-items: center;
      gap: 8px;
    }

    .mono {
      font-family: var(--vscode-editor-font-family, Menlo, monospace);
      font-size: 11.5px;
    }

    .key-controls {
      display: flex;
      gap: 8px;
      align-items: center;
    }

    .key-controls input {
      flex: 1;
    }

    /* Toggle Switch Rows */
    .toggle-row {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      padding: 11px 0;
      border-top: 1px solid var(--obs-border);
    }

    .toggle-row:first-child {
      border-top: none;
      padding-top: 0;
    }

    .toggle-info {
      display: flex;
      flex-direction: column;
      gap: 2px;
    }

    .toggle-title {
      font-weight: 600;
      color: #F1F5F9;
      font-size: 13px;
    }

    .toggle-sub {
      font-size: 11.5px;
      color: var(--obs-muted);
    }

    .switch {
      position: relative;
      display: inline-block;
      width: 40px;
      height: 22px;
      flex-shrink: 0;
    }

    .switch input {
      opacity: 0;
      width: 0;
      height: 0;
    }

    .slider {
      position: absolute;
      cursor: pointer;
      inset: 0;
      background-color: #3a3a3a;
      border: 1px solid #4a4a4a;
      transition: 0.16s;
      border-radius: 999px;
    }

    .slider:before {
      position: absolute;
      content: "";
      height: 16px;
      width: 16px;
      left: 2px;
      bottom: 2px;
      background-color: #d4d4d4;
      transition: 0.16s;
      border-radius: 50%;
    }

    .switch input:checked + .slider {
      background-color: #22c55e;
      border-color: #16a34a;
    }

    .switch input:checked + .slider:before {
      transform: translateX(18px);
      background-color: #fff;
    }

    /* Permission Profile Cards */
    .profile-grid {
      display: grid;
      grid-template-columns: repeat(3, 1fr);
      gap: 10px;
      margin-top: 6px;
    }

    @media (max-width: 700px) {
      .profile-grid {
        grid-template-columns: 1fr;
      }
    }

    .profile-card {
      border: 1px solid var(--obs-border);
      background: var(--obs-input-bg);
      border-radius: var(--radius-md);
      padding: 11px 12px;
      cursor: pointer;
      transition: all 0.12s ease;
    }

    .profile-card:hover {
      border-color: #2E3D59;
    }

    .profile-card.selected {
      border-color: var(--obs-cyan);
      background: var(--obs-cyan-dim);
    }

    .profile-name {
      font-weight: 700;
      font-size: 12.5px;
      color: #F8FAFC;
      margin-bottom: 4px;
    }

    .profile-desc {
      font-size: 11px;
      color: var(--obs-muted);
      line-height: 1.4;
    }

    /* Slider + Number */
    .budget-wrap {
      display: flex;
      align-items: center;
      gap: 12px;
    }

    .budget-wrap input[type="range"] {
      flex: 1;
      accent-color: var(--obs-cyan);
    }

    .preset-pills {
      display: flex;
      gap: 6px;
      flex-wrap: wrap;
      margin-top: 6px;
    }

    .preset-pill {
      font-size: 11px;
      padding: 2px 8px;
      border-radius: 999px;
      background: rgba(148, 163, 184, 0.12);
      border: 1px solid var(--obs-border);
      color: var(--obs-muted);
      cursor: pointer;
    }

    .preset-pill:hover {
      border-color: var(--obs-cyan);
      color: #F8FAFC;
    }

    /* Tables & Lists */
    table.data-table {
      width: 100%;
      border-collapse: collapse;
      font-size: 12.5px;
    }

    table.data-table th {
      text-align: left;
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0.05em;
      color: var(--obs-muted);
      padding: 8px 10px;
      border-bottom: 1px solid var(--obs-border);
    }

    table.data-table td {
      padding: 10px;
      border-bottom: 1px solid rgba(27, 35, 52, 0.65);
      vertical-align: middle;
    }

    table.data-table tr:last-child td {
      border-bottom: none;
    }

    .list-item {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      padding: 10px 12px;
      border: 1px solid var(--obs-border);
      border-radius: var(--radius-md);
      background: var(--obs-input-bg);
      margin-bottom: 8px;
    }

    .stat-grid {
      display: grid;
      grid-template-columns: repeat(4, 1fr);
      gap: 12px;
      margin-bottom: 16px;
    }

    @media (max-width: 720px) {
      .stat-grid {
        grid-template-columns: repeat(2, 1fr);
      }
    }

    .stat-box {
      background: var(--obs-input-bg);
      border: 1px solid var(--obs-border);
      border-radius: var(--radius-md);
      padding: 12px 14px;
    }

    .stat-value {
      font-size: 22px;
      font-weight: 700;
      color: var(--obs-cyan);
      line-height: 1.2;
    }

    .stat-label {
      font-size: 11px;
      color: var(--obs-muted);
      margin-top: 3px;
    }

    pre.json-viewer {
      background: var(--obs-input-bg);
      border: 1px solid var(--obs-border);
      border-radius: var(--radius-md);
      padding: 12px;
      max-height: 210px;
      overflow: auto;
      font-family: var(--vscode-editor-font-family, Menlo, monospace);
      font-size: 11.5px;
      color: #CBD5E1;
      margin: 10px 0 0;
    }

    /* Toast Banner */
    .toast {
      position: fixed;
      bottom: 20px;
      right: 24px;
      padding: 10px 16px;
      border-radius: var(--radius-md);
      font-size: 12.5px;
      font-weight: 600;
      box-shadow: 0 8px 24px rgba(0, 0, 0, 0.45);
      z-index: 100;
      transition: opacity 0.2s ease, transform 0.2s ease;
    }

    .toast[hidden] {
      display: none;
    }

    .toast.success {
      background: #0D2821;
      border: 1px solid var(--obs-emerald);
      color: #A7F3D0;
    }

    .toast.error {
      background: #2C1018;
      border: 1px solid var(--obs-rose);
      color: #FECDD3;
    }

    .toast.info {
      background: #0E2034;
      border: 1px solid var(--obs-cyan);
      color: #BAE6FD;
    }
  </style>
</head>
<body>
  <div class="shell">
    <nav class="nav-rail" id="navRail">
      <div class="profile-card-nav">
        <div class="profile-avatar" aria-hidden="true">R</div>
        <div class="profile-meta">
          <div class="profile-email" id="accountEmailLabel">local</div>
          <div class="profile-plan" id="accountPlanLabel">Pro+ Plan</div>
        </div>
      </div>
      <div class="search-wrap">
        <input type="search" id="settingsSearch" placeholder="Search settings ⌘F" />
      </div>
      <button type="button" class="nav-btn active" data-tab="general"><span>General</span></button>
      <button type="button" class="nav-btn" data-action="open-vscode-settings">
        <span>VS Code Settings</span><span class="ext-ico">↗</span>
      </button>
      <div class="nav-divider"></div>
      <button type="button" class="nav-btn" data-tab="plan"><span>Plan &amp; Usage</span></button>
      <button type="button" class="nav-btn" data-tab="agents"><span>Agents</span></button>
      <button type="button" class="nav-btn" data-tab="cloud"><span>Cloud Agents</span></button>
      <button type="button" class="nav-btn" data-tab="models"><span>Models</span></button>
      <button type="button" class="nav-btn" data-tab="voice"><span>Voice</span></button>
      <button type="button" class="nav-btn" data-tab="source"><span>Source Control</span></button>
      <div class="nav-divider"></div>
      <button type="button" class="nav-btn" data-tab="customize">
        <span>Customize</span><span class="ext-ico">↗</span>
      </button>
      <button type="button" class="nav-btn" data-tab="browser"><span>Browser &amp; Network</span></button>
      <button type="button" class="nav-btn" data-tab="codeintel"><span>Code Intelligence</span></button>
      <button type="button" class="nav-btn" data-tab="beta"><span>Beta</span></button>
      <button type="button" class="nav-btn" data-action="open-docs">
        <span>Docs</span><span class="ext-ico">↗</span>
      </button>
      <span id="navKeysBadge" hidden>0/3</span>
      <span id="navIndexBadge" hidden>0</span>
      <span id="navRulesBadge" hidden>0</span>
      <span id="navTabBadge" hidden>On</span>
      <span id="navMcpBadge" hidden>0</span>
      <div style="flex:1"></div>
      <div class="nav-footer">
        <span id="sidecarStatusBadge" class="badge badge-warn">Connecting...</span>
        <button type="button" class="btn" id="refreshAllBtn" hidden>↻ Refresh</button>
      </div>
    </nav>

    <main class="content">
      <!-- TAB: GENERAL (Cursor-like) -->
      <section class="tab-pane active" id="pane-general">
        <div class="notice-banner" id="customizeBanner">
          <div class="notice-text">Plugins, MCPs, Skills, and Rules have moved to Customize. We've introduced a new home for all the ways to customize Rursor.</div>
          <div class="notice-actions">
            <button type="button" class="btn" id="dismissBannerBtn">Dismiss</button>
            <button type="button" class="btn btn-primary" data-goto="customize">Open Customize →</button>
          </div>
        </div>

        <div class="pane-header">
          <h1 class="pane-title">General</h1>
        </div>

        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Rursor Account</div>
              <div class="settings-sub">Manage your account and billing</div>
            </div>
            <button type="button" class="btn btn-open" id="openAccountBtn">Open ↗</button>
          </div>
        </div>

        <div class="section-label">Preferences</div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Editor Settings</div>
              <div class="settings-sub">Configure font, formatting, minimap and more</div>
            </div>
            <button type="button" class="btn btn-open" id="openEditorSettingsBtn">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Keyboard Shortcuts</div>
              <div class="settings-sub">Configure keyboard shortcuts</div>
            </div>
            <button type="button" class="btn btn-open" id="openKeysBtn">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Import Settings from VS Code</div>
              <div class="settings-sub">Import settings, extensions, and keybindings from VS Code</div>
            </div>
            <button type="button" class="btn btn-open" id="importVsCodeBtn">Import</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Reset "Don't Ask Again" Dialogs</div>
              <div class="settings-sub">See warnings and tips that you've hidden</div>
            </div>
            <button type="button" class="btn btn-open" id="resetDontAskBtn">Show</button>
          </div>
        </div>

        <div class="section-label">Layout</div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Window Layout</div>
              <div class="settings-sub">Switch between Agent and Editor default layouts.</div>
            </div>
            <div class="layout-picker" id="windowLayoutPicker">
              <button type="button" class="layout-card" data-layout="agent" title="Agent">
                <div class="layout-preview agent"><span></span><span></span></div>
                <div class="layout-name">Agent</div>
              </button>
              <button type="button" class="layout-card selected" data-layout="editor" title="Editor">
                <div class="layout-preview editor"><span></span><span></span><span></span></div>
                <div class="layout-name">Editor</div>
              </button>
            </div>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Conversation Density</div>
              <div class="settings-sub">Choose how much detail Agent tool calls show in the conversation.</div>
            </div>
            <select id="conversationDensitySelect" class="inline-select">
              <option value="compact">Compact</option>
              <option value="detailed" selected>Detailed</option>
              <option value="verbose">Verbose</option>
            </select>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Title Bar</div>
              <div class="settings-sub">Show title bar in agent layout.</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleTitleBar" checked /><span class="slider"></span></label>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Status Bar</div>
              <div class="settings-sub">Show status bar at the bottom of the window.</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleStatusBar" checked /><span class="slider"></span></label>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Review Control Location</div>
              <div class="settings-sub">Show inline diff review controls in top level breadcrumbs or floating island.</div>
            </div>
            <select id="reviewControlSelect" class="inline-select">
              <option value="breadcrumb" selected>Breadcrumb</option>
              <option value="floating">Floating Island</option>
            </select>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Auto-Hide Editor When Empty</div>
              <div class="settings-sub">When all editors are closed, hide the editor area and maximize chat.</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleAutoHideEditor" /><span class="slider"></span></label>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Open Chat as Editor Tabs <span class="new-pill">NEW</span></div>
              <div class="settings-sub">Open Agent chats as editor tabs instead of only the sidebar panel.</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleChatTabs" checked /><span class="slider"></span></label>
          </div>
        </div>
        <p class="workspace-hint" id="topWorkspaceLabel">No workspace folder open</p>
      </section>

      <!-- TAB: PLAN & USAGE -->
      <section class="tab-pane" id="pane-plan">
        <div class="pane-header">
          <h1 class="pane-title">Plan &amp; Usage</h1>
          <p class="pane-desc">Local BYOK usage stays on your machine. Track sidecar readiness and connected providers.</p>
        </div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Current Plan</div>
              <div class="settings-sub" id="planUsageSub">Pro+ · Bring Your Own Key</div>
            </div>
            <span class="badge badge-ok" id="planBadge">Pro+</span>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Provider Keys Connected</div>
              <div class="settings-sub">Gemini / Anthropic / OpenAI keychain status</div>
            </div>
            <span class="badge badge-violet" id="planKeysBadge">0/3</span>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Manage API Keys</div>
              <div class="settings-sub">Jump to Models to update OS Keychain credentials</div>
            </div>
            <button type="button" class="btn btn-primary" data-goto="models">Open Models</button>
          </div>
        </div>
      </section>

      <!-- TAB: AGENTS -->
      <section class="tab-pane" id="pane-agents">
        <div class="pane-header">
          <h1 class="pane-title">Agents</h1>
          <p class="pane-desc">Execution permissions, verification loops, and agent runtime defaults.</p>
        </div>
        <div class="card">
          <div class="field">
            <label>
              <span>Agent Permission Profile</span>
              <span class="mono" style="color: var(--obs-muted);">rursor.agent.permissionProfile</span>
            </label>
            <div class="profile-grid" id="permissionProfileGrid">
              <div class="profile-card selected" data-profile="safe-auto">
                <div class="profile-name">safe-auto</div>
                <div class="profile-desc">Auto-runs reads &amp; search; prompts before mutations or shell.</div>
              </div>
              <div class="profile-card" data-profile="read-only">
                <div class="profile-name">read-only</div>
                <div class="profile-desc">Exploration only; blocks file writes and command execution.</div>
              </div>
              <div class="profile-card" data-profile="full-auto">
                <div class="profile-name">full-auto</div>
                <div class="profile-desc">Autonomous multi-file edits and self-healing verify loops.</div>
              </div>
            </div>
          </div>
          <div class="btn-row" style="margin-top: 12px;">
            <button type="button" class="btn btn-primary" id="saveAgentProfileBtn">Save Permission Profile</button>
          </div>
        </div>
        <div class="card">
          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Prompt Context Caching</div>
              <div class="toggle-sub">Cache static rules and memory prefixes on Gemini turns.</div>
            </div>
            <label class="switch"><input type="checkbox" id="togglePromptCache" checked /><span class="slider"></span></label>
          </div>
          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Auto-Verify Agent Edits</div>
              <div class="toggle-sub">Run detected build/test commands after edits and self-correct failures.</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleAutoVerify" /><span class="slider"></span></label>
          </div>
        </div>
      </section>

      <!-- TAB: CLOUD AGENTS -->
      <section class="tab-pane" id="pane-cloud">
        <div class="pane-header">
          <h1 class="pane-title">Cloud Agents</h1>
          <p class="pane-desc">Launch isolated cloud workers for long-running tasks (Mission Control / Swarm).</p>
        </div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Mission Control</div>
              <div class="settings-sub">Open multi-agent Swarm and Model Arena</div>
            </div>
            <button type="button" class="btn btn-primary" data-cmd="rursor.missionControl">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">APFS CoW Worker Clones</div>
              <div class="settings-sub">Instant zero-copy sandboxes for parallel cloud-style workers on macOS</div>
            </div>
            <button type="button" class="btn" data-cmd="rursor.manageCowClones">Manage</button>
          </div>
        </div>
      </section>

      <!-- TAB: MODELS -->
      <section class="tab-pane" id="pane-models">
        <div class="pane-header">
          <h1 class="pane-title">Models</h1>
          <p class="pane-desc">Manage OS Keychain BYOK credentials, Smart Router defaults, and reasoning budgets.</p>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Provider API Keys (OS Keychain)</h2>
              <p class="card-subtitle">Stored encrypted via VS Code SecretStorage (<span class="mono">context.secrets</span>) — never written to plaintext settings.</p>
            </div>
            <span class="badge badge-violet">Keychain Encrypted</span>
          </div>

          <!-- Gemini -->
          <div class="key-row">
            <div class="key-meta">
              <div class="key-title">
                <span>Google Gemini API Key</span>
                <span class="mono" style="color: var(--obs-muted);">rursor.geminiApiKey</span>
              </div>
              <span id="badge-gemini" class="badge badge-warn">Unset</span>
            </div>
            <div class="key-controls">
              <input type="password" id="input-gemini" placeholder="AIzaSy... (enter new Gemini key)" autocomplete="off" />
              <button type="button" class="btn btn-primary" data-action="save-key" data-provider="gemini">Save</button>
              <button type="button" class="btn btn-danger" data-action="clear-key" data-provider="gemini">Clear</button>
            </div>
          </div>

          <!-- Anthropic -->
          <div class="key-row">
            <div class="key-meta">
              <div class="key-title">
                <span>Anthropic API Key</span>
                <span class="mono" style="color: var(--obs-muted);">rursor.anthropicApiKey</span>
              </div>
              <span id="badge-anthropic" class="badge badge-warn">Unset</span>
            </div>
            <div class="key-controls">
              <input type="password" id="input-anthropic" placeholder="sk-ant-... (optional BYOK)" autocomplete="off" />
              <button type="button" class="btn btn-primary" data-action="save-key" data-provider="anthropic">Save</button>
              <button type="button" class="btn btn-danger" data-action="clear-key" data-provider="anthropic">Clear</button>
            </div>
          </div>

          <!-- OpenAI -->
          <div class="key-row">
            <div class="key-meta">
              <div class="key-title">
                <span>OpenAI API Key</span>
                <span class="mono" style="color: var(--obs-muted);">rursor.openaiApiKey</span>
              </div>
              <span id="badge-openai" class="badge badge-warn">Unset</span>
            </div>
            <div class="key-controls">
              <input type="password" id="input-openai" placeholder="sk-proj-... (optional BYOK)" autocomplete="off" />
              <button type="button" class="btn btn-primary" data-action="save-key" data-provider="openai">Save</button>
              <button type="button" class="btn btn-danger" data-action="clear-key" data-provider="openai">Clear</button>
            </div>
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Models &amp; Smart Router</h2>
              <p class="card-subtitle">Route tasks automatically by complexity or pin a specific Gemini, Claude, OpenAI, or Local model.</p>
            </div>
          </div>

          <div class="field">
            <label for="defaultModelSelect">
              <span>Default Agent &amp; Inline Edit Model</span>
              <span class="mono" style="color: var(--obs-muted);">rursor.defaultModel</span>
            </label>
            <select id="defaultModelSelect">
              <option value="auto">Auto (Smart Router — Dynamic Complexity + BYOK Fallback)</option>
              <option value="gemini-3.8-flash">gemini-3.8-flash (Fast Default)</option>
              <option value="gemini-3.1-pro-preview">gemini-3.1-pro-preview (Deep Architecture &amp; Reasoning)</option>
              <option value="gemini-3.5-flash-lite">gemini-3.5-flash-lite (Ultra Low Latency)</option>
              <option value="claude-sonnet-4-20250514">claude-sonnet-4-20250514 (Anthropic Deep)</option>
              <option value="claude-3-5-haiku-latest">claude-3-5-haiku-latest (Anthropic Fast)</option>
              <option value="gpt-4.1">gpt-4.1 (OpenAI Flagship)</option>
              <option value="gpt-4o">gpt-4o (OpenAI Multimodal)</option>
              <option value="gpt-4o-mini">gpt-4o-mini (OpenAI Fast)</option>
              <option value="o3-mini">o3-mini (OpenAI Reasoning)</option>
              <option value="o4-mini">o4-mini (OpenAI Fast Reasoning)</option>
            </select>
          </div>

          <div class="field">
            <label for="thinkingBudgetNumber">
              <span>Thinking Token Budget</span>
              <span class="mono" id="thinkingBudgetLabel" style="color: var(--obs-cyan);">0 (Automatic)</span>
            </label>
            <div class="budget-wrap">
              <input type="range" id="thinkingBudgetRange" min="0" max="32768" step="1024" value="0" />
              <input type="number" id="thinkingBudgetNumber" min="0" max="32768" step="512" value="0" style="width: 105px;" />
            </div>
            <div class="preset-pills" id="budgetPresets">
              <button type="button" class="preset-pill" data-budget="0">Auto (0)</button>
              <button type="button" class="preset-pill" data-budget="4096">4K</button>
              <button type="button" class="preset-pill" data-budget="8192">8K</button>
              <button type="button" class="preset-pill" data-budget="16384">16K</button>
              <button type="button" class="preset-pill" data-budget="24576">24K</button>
            </div>
          </div>

          <div class="btn-row" style="margin-top: 16px;">
            <button type="button" class="btn btn-primary" id="saveModelSettingsBtn">Save Model &amp; Router Settings</button>
          </div>
        </div>
      </section>

      <!-- TAB: VOICE -->
      <section class="tab-pane" id="pane-voice">
        <div class="pane-header">
          <h1 class="pane-title">Voice</h1>
          <p class="pane-desc">Dictate prompts into Rursor Agent with local or cloud transcription.</p>
        </div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Voice Capture</div>
              <div class="settings-sub">Start a voice prompt that fills the Agent composer</div>
            </div>
            <button type="button" class="btn btn-primary" data-cmd="rursor.voicePrompt">Start</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Telemetry Dashboard</div>
              <div class="settings-sub">Inspect turn latency and token estimates for Agent / Ask / Voice</div>
            </div>
            <button type="button" class="btn" data-cmd="rursor.showTelemetry">Open</button>
          </div>
        </div>
      </section>

      <!-- TAB: CODE INTEL -->
      <section class="tab-pane" id="pane-codeintel">
        <div class="pane-header">
          <h1 class="pane-title">Tab &amp; Next-Edit Prediction (NES)</h1>
          <p class="pane-desc">Configure low-latency ghost-text completions, multi-line Next-Edit Predictions, Tab-to-Jump, and local Ollama / MLX engines.</p>
        </div>

        <div class="card">
          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Enable Rursor Tab Autocomplete</div>
              <div class="toggle-sub">Stream speculative FIM ghost-text completions and word-by-word acceptance (<span class="mono">Cmd+Right</span>).</div>
            </div>
            <label class="switch">
              <input type="checkbox" id="tabEnabledToggle" checked />
              <span class="slider"></span>
            </label>
          </div>

          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Enable Next-Edit Prediction (NES) &amp; Tab-to-Jump</div>
              <div class="toggle-sub">Predict follow-up multi-line edits and cursor jump targets across related symbols (<span class="mono">Tab</span> to accept/jump).</div>
            </div>
            <label class="switch">
              <input type="checkbox" id="nesEnabledToggle" checked />
              <span class="slider"></span>
            </label>
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Completion Engine &amp; Local LLM Backend</h2>
              <p class="card-subtitle">Run completions via Gemini Flash-Lite in the cloud or 100% locally with Ollama / Apple Silicon MLX.</p>
            </div>
          </div>

          <div class="field-grid">
            <div class="field">
              <label for="tabBackendSelect">Autocomplete Backend</label>
              <select id="tabBackendSelect">
                <option value="gemini">gemini (Gemini Cloud Flash-Lite)</option>
                <option value="auto">auto (Prefer Local, Fallback to Cloud)</option>
                <option value="ollama">ollama (Local Ollama Server)</option>
                <option value="mlx">mlx (Apple Silicon MLX Server)</option>
              </select>
            </div>

            <div class="field">
              <label for="tabCloudModelInput">Cloud Autocomplete Model</label>
              <input type="text" id="tabCloudModelInput" value="gemini-3.5-flash-lite" placeholder="gemini-3.5-flash-lite" />
            </div>

            <div class="field">
              <label for="tabLocalEndpointInput">Local Endpoint URL</label>
              <input type="text" id="tabLocalEndpointInput" value="http://127.0.0.1:11434" placeholder="http://127.0.0.1:11434" />
            </div>

            <div class="field">
              <label for="tabLocalModelInput">Local FIM / Coder Model</label>
              <input type="text" id="tabLocalModelInput" value="qwen2.5-coder:7b" placeholder="qwen2.5-coder:7b" />
            </div>
          </div>

          <div class="btn-row" style="margin-top: 10px;">
            <button type="button" class="btn btn-primary" id="saveTabSettingsBtn">Save Tab &amp; NES Settings</button>
          </div>
        </div>
      </section>

      <!-- TAB: CUSTOMIZE -->
      <section class="tab-pane" id="pane-customize">
        <div class="pane-header">
          <h1 class="pane-title">Workspace Rules &amp; Memory</h1>
          <p class="pane-desc">Edit <span class="mono">.rursorrules</span> live on disk, manage reusable <span class="mono">/skill</span> workflows, and inspect persistent agent memory.</p>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Workspace Rules (<span class="mono">.rursorrules</span>)</h2>
              <p class="card-subtitle" id="rulesPathSubtitle">Injected into every Agent and Inline Edit system prompt.</p>
            </div>
            <span id="rulesActiveBadge" class="badge badge-warn">Unset</span>
          </div>

          <div class="field">
            <textarea id="rursorRulesTextarea" class="code-area" placeholder="# Write project-specific architectural guidelines, coding conventions, and test commands..."></textarea>
          </div>

          <div class="btn-row">
            <button type="button" class="btn btn-primary" id="saveRulesBtn">Save .rursorrules &amp; Reload Sidecar</button>
            <button type="button" class="btn btn-violet" id="insertStarterRulesBtn">Insert Starter Template</button>
            <button type="button" class="btn" id="openRulesEditorBtn">Open in Editor</button>
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Workspace Skills (<span class="mono">.rursor/skills/*.md</span>)</h2>
              <p class="card-subtitle">Invoke in Agent chat with <span class="mono">/skill-name</span>. Built-in skills (<span class="mono">/review</span>, <span class="mono">/test</span>, <span class="mono">/fix</span>, <span class="mono">/commit</span>, <span class="mono">/explain</span>) are always available.</p>
            </div>
            <span class="badge badge-violet" id="skillsCountBadge">0 total</span>
          </div>

          <div class="key-controls" style="margin-bottom: 12px;">
            <input type="text" id="newSkillNameInput" placeholder="New skill slug (e.g. security-audit, migrate, perf)" />
            <button type="button" class="btn btn-primary" id="createSkillBtn">+ Create Skill Template</button>
          </div>

          <div id="skillsListContainer"></div>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Persistent Memory (<span class="mono">.rursor/memory.json</span>)</h2>
              <p class="card-subtitle">Facts and preferences remembered across Agent sessions.</p>
            </div>
            <button type="button" class="btn" id="openMemoryJsonBtn">Open .rursor/memory.json</button>
          </div>

          <div class="key-controls" style="margin-bottom: 12px;">
            <select id="newMemoryScope" style="width: 125px;">
              <option value="workspace">workspace</option>
              <option value="global">global</option>
            </select>
            <input type="text" id="newMemoryCategory" placeholder="category (e.g. arch)" style="width: 140px; flex: 0 0 auto;" />
            <input type="text" id="newMemoryContent" placeholder="Add a persistent fact or preference..." />
            <button type="button" class="btn btn-primary" id="addMemoryBtn">+ Add</button>
          </div>

          <div id="memoriesListContainer"></div>

          <details style="margin-top: 10px;">
            <summary style="cursor: pointer; font-size: 12px; color: var(--obs-muted); font-weight: 600;">Raw .rursor/memory.json Preview</summary>
            <pre class="json-viewer" id="rawMemoryViewer">[]</pre>
          </details>
        </div>
      </section>

      <!-- TAB 4: MCP SERVERS -->
      <section class="tab-pane" id="pane-mcp">
        <div class="pane-header">
          <h1 class="pane-title">Model Context Protocol (MCP) Servers</h1>
          <p class="pane-desc">Connect external tools, databases, issue trackers, and documentation servers via stdio JSON-RPC.</p>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Configured MCP Servers</h2>
              <p class="card-subtitle" id="mcpPathSubtitle">Loaded from <span class="mono">.rursor/mcp.json</span> and <span class="mono">~/.rursor/mcp.json</span>.</p>
            </div>
            <div class="btn-row">
              <button type="button" class="btn" id="reloadMcpBtn">↻ Reload MCP</button>
              <button type="button" class="btn btn-primary" id="editMcpJsonBtn">Edit .rursor/mcp.json</button>
            </div>
          </div>

          <div id="mcpTableContainer"></div>
        </div>
      </section>

      <!-- TAB: SOURCE CONTROL -->
      <section class="tab-pane" id="pane-source">
        <div class="pane-header">
          <h1 class="pane-title">Codebase Index &amp; Sandbox</h1>
          <p class="pane-desc">Monitor Rust sidecar BM25 + AST symbol indexing, PTY execution, checkpoints, and isolated Git worktrees.</p>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Live Workspace Index</h2>
              <p class="card-subtitle" id="indexWorkspacePath">No workspace folder open</p>
            </div>
            <button type="button" class="btn btn-primary" id="reindexBtn">⚡ Reindex Workspace</button>
          </div>

          <div class="stat-grid">
            <div class="stat-box">
              <div class="stat-value" id="statIndexedFiles">0</div>
              <div class="stat-label">Indexed Files</div>
            </div>
            <div class="stat-box">
              <div class="stat-value" id="statSkills">0</div>
              <div class="stat-label">Active Skills</div>
            </div>
            <div class="stat-box">
              <div class="stat-value" id="statMemories">0</div>
              <div class="stat-label">Saved Memories</div>
            </div>
            <div class="stat-box">
              <div class="stat-value" id="statBgProcs">0</div>
              <div class="stat-label">Background PTYs</div>
            </div>
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <div>
              <h2 class="card-title">Execution Sandbox &amp; Recovery</h2>
              <p class="card-subtitle">Isolate parallel agent tasks with Git worktrees and instant pre-turn file checkpoints.</p>
            </div>
          </div>

          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Real PTY Shell Runner (<span class="mono">portable-pty</span>)</div>
              <div class="toggle-sub">Execute Agent terminal commands inside a true pseudo-terminal in the Rust sidecar (<span class="mono">rursor.agent.usePty</span>).</div>
            </div>
            <label class="switch">
              <input type="checkbox" id="toggleUsePty" checked />
              <span class="slider"></span>
            </label>
          </div>

          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Git Worktree Sandboxes (<span id="worktreeCountLabel">0</span>)</div>
              <div class="toggle-sub">Create, diff, merge, or clean up isolated worktree branches for parallel agent runs.</div>
            </div>
            <button type="button" class="btn btn-violet" id="manageWorktreesBtn">Manage Worktrees...</button>
          </div>

          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Agent Shadow Checkpoints (<span id="checkpointCountLabel">0</span>)</div>
              <div class="toggle-sub">Restore workspace files to any pre-edit snapshot captured automatically before Agent mutations.</div>
            </div>
            <button type="button" class="btn" id="restoreCheckpointBtn">Restore Checkpoint...</button>
          </div>
        </div>
      </section>

      <!-- TAB: BROWSER & NETWORK -->
      <section class="tab-pane" id="pane-browser">
        <div class="pane-header">
          <h1 class="pane-title">Browser &amp; Network</h1>
          <p class="pane-desc">Chromium CDP computer-use studio and sandbox network policy.</p>
        </div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Browser Studio</div>
              <div class="settings-sub">Native Chromium CDP with Set-of-Mark actions and walkthrough video proof</div>
            </div>
            <button type="button" class="btn btn-primary" data-cmd="rursor.openBrowserStudio">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Sandbox Network Access</div>
              <div class="settings-sub">Allow outbound network inside macOS Seatbelt sandboxed Agent commands</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleSandboxNetwork" /><span class="slider"></span></label>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Web Preview</div>
              <div class="settings-sub">Inspect localhost pages from Agent tool calls</div>
            </div>
            <button type="button" class="btn" data-cmd="rursor.openPreview">Open</button>
          </div>
        </div>
      </section>

      <!-- TAB: BETA -->
      <section class="tab-pane" id="pane-beta">
        <div class="pane-header">
          <h1 class="pane-title">Beta</h1>
          <p class="pane-desc">Experimental Rursor surfaces that ship ahead of stable defaults.</p>
        </div>
        <div class="settings-group">
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Infinite Design Canvas</div>
              <div class="settings-sub">Design-to-code canvas for interactive UI exploration</div>
            </div>
            <button type="button" class="btn" data-cmd="rursor.openCanvas">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Artifact Studio</div>
              <div class="settings-sub">Decks, explainer video, Office docs, and GenUI widgets</div>
            </div>
            <button type="button" class="btn" data-cmd="rursor.openArtifactStudio">Open</button>
          </div>
          <div class="settings-row">
            <div class="settings-copy">
              <div class="settings-title">Open Chat as Editor Tabs</div>
              <div class="settings-sub">Same as General → Layout (beta default on)</div>
            </div>
            <label class="switch"><input type="checkbox" id="toggleChatTabsBeta" checked /><span class="slider"></span></label>
          </div>
        </div>
      </section>
    </main>
  </div>

  <div id="toast" class="toast info" hidden></div>

  <script nonce="${nonce}">
    (function () {
      const vscode = acquireVsCodeApi();
      let currentPermissionProfile = 'safe-auto';
      let toastTimer = null;
      let rulesDirty = false;

      function el(id) {
        return document.getElementById(id);
      }

      function escapeHtml(str) {
        return String(str || '')
          .replace(/&/g, '&amp;')
          .replace(/</g, '&lt;')
          .replace(/>/g, '&gt;')
          .replace(/"/g, '&quot;')
          .replace(/'/g, '&#39;');
      }

      function showToast(message, kind) {
        const node = el('toast');
        if (!node) return;
        node.textContent = message;
        node.className = 'toast ' + (kind || 'success');
        node.hidden = false;
        if (toastTimer) clearTimeout(toastTimer);
        toastTimer = setTimeout(function () {
          node.hidden = true;
        }, 3200);
      }

      function activateTab(target) {
        if (!target) return;
        document.querySelectorAll('.nav-btn[data-tab]').forEach(function (b) {
          b.classList.toggle('active', b.getAttribute('data-tab') === target);
        });
        document.querySelectorAll('.tab-pane').forEach(function (pane) {
          pane.classList.toggle('active', pane.id === 'pane-' + target);
        });
      }

      document.querySelectorAll('.nav-btn[data-tab]').forEach(function (btn) {
        btn.addEventListener('click', function () {
          activateTab(btn.getAttribute('data-tab'));
        });
      });

      el('refreshAllBtn').addEventListener('click', function () {
        rulesDirty = false;
        vscode.postMessage({ type: 'refresh' });
      });

      const searchEl = el('settingsSearch');
      if (searchEl) {
        searchEl.addEventListener('input', function (e) {
          const q = String(e.target.value || '').trim().toLowerCase();
          document.querySelectorAll('.nav-btn[data-tab]').forEach(function (btn) {
            const label = (btn.textContent || '').toLowerCase();
            btn.style.display = !q || label.indexOf(q) >= 0 ? '' : 'none';
          });
        });
        window.addEventListener('keydown', function (e) {
          if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f') {
            e.preventDefault();
            searchEl.focus();
          }
        });
      }

      // Delegated Clicks (API Keys, Skill Open, Memory Delete, Budget Presets, nav actions)
      document.addEventListener('click', function (ev) {
        const gotoBtn = ev.target.closest('[data-goto]');
        if (gotoBtn) {
          activateTab(gotoBtn.getAttribute('data-goto'));
          return;
        }
        const cmdBtn = ev.target.closest('[data-cmd]');
        if (cmdBtn) {
          vscode.postMessage({ type: 'runCommand', command: cmdBtn.getAttribute('data-cmd') });
          return;
        }

        const target = ev.target.closest('[data-action], [data-budget], [data-profile]');
        if (!target) return;

        if (target.hasAttribute('data-budget')) {
          const val = Number(target.getAttribute('data-budget') || '0');
          syncBudgetInputs(val);
          return;
        }

        if (target.hasAttribute('data-profile')) {
          selectPermissionProfile(target.getAttribute('data-profile') || 'safe-auto');
          return;
        }

        const action = target.getAttribute('data-action');
        if (action === 'open-vscode-settings') {
          vscode.postMessage({ type: 'openVscodeSettings' });
        } else if (action === 'open-docs') {
          vscode.postMessage({ type: 'runCommand', command: 'rursor.openSettings' });
          showToast('See README.md in the Rursor repo for full docs.', 'info');
        } else if (action === 'save-key') {
          const provider = target.getAttribute('data-provider');
          const input = el('input-' + provider);
          vscode.postMessage({
            type: 'saveApiKey',
            provider: provider,
            value: input ? input.value : '',
          });
          if (input) input.value = '';
        } else if (action === 'clear-key') {
          const provider = target.getAttribute('data-provider');
          const input = el('input-' + provider);
          if (input) input.value = '';
          vscode.postMessage({
            type: 'clearApiKey',
            provider: provider,
          });
        } else if (action === 'open-skill') {
          vscode.postMessage({
            type: 'openSkillFile',
            filePath: target.getAttribute('data-path'),
          });
        } else if (action === 'delete-memory') {
          vscode.postMessage({
            type: 'deleteMemory',
            memoryId: target.getAttribute('data-id'),
          });
        }
      });

      function syncBudgetInputs(val) {
        const clamped = Math.max(0, Math.min(32768, Math.round(Number(val) || 0)));
        el('thinkingBudgetRange').value = String(clamped);
        el('thinkingBudgetNumber').value = String(clamped);
        el('thinkingBudgetLabel').textContent =
          clamped === 0 ? '0 (Automatic)' : clamped.toLocaleString() + ' tokens';
      }

      el('thinkingBudgetRange').addEventListener('input', function (e) {
        syncBudgetInputs(e.target.value);
      });
      el('thinkingBudgetNumber').addEventListener('input', function (e) {
        syncBudgetInputs(e.target.value);
      });

      function selectPermissionProfile(profile) {
        currentPermissionProfile = profile;
        document.querySelectorAll('#permissionProfileGrid .profile-card').forEach(function (card) {
          card.classList.toggle('selected', card.getAttribute('data-profile') === profile);
        });
      }

      el('saveModelSettingsBtn').addEventListener('click', function () {
        vscode.postMessage({
          type: 'saveModelSettings',
          defaultModel: el('defaultModelSelect').value,
          thinkingBudget: Number(el('thinkingBudgetNumber').value || '0'),
          permissionProfile: currentPermissionProfile,
        });
      });

      el('saveAgentProfileBtn').addEventListener('click', function () {
        vscode.postMessage({
          type: 'saveModelSettings',
          defaultModel: el('defaultModelSelect').value,
          thinkingBudget: Number(el('thinkingBudgetNumber').value || '0'),
          permissionProfile: currentPermissionProfile,
        });
      });

      function bindToggle(id, key) {
        const node = el(id);
        if (!node) return;
        node.addEventListener('change', function (e) {
          vscode.postMessage({
            type: 'toggleSetting',
            key: key,
            boolValue: Boolean(e.target.checked),
          });
        });
      }

      bindToggle('togglePromptCache', 'agent.promptCache');
      bindToggle('toggleAutoVerify', 'agent.autoVerify');
      bindToggle('toggleUsePty', 'agent.usePty');
      bindToggle('toggleTitleBar', 'layout.showTitleBar');
      bindToggle('toggleStatusBar', 'layout.showStatusBar');
      bindToggle('toggleAutoHideEditor', 'layout.autoHideEditorWhenEmpty');
      bindToggle('toggleChatTabs', 'layout.openChatAsEditorTabs');
      bindToggle('toggleChatTabsBeta', 'layout.openChatAsEditorTabs');
      bindToggle('toggleSandboxNetwork', 'agent.sandboxAllowNetwork');

      el('conversationDensitySelect').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'layout.conversationDensity',
          value: e.target.value,
        });
      });
      el('reviewControlSelect').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'layout.reviewControlLocation',
          value: e.target.value,
        });
      });

      document.querySelectorAll('#windowLayoutPicker .layout-card').forEach(function (card) {
        card.addEventListener('click', function () {
          const layout = card.getAttribute('data-layout') || 'editor';
          document.querySelectorAll('#windowLayoutPicker .layout-card').forEach(function (c) {
            c.classList.toggle('selected', c === card);
          });
          vscode.postMessage({
            type: 'toggleSetting',
            key: 'layout.windowLayout',
            value: layout,
          });
        });
      });

      el('dismissBannerBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'dismissCustomizeBanner' });
      });
      el('openAccountBtn').addEventListener('click', function () {
        activateTab('plan');
      });
      el('openEditorSettingsBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'openEditorSettings' });
      });
      el('openKeysBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'openKeyboardShortcuts' });
      });
      el('importVsCodeBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'importFromVsCode' });
      });
      el('resetDontAskBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'resetDontAskAgain' });
      });

      el('tabEnabledToggle').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'autocomplete.enabled',
          boolValue: Boolean(e.target.checked),
        });
      });

      el('nesEnabledToggle').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'autocomplete.nesEnabled',
          boolValue: Boolean(e.target.checked),
        });
      });

      el('saveTabSettingsBtn').addEventListener('click', function () {
        vscode.postMessage({
          type: 'saveTabSettings',
          enabled: Boolean(el('tabEnabledToggle').checked),
          nesEnabled: Boolean(el('nesEnabledToggle').checked),
          backend: el('tabBackendSelect').value,
          autocompleteModel: el('tabCloudModelInput').value,
          localEndpoint: el('tabLocalEndpointInput').value,
          localModel: el('tabLocalModelInput').value,
        });
      });

      // Rules & Memory
      el('rursorRulesTextarea').addEventListener('input', function () {
        rulesDirty = true;
      });

      el('saveRulesBtn').addEventListener('click', function () {
        rulesDirty = false;
        vscode.postMessage({
          type: 'saveRules',
          rulesContent: el('rursorRulesTextarea').value,
        });
      });

      el('insertStarterRulesBtn').addEventListener('click', function () {
        const area = el('rursorRulesTextarea');
        const starter =
          '# Rursor Workspace Engineering Rules\\n\\n' +
          '- Preserve strict type safety and idiomatic error handling across all edits.\\n' +
          '- Prefer minimal, surgical changes that respect existing module boundaries.\\n' +
          '- Add or update focused unit tests alongside behavioral changes.\\n' +
          '- Never hardcode secrets, tokens, or environment-specific absolute paths.\\n';
        area.value = area.value.trim() ? area.value + '\\n\\n' + starter : starter;
        rulesDirty = true;
      });

      el('openRulesEditorBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'openRulesInEditor' });
      });

      el('createSkillBtn').addEventListener('click', function () {
        const input = el('newSkillNameInput');
        vscode.postMessage({
          type: 'createSkillTemplate',
          skillName: input.value,
        });
        input.value = '';
      });

      el('addMemoryBtn').addEventListener('click', function () {
        const contentInput = el('newMemoryContent');
        if (!contentInput.value.trim()) return;
        vscode.postMessage({
          type: 'addMemory',
          memoryScope: el('newMemoryScope').value,
          memoryCategory: el('newMemoryCategory').value || 'general',
          memoryContent: contentInput.value,
        });
        contentInput.value = '';
      });

      el('openMemoryJsonBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'openMemoryJson' });
      });

      // MCP
      el('reloadMcpBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'reloadMcp' });
      });

      el('editMcpJsonBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'editMcpJson' });
      });

      // Index & Sandbox
      el('reindexBtn').addEventListener('click', function () {
        el('reindexBtn').textContent = 'Indexing...';
        vscode.postMessage({ type: 'reindexWorkspace' });
      });

      el('manageWorktreesBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'manageWorktrees' });
      });

      el('restoreCheckpointBtn').addEventListener('click', function () {
        vscode.postMessage({ type: 'restoreCheckpoint' });
      });

      function renderKeyStatus(provider, info) {
        const badge = el('badge-' + provider);
        if (!badge) return;
        if (info.hasKeychainKey) {
          badge.className = 'badge badge-ok';
          badge.textContent = '● Connected (' + info.maskedPreview + ')';
        } else if (info.hasSidecarKey) {
          badge.className = 'badge badge-ok';
          badge.textContent = '● Connected (ENV)';
        } else {
          badge.className = 'badge badge-warn';
          badge.textContent = 'Unset';
        }
      }

      function applyState(state) {
        // Header & Rail Badges
        const sidecarBadge = el('sidecarStatusBadge');
        if (state.sidecarReady) {
          sidecarBadge.className = 'badge badge-ok';
          sidecarBadge.textContent = '● Sidecar Ready';
        } else {
          sidecarBadge.className = 'badge badge-err';
          sidecarBadge.textContent = '● Sidecar Offline';
        }

        el('topWorkspaceLabel').textContent = state.workspaceRoot
          ? state.workspaceRoot
          : 'No workspace folder open';
        if (el('accountEmailLabel')) el('accountEmailLabel').textContent = state.accountEmail || 'local';
        if (el('accountPlanLabel')) el('accountPlanLabel').textContent = state.accountPlan || 'Pro+';
        if (el('planBadge')) el('planBadge').textContent = state.accountPlan || 'Pro+';

        const connectedKeysCount = ['gemini', 'anthropic', 'openai'].filter(function (p) {
          return state.keys[p].hasKeychainKey || state.keys[p].hasSidecarKey;
        }).length;
        el('navKeysBadge').textContent = connectedKeysCount + '/3';
        if (el('planKeysBadge')) el('planKeysBadge').textContent = connectedKeysCount + '/3';
        el('navTabBadge').textContent = state.tab.enabled ? 'On' : 'Off';
        el('navRulesBadge').textContent = String(
          (state.rulesAndMemory.hasWorkspaceRules ? 1 : 0) +
            state.rulesAndMemory.workspaceSkills.length
        );
        el('navMcpBadge').textContent = String(state.mcp.servers.length);
        el('navIndexBadge').textContent = String(state.indexAndSandbox.indexedFiles);

        // Layout (General)
        if (state.layout) {
          const banner = el('customizeBanner');
          if (banner) banner.hidden = Boolean(state.layout.customizeBannerDismissed);
          document.querySelectorAll('#windowLayoutPicker .layout-card').forEach(function (card) {
            card.classList.toggle('selected', card.getAttribute('data-layout') === state.layout.windowLayout);
          });
          if (el('conversationDensitySelect')) el('conversationDensitySelect').value = state.layout.conversationDensity;
          if (el('reviewControlSelect')) el('reviewControlSelect').value = state.layout.reviewControlLocation;
          if (el('toggleTitleBar')) el('toggleTitleBar').checked = Boolean(state.layout.showTitleBar);
          if (el('toggleStatusBar')) el('toggleStatusBar').checked = Boolean(state.layout.showStatusBar);
          if (el('toggleAutoHideEditor')) el('toggleAutoHideEditor').checked = Boolean(state.layout.autoHideEditorWhenEmpty);
          if (el('toggleChatTabs')) el('toggleChatTabs').checked = Boolean(state.layout.openChatAsEditorTabs);
          if (el('toggleChatTabsBeta')) el('toggleChatTabsBeta').checked = Boolean(state.layout.openChatAsEditorTabs);
        }

        // Keys
        renderKeyStatus('gemini', state.keys.gemini);
        renderKeyStatus('anthropic', state.keys.anthropic);
        renderKeyStatus('openai', state.keys.openai);

        // Models
        const modelSelect = el('defaultModelSelect');
        const hasOption = Array.from(modelSelect.options).some(function (o) {
          return o.value === state.models.defaultModel;
        });
        if (!hasOption && state.models.defaultModel) {
          const customOpt = document.createElement('option');
          customOpt.value = state.models.defaultModel;
          customOpt.textContent = state.models.defaultModel + ' (Custom)';
          modelSelect.appendChild(customOpt);
        }
        modelSelect.value = state.models.defaultModel;
        syncBudgetInputs(state.models.thinkingBudget);
        selectPermissionProfile(state.models.permissionProfile);
        el('togglePromptCache').checked = Boolean(state.models.promptCache);
        el('toggleAutoVerify').checked = Boolean(state.models.autoVerify);

        // Tab & NES
        el('tabEnabledToggle').checked = Boolean(state.tab.enabled);
        el('nesEnabledToggle').checked = Boolean(state.tab.nesEnabled);
        el('tabBackendSelect').value = state.tab.backend;
        el('tabCloudModelInput').value = state.tab.autocompleteModel;
        el('tabLocalEndpointInput').value = state.tab.localEndpoint;
        el('tabLocalModelInput').value = state.tab.localModel;

        // Rules & Memory
        const rulesBadge = el('rulesActiveBadge');
        if (state.rulesAndMemory.hasWorkspaceRules) {
          rulesBadge.className = 'badge badge-ok';
          rulesBadge.textContent = '● Active';
        } else {
          rulesBadge.className = 'badge badge-warn';
          rulesBadge.textContent = 'Empty';
        }
        el('rulesPathSubtitle').textContent = state.rulesAndMemory.rursorRulesPath
          ? state.rulesAndMemory.rursorRulesPath
          : 'Open a workspace folder to edit .rursorrules';
        if (!rulesDirty) {
          el('rursorRulesTextarea').value = state.rulesAndMemory.rursorRulesContent || '';
        }

        el('skillsCountBadge').textContent = state.rulesAndMemory.allSkillsCount + ' available';
        const skillsBox = el('skillsListContainer');
        if (state.rulesAndMemory.workspaceSkills.length === 0) {
          skillsBox.innerHTML =
            '<div style="color: var(--obs-muted); font-size: 12px; padding: 6px 2px;">' +
            'No custom <span class="mono">.rursor/skills/*.md</span> files yet. Create a template above to add a project slash skill.' +
            '</div>';
        } else {
          skillsBox.innerHTML = state.rulesAndMemory.workspaceSkills
            .map(function (sk) {
              return (
                '<div class="list-item">' +
                '<div>' +
                '<div style="font-weight: 600; color: #F8FAFC;">/' +
                escapeHtml(sk.name) +
                ' <span class="mono" style="color: var(--obs-muted); font-weight: 400;">(' +
                escapeHtml(sk.fileName || '') +
                ')</span></div>' +
                '<div style="font-size: 11.5px; color: var(--obs-muted);">' +
                escapeHtml(sk.description) +
                '</div>' +
                '</div>' +
                '<button type="button" class="btn" data-action="open-skill" data-path="' +
                escapeHtml(sk.absPath || '') +
                '">Open</button>' +
                '</div>'
              );
            })
            .join('');
        }

        const memBox = el('memoriesListContainer');
        if (state.rulesAndMemory.memories.length === 0) {
          memBox.innerHTML =
            '<div style="color: var(--obs-muted); font-size: 12px; padding: 4px 2px;">' +
            'No persistent memories recorded yet.' +
            '</div>';
        } else {
          memBox.innerHTML = state.rulesAndMemory.memories
            .map(function (m) {
              return (
                '<div class="list-item">' +
                '<div>' +
                '<div style="display: flex; gap: 6px; align-items: center; margin-bottom: 3px;">' +
                '<span class="badge badge-violet">' +
                escapeHtml(m.scope) +
                '</span>' +
                '<span class="mono" style="color: var(--obs-cyan);">' +
                escapeHtml(m.category) +
                '</span>' +
                '</div>' +
                '<div style="font-size: 12.5px; color: #E2E8F0;">' +
                escapeHtml(m.content) +
                '</div>' +
                '</div>' +
                '<button type="button" class="btn btn-danger" data-action="delete-memory" data-id="' +
                escapeHtml(m.id) +
                '">Delete</button>' +
                '</div>'
              );
            })
            .join('');
        }
        el('rawMemoryViewer').textContent = state.rulesAndMemory.rawMemoryJson || '[]';

        // MCP Table
        const mcpBox = el('mcpTableContainer');
        if (state.mcp.servers.length === 0) {
          mcpBox.innerHTML =
            '<div style="padding: 16px 4px; color: var(--obs-muted); font-size: 12.5px;">' +
            'No MCP servers configured. Click <strong>Edit .rursor/mcp.json</strong> to create a starter configuration.' +
            '</div>';
        } else {
          const rows = state.mcp.servers
            .map(function (srv) {
              const statusPill = srv.connected
                ? '<span class="badge badge-ok">● Connected</span>'
                : '<span class="badge badge-err">● Disconnected</span>';
              return (
                '<tr>' +
                '<td style="font-weight: 600; color: #F8FAFC;">' +
                escapeHtml(srv.name) +
                (srv.commandSummary
                  ? '<div class="mono" style="color: var(--obs-muted); font-weight: 400; font-size: 11px;">' +
                    escapeHtml(srv.commandSummary) +
                    '</div>'
                  : '') +
                '</td>' +
                '<td>' +
                statusPill +
                '</td>' +
                '<td class="mono">' +
                String(srv.toolCount || 0) +
                ' tools</td>' +
                '<td style="color: var(--obs-muted); font-size: 11.5px;">' +
                escapeHtml(srv.error || '—') +
                '</td>' +
                '</tr>'
              );
            })
            .join('');
          mcpBox.innerHTML =
            '<table class="data-table">' +
            '<thead><tr><th>Server</th><th>Status</th><th>Tools</th><th>Details</th></tr></thead>' +
            '<tbody>' +
            rows +
            '</tbody></table>';
        }

        // Index & Sandbox
        el('reindexBtn').textContent = '⚡ Reindex Workspace';
        el('indexWorkspacePath').textContent =
          state.workspaceRoot || 'Open a workspace folder to enable indexing';
        el('statIndexedFiles').textContent = String(state.indexAndSandbox.indexedFiles);
        el('statSkills').textContent = String(state.indexAndSandbox.skillsCount);
        el('statMemories').textContent = String(state.indexAndSandbox.memoriesCount);
        el('statBgProcs').textContent = String(state.indexAndSandbox.backgroundProcesses);
        el('toggleUsePty').checked = Boolean(state.indexAndSandbox.usePty);
        el('worktreeCountLabel').textContent = String(state.indexAndSandbox.worktrees.length);
        el('checkpointCountLabel').textContent = String(state.indexAndSandbox.checkpoints.length);
      }

      window.addEventListener('message', function (event) {
        const msg = event.data;
        if (!msg) return;
        if (msg.type === 'state' && msg.payload) {
          applyState(msg.payload);
        } else if (msg.type === 'toast') {
          showToast(msg.message, msg.kind);
        }
      });

      vscode.postMessage({ type: 'ready' });
    })();
  </script>
</body>
</html>`;
  }
}
