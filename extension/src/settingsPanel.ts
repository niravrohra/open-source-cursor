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

interface DashboardStatePayload {
  workspaceRoot: string | null;
  sidecarReady: boolean;
  keys: Record<ProviderId, KeyStatusInfo>;
  models: {
    defaultModel: string;
    thinkingBudget: number;
    permissionProfile: PermissionProfile;
    promptCache: boolean;
    autoVerify: boolean;
  };
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
      'Rursor Settings',
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
          if (!msg.key || typeof msg.boolValue !== 'boolean') {
            return;
          }
          await this.updateRursorSetting(msg.key, msg.boolValue);
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

    const statePayload: DashboardStatePayload = {
      workspaceRoot: statusRes.workspaceRoot ?? root,
      sidecarReady: Boolean(statusRes.ready),
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
        defaultModel: cfg.get<string>('defaultModel', 'gemini-2.5-flash'),
        thinkingBudget: cfg.get<number>('agent.thinkingBudget', 0),
        permissionProfile,
        promptCache: cfg.get<boolean>('agent.promptCache', true),
        autoVerify: cfg.get<boolean>('agent.autoVerify', false),
      },
      tab: {
        enabled: cfg.get<boolean>('autocomplete.enabled', true),
        nesEnabled: cfg.get<boolean>('autocomplete.nesEnabled', true),
        backend,
        autocompleteModel: cfg.get<string>('autocomplete.model', 'gemini-2.5-flash-lite'),
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
      --obs-bg: var(--vscode-editor-background, #0B0D12);
      --obs-surface: var(--vscode-sideBar-background, #080A0F);
      --obs-card: #0F131D;
      --obs-card-hover: #131926;
      --obs-border: var(--vscode-panel-border, #1B2334);
      --obs-input-bg: var(--vscode-input-background, #0C1018);
      --obs-input-border: var(--vscode-input-border, #1F293D);
      --obs-fg: var(--vscode-editor-foreground, #E2E8F0);
      --obs-muted: var(--vscode-descriptionForeground, #8B9BB4);
      --obs-cyan: #38BDF8;
      --obs-cyan-dim: rgba(56, 189, 248, 0.14);
      --obs-violet: #A78BFA;
      --obs-violet-dim: rgba(167, 139, 250, 0.16);
      --obs-emerald: #34D399;
      --obs-emerald-dim: rgba(52, 211, 153, 0.15);
      --obs-rose: #FB7185;
      --obs-rose-dim: rgba(251, 113, 133, 0.15);
      --obs-amber: #FBBF24;
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

    /* Top Header Bar */
    .topbar {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 14px 24px;
      background: var(--obs-surface);
      border-bottom: 1px solid var(--obs-border);
      flex-shrink: 0;
    }

    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
    }

    .brand-orb {
      width: 28px;
      height: 28px;
      border-radius: 8px;
      background: linear-gradient(135deg, var(--obs-cyan), var(--obs-violet));
      display: flex;
      align-items: center;
      justify-content: center;
      color: #06080C;
      font-weight: 800;
      font-size: 15px;
      box-shadow: 0 0 16px rgba(56, 189, 248, 0.28);
    }

    .brand-title {
      font-size: 15px;
      font-weight: 700;
      letter-spacing: -0.01em;
      color: #F8FAFC;
    }

    .brand-sub {
      font-size: 11px;
      color: var(--obs-muted);
    }

    .topbar-right {
      display: flex;
      align-items: center;
      gap: 10px;
    }

    .kbd {
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11px;
      padding: 2px 7px;
      border-radius: 5px;
      background: rgba(148, 163, 184, 0.12);
      border: 1px solid var(--obs-border);
      color: var(--obs-muted);
    }

    /* Main Split Shell */
    .shell {
      display: grid;
      grid-template-columns: 230px 1fr;
      flex: 1;
      min-height: 0;
    }

    @media (max-width: 760px) {
      .shell {
        grid-template-columns: 1fr;
      }
      .nav-rail {
        flex-direction: row;
        overflow-x: auto;
        border-right: none;
        border-bottom: 1px solid var(--obs-border);
        padding: 8px 12px;
      }
    }

    .nav-rail {
      background: var(--obs-surface);
      border-right: 1px solid var(--obs-border);
      padding: 16px 12px;
      display: flex;
      flex-direction: column;
      gap: 4px;
      overflow-y: auto;
    }

    .nav-section-label {
      font-size: 10px;
      font-weight: 700;
      text-transform: uppercase;
      letter-spacing: 0.08em;
      color: var(--obs-muted);
      padding: 4px 10px 8px;
    }

    .nav-btn {
      appearance: none;
      border: 1px solid transparent;
      background: transparent;
      color: var(--obs-muted);
      text-align: left;
      padding: 9px 12px;
      border-radius: var(--radius-md);
      font-size: 13px;
      font-weight: 500;
      cursor: pointer;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      transition: all 0.14s ease;
    }

    .nav-btn:hover {
      background: rgba(148, 163, 184, 0.08);
      color: #F1F5F9;
    }

    .nav-btn.active {
      background: var(--obs-cyan-dim);
      border-color: rgba(56, 189, 248, 0.32);
      color: #F8FAFC;
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
      background: var(--obs-cyan);
      color: #06080C;
      font-weight: 700;
    }

    /* Content Viewport */
    .content {
      overflow-y: auto;
      padding: 24px 32px 48px;
    }

    .tab-pane {
      display: none;
      max-width: 880px;
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
      margin-bottom: 20px;
    }

    .pane-title {
      font-size: 19px;
      font-weight: 700;
      color: #F8FAFC;
      margin: 0 0 4px;
      letter-spacing: -0.01em;
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
      background-color: #1E2738;
      border: 1px solid #2E3B52;
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
      background-color: #94A3B8;
      transition: 0.16s;
      border-radius: 50%;
    }

    .switch input:checked + .slider {
      background-color: rgba(56, 189, 248, 0.25);
      border-color: var(--obs-cyan);
    }

    .switch input:checked + .slider:before {
      transform: translateX(18px);
      background-color: var(--obs-cyan);
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
  <div class="topbar">
    <div class="brand">
      <div class="brand-orb">✦</div>
      <div>
        <div class="brand-title">Rursor Settings</div>
        <div class="brand-sub" id="topWorkspaceLabel">Native AI IDE Control Plane</div>
      </div>
    </div>
    <div class="topbar-right">
      <span id="sidecarStatusBadge" class="badge badge-warn">Connecting...</span>
      <span class="kbd">⌘⇧J</span>
      <button type="button" class="btn" id="refreshAllBtn">↻ Refresh</button>
    </div>
  </div>

  <div class="shell">
    <nav class="nav-rail" id="navRail">
      <div class="nav-section-label">Configuration</div>
      <button type="button" class="nav-btn active" data-tab="general">
        <span>General &amp; Models</span>
        <span class="nav-badge" id="navKeysBadge">0/3</span>
      </button>
      <button type="button" class="nav-btn" data-tab="tab">
        <span>Tab &amp; NES</span>
        <span class="nav-badge" id="navTabBadge">On</span>
      </button>
      <button type="button" class="nav-btn" data-tab="rules">
        <span>Rules &amp; Memory</span>
        <span class="nav-badge" id="navRulesBadge">0</span>
      </button>
      <button type="button" class="nav-btn" data-tab="mcp">
        <span>MCP Servers</span>
        <span class="nav-badge" id="navMcpBadge">0</span>
      </button>
      <button type="button" class="nav-btn" data-tab="index">
        <span>Index &amp; Sandbox</span>
        <span class="nav-badge" id="navIndexBadge">0</span>
      </button>
    </nav>

    <main class="content">
      <!-- TAB 1: GENERAL & MODELS -->
      <section class="tab-pane active" id="pane-general">
        <div class="pane-header">
          <h1 class="pane-title">General &amp; Models</h1>
          <p class="pane-desc">Manage OS Keychain BYOK credentials, Smart Router defaults, reasoning token budgets, and execution permissions.</p>
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
              <option value="gemini-2.5-flash">gemini-2.5-flash (Fast Default)</option>
              <option value="gemini-2.5-pro">gemini-2.5-pro (Deep Architecture &amp; Reasoning)</option>
              <option value="gemini-2.5-flash-lite">gemini-2.5-flash-lite (Ultra Low Latency)</option>
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

          <div class="field">
            <label>
              <span>Agent Permission Profile</span>
              <span class="mono" style="color: var(--obs-muted);">rursor.agent.permissionProfile</span>
            </label>
            <div class="profile-grid" id="permissionProfileGrid">
              <div class="profile-card selected" data-profile="safe-auto">
                <div class="profile-name">safe-auto</div>
                <div class="profile-desc">Auto-runs codebase reads &amp; search; prompts before file mutations or shell commands.</div>
              </div>
              <div class="profile-card" data-profile="read-only">
                <div class="profile-name">read-only</div>
                <div class="profile-desc">Exploration &amp; architectural review mode; blocks file writes and command execution.</div>
              </div>
              <div class="profile-card" data-profile="full-auto">
                <div class="profile-name">full-auto</div>
                <div class="profile-desc">Autonomous multi-file edits, PTY test verification, and self-healing loops.</div>
              </div>
            </div>
          </div>

          <div class="btn-row" style="margin-top: 16px;">
            <button type="button" class="btn btn-primary" id="saveModelSettingsBtn">Save Model &amp; Router Settings</button>
          </div>
        </div>

        <div class="card">
          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Prompt Context Caching</div>
              <div class="toggle-sub">Cache static workspace rules and persistent memory prefixes on Gemini turns (<span class="mono">rursor.agent.promptCache</span>).</div>
            </div>
            <label class="switch">
              <input type="checkbox" id="togglePromptCache" checked />
              <span class="slider"></span>
            </label>
          </div>
          <div class="toggle-row">
            <div class="toggle-info">
              <div class="toggle-title">Auto-Verify Agent Edits</div>
              <div class="toggle-sub">Automatically run detected build/test/typecheck suites after edits and self-correct failures (<span class="mono">rursor.agent.autoVerify</span>).</div>
            </div>
            <label class="switch">
              <input type="checkbox" id="toggleAutoVerify" />
              <span class="slider"></span>
            </label>
          </div>
        </div>
      </section>

      <!-- TAB 2: TAB & NES -->
      <section class="tab-pane" id="pane-tab">
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
              <input type="text" id="tabCloudModelInput" value="gemini-2.5-flash-lite" placeholder="gemini-2.5-flash-lite" />
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

      <!-- TAB 3: RULES & MEMORY -->
      <section class="tab-pane" id="pane-rules">
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

      <!-- TAB 5: INDEX & SANDBOX -->
      <section class="tab-pane" id="pane-index">
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

      // Tab Switching
      document.querySelectorAll('.nav-btn').forEach(function (btn) {
        btn.addEventListener('click', function () {
          const target = btn.getAttribute('data-tab');
          document.querySelectorAll('.nav-btn').forEach(function (b) {
            b.classList.toggle('active', b === btn);
          });
          document.querySelectorAll('.tab-pane').forEach(function (pane) {
            pane.classList.toggle('active', pane.id === 'pane-' + target);
          });
        });
      });

      el('refreshAllBtn').addEventListener('click', function () {
        rulesDirty = false;
        vscode.postMessage({ type: 'refresh' });
      });

      // Delegated Clicks (API Keys, Skill Open, Memory Delete, Budget Presets)
      document.addEventListener('click', function (ev) {
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
        if (action === 'save-key') {
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

      // Instant Switches
      el('togglePromptCache').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'agent.promptCache',
          boolValue: Boolean(e.target.checked),
        });
      });

      el('toggleAutoVerify').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'agent.autoVerify',
          boolValue: Boolean(e.target.checked),
        });
      });

      el('toggleUsePty').addEventListener('change', function (e) {
        vscode.postMessage({
          type: 'toggleSetting',
          key: 'agent.usePty',
          boolValue: Boolean(e.target.checked),
        });
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

        const connectedKeysCount = ['gemini', 'anthropic', 'openai'].filter(function (p) {
          return state.keys[p].hasKeychainKey || state.keys[p].hasSidecarKey;
        }).length;
        el('navKeysBadge').textContent = connectedKeysCount + '/3';
        el('navTabBadge').textContent = state.tab.enabled ? 'On' : 'Off';
        el('navRulesBadge').textContent = String(
          (state.rulesAndMemory.hasWorkspaceRules ? 1 : 0) +
            state.rulesAndMemory.workspaceSkills.length
        );
        el('navMcpBadge').textContent = String(state.mcp.servers.length);
        el('navIndexBadge').textContent = String(state.indexAndSandbox.indexedFiles);

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
