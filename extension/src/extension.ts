import * as vscode from 'vscode';
import { AgentPanelProvider, RursorDiffContentProvider } from './agentPanel';
import { registerInlineCompletionCommands, RursorInlineCompletionProvider } from './inlineCompletion';
import { InlineEditSessionManager, RursorCodeActionProvider } from './inlineEdit';
import { SidecarClient } from './sidecarClient';
import { RursorWebPreviewPanel } from './webPreview';

const SECRET_GEMINI_KEY = 'rursor.geminiApiKey';
const SECRET_ANTHROPIC_KEY = 'rursor.anthropicApiKey';
const SECRET_OPENAI_KEY = 'rursor.openaiApiKey';

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const client = new SidecarClient(context.extensionPath);
  context.subscriptions.push(client);

  const mainStatus = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  mainStatus.text = '$(sparkle) Rursor';
  mainStatus.tooltip = 'Click to view Rursor Sidecar status';
  mainStatus.command = 'rursor.status';
  mainStatus.show();
  context.subscriptions.push(mainStatus);

  const tabStatus = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 99);
  tabStatus.command = 'rursor.toggleAutocomplete';
  const refreshTabStatus = () => {
    const enabled = vscode.workspace
      .getConfiguration('rursor')
      .get<boolean>('autocomplete.enabled', true);
    tabStatus.text = enabled ? '$(zap) Tab: On' : '$(circle-slash) Tab: Off';
    tabStatus.tooltip = 'Toggle Rursor Tab Autocomplete';
  };
  refreshTabStatus();
  tabStatus.show();
  context.subscriptions.push(tabStatus);

  let agentProvider: AgentPanelProvider | undefined;

  const syncConfig = async (modelOverride?: string): Promise<void> => {
    const cfg = vscode.workspace.getConfiguration('rursor');
    const [apiKey, anthropicApiKey, openaiApiKey] = await Promise.all([
      context.secrets.get(SECRET_GEMINI_KEY),
      context.secrets.get(SECRET_ANTHROPIC_KEY),
      context.secrets.get(SECRET_OPENAI_KEY),
    ]);
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const model = modelOverride ?? cfg.get<string>('defaultModel', 'gemini-2.5-flash');
    const autocompleteModel = cfg.get<string>('autocomplete.model', 'gemini-2.5-flash-lite');
    const autocompleteBackend = cfg.get<string>('autocomplete.backend', 'gemini');
    const localEndpoint = cfg.get<string>('autocomplete.localEndpoint', 'http://127.0.0.1:11434');
    const localModel = cfg.get<string>('autocomplete.localModel', 'qwen2.5-coder:7b');
    const thinkingBudget = cfg.get<number>('agent.thinkingBudget', 0);
    const enablePromptCache = cfg.get<boolean>('agent.promptCache', true);
    const autoVerify = cfg.get<boolean>('agent.autoVerify', false);
    const { promise } = client.request('set_config', {
      apiKey,
      anthropicApiKey,
      openaiApiKey,
      model,
      autocompleteModel,
      autocompleteBackend,
      localEndpoint,
      localModel,
      thinkingBudget,
      enablePromptCache,
      autoVerify,
      workspaceRoot,
    });
    await promise.catch(() => undefined);
    await agentProvider?.refreshStatus();
  };

  void syncConfig();

  const diffProvider = new RursorDiffContentProvider();
  context.subscriptions.push(
    diffProvider,
    vscode.workspace.registerTextDocumentContentProvider('rursor-diff', diffProvider)
  );

  const inlineEditManager = new InlineEditSessionManager(client);
  context.subscriptions.push(
    inlineEditManager,
    vscode.languages.registerCodeLensProvider({ scheme: 'file' }, inlineEditManager),
    vscode.languages.registerCodeActionsProvider(
      { scheme: 'file' },
      new RursorCodeActionProvider(),
      { providedCodeActionKinds: [vscode.CodeActionKind.QuickFix] }
    )
  );

  const completionProvider = new RursorInlineCompletionProvider(client);
  context.subscriptions.push(
    completionProvider,
    vscode.languages.registerInlineCompletionItemProvider({ pattern: '**' }, completionProvider)
  );
  registerInlineCompletionCommands(context, completionProvider);

  agentProvider = new AgentPanelProvider(client, diffProvider, context.extensionUri, async (model) => {
    await syncConfig(model);
  });
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(AgentPanelProvider.viewType, agentProvider, {
      webviewOptions: { retainContextWhenHidden: true },
    })
  );

  // Incremental workspace indexing on save/create/delete
  const watcher = vscode.workspace.createFileSystemWatcher('**/*');
  context.subscriptions.push(
    watcher,
    watcher.onDidCreate((uri) => {
      if (uri.scheme === 'file') {
        void client.request('update_file', { path: uri.fsPath }).promise.catch(() => undefined);
      }
    }),
    watcher.onDidDelete((uri) => {
      if (uri.scheme === 'file') {
        void client.request('remove_file', { path: uri.fsPath }).promise.catch(() => undefined);
      }
    }),
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.uri.scheme === 'file') {
        void client
          .request('update_file', { path: doc.uri.fsPath, content: doc.getText() })
          .promise.catch(() => undefined);
      }
    }),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration('rursor')) {
        refreshTabStatus();
        void syncConfig();
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand('rursor.setApiKey', async () => {
      const key = await vscode.window.showInputBox({
        title: 'Set Gemini Cloud API Key',
        prompt: 'Stored securely in your OS keychain via VS Code SecretStorage',
        password: true,
        ignoreFocusOut: true,
      });
      if (key === undefined) {
        return;
      }
      await context.secrets.store(SECRET_GEMINI_KEY, key.trim());
      await syncConfig();
      vscode.window.showInformationMessage('Rursor: Gemini API key saved.');
    }),
    vscode.commands.registerCommand('rursor.setAnthropicApiKey', async () => {
      const key = await vscode.window.showInputBox({
        title: 'Set Anthropic API Key (Optional BYOK)',
        prompt: 'Stored securely in your OS keychain via VS Code SecretStorage',
        password: true,
        ignoreFocusOut: true,
      });
      if (key === undefined) {
        return;
      }
      await context.secrets.store(SECRET_ANTHROPIC_KEY, key.trim());
      await syncConfig();
      vscode.window.showInformationMessage('Rursor: Anthropic API key saved.');
    }),
    vscode.commands.registerCommand('rursor.setOpenAiApiKey', async () => {
      const key = await vscode.window.showInputBox({
        title: 'Set OpenAI API Key (Optional BYOK)',
        prompt: 'Stored securely in your OS keychain via VS Code SecretStorage',
        password: true,
        ignoreFocusOut: true,
      });
      if (key === undefined) {
        return;
      }
      await context.secrets.store(SECRET_OPENAI_KEY, key.trim());
      await syncConfig();
      vscode.window.showInformationMessage('Rursor: OpenAI API key saved.');
    }),
    vscode.commands.registerCommand('rursor.status', async () => {
      const { promise } = client.request('status');
      const status = await promise;
      vscode.window.showInformationMessage(`Rursor sidecar: ${JSON.stringify(status)}`);
    }),
    vscode.commands.registerCommand('rursor.reindex', async () => {
      const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (!root) {
        vscode.window.showWarningMessage('Open a folder first to index.');
        return;
      }
      const { promise } = client.request<{ indexedFiles: number }>('index_workspace', {
        workspaceRoot: root,
      });
      const res = await promise;
      vscode.window.showInformationMessage(`Rursor indexed ${res.indexedFiles} files.`);
    }),
    vscode.commands.registerCommand('rursor.inlineEdit', async (arg?: unknown) => {
      await inlineEditManager.runInlineEdit(
        arg as
        | string
        | {
          prompt?: string;
          range?: {
            startLineNumber: number;
            startColumn: number;
            endLineNumber: number;
            endColumn: number;
          };
          uri?: string;
        }
        | undefined
      );
    }),
    vscode.commands.registerCommand('rursor.openPreview', () => {
      RursorWebPreviewPanel.createOrShow(context.extensionUri);
    }),
    vscode.commands.registerCommand('rursor.restoreCheckpoint', async () => {
      try {
        const { promise } = client.request<{
          checkpoints: Array<{ checkpointId: string; label: string; fileCount: number }>;
        }>('list_checkpoints');
        const res = await promise;
        const checkpoints = res.checkpoints ?? [];
        if (checkpoints.length === 0) {
          vscode.window.showInformationMessage('No Rursor checkpoints saved yet.');
          return;
        }
        const picked = await vscode.window.showQuickPick(
          checkpoints.map((cp) => ({
            label: cp.label,
            description: `${cp.fileCount} files (${cp.checkpointId})`,
            checkpointId: cp.checkpointId,
          })),
          { placeHolder: 'Select checkpoint to restore' }
        );
        if (!picked) {
          return;
        }
        await client.request('restore_checkpoint', { checkpointId: picked.checkpointId }).promise;
        vscode.window.showInformationMessage(`Restored checkpoint: ${picked.label}`);
      } catch (err) {
        vscode.window.showErrorMessage(
          `Failed to restore checkpoint: ${err instanceof Error ? err.message : String(err)}`
        );
      }
    }),
    vscode.commands.registerCommand('rursor.manageWorktrees', async () => {
      try {
        const action = await vscode.window.showQuickPick(
          ['Create Worktree Sandbox', 'Diff Worktree', 'Merge Worktree', 'Remove Worktree'],
          { placeHolder: 'Rursor Git Worktree Sandboxes' }
        );
        if (!action) {
          return;
        }
        if (action === 'Create Worktree Sandbox') {
          const taskName = await vscode.window.showInputBox({
            title: 'Create Git Worktree Sandbox',
            prompt: 'Short task slug (e.g. feat-auth)',
          });
          if (!taskName?.trim()) {
            return;
          }
          const { promise } = client.request<{ worktree: { path: string; branch?: string } }>(
            'create_worktree',
            { taskName: taskName.trim() }
          );
          const created = await promise;
          vscode.window.showInformationMessage(
            `Created worktree at ${created.worktree.path} (${created.worktree.branch ?? ''})`
          );
          return;
        }
        const { promise: listPromise } = client.request<{
          worktrees: Array<{ path: string; branch?: string; isMain: boolean }>;
        }>('list_worktrees');
        const listRes = await listPromise;
        const candidates = (listRes.worktrees ?? []).filter((w) => !w.isMain);
        if (candidates.length === 0) {
          vscode.window.showInformationMessage('No secondary git worktree sandboxes found.');
          return;
        }
        const picked = await vscode.window.showQuickPick(
          candidates.map((w) => ({
            label: w.branch ?? w.path,
            description: w.path,
            worktree: w,
          })),
          { placeHolder: `Select worktree for: ${action}` }
        );
        if (!picked) {
          return;
        }
        if (action === 'Diff Worktree') {
          const { promise } = client.request<{ diff: string }>('diff_worktree', {
            worktreePath: picked.worktree.path,
          });
          const res = await promise;
          const doc = await vscode.workspace.openTextDocument({
            content: res.diff || '(no changes)',
            language: 'diff',
          });
          await vscode.window.showTextDocument(doc);
        } else if (action === 'Merge Worktree' && picked.worktree.branch) {
          await client.request('merge_worktree', { branchName: picked.worktree.branch }).promise;
          vscode.window.showInformationMessage(`Merged worktree branch ${picked.worktree.branch}.`);
        } else if (action === 'Remove Worktree') {
          await client.request('remove_worktree', { worktreePath: picked.worktree.path }).promise;
          vscode.window.showInformationMessage(`Removed worktree ${picked.worktree.path}.`);
        }
      } catch (err) {
        vscode.window.showErrorMessage(
          `Worktree error: ${err instanceof Error ? err.message : String(err)}`
        );
      }
    }),
    vscode.commands.registerCommand('rursor.fixDiagnostics', async () => {
      await inlineEditManager.runInlineEdit('Fix the diagnostics in this code');
    }),
    vscode.commands.registerCommand('rursor.acceptInlineEdit', async () => {
      await inlineEditManager.acceptSession();
    }),
    vscode.commands.registerCommand('rursor.revertInlineEdit', async () => {
      await inlineEditManager.revertSession();
    }),
    vscode.commands.registerCommand('rursor.refineInlineEdit', async () => {
      await inlineEditManager.refineSession();
    }),
    vscode.commands.registerCommand('rursor.toggleAutocomplete', async () => {
      const cfg = vscode.workspace.getConfiguration('rursor');
      const current = cfg.get<boolean>('autocomplete.enabled', true);
      await cfg.update('autocomplete.enabled', !current, vscode.ConfigurationTarget.Global);
      refreshTabStatus();
      vscode.window.showInformationMessage(
        `Rursor Tab Autocomplete ${!current ? 'enabled' : 'disabled'}.`
      );
    }),
    vscode.commands.registerCommand('rursor.openAgent', async () => {
      await vscode.commands.executeCommand('workbench.view.extension.rursor-sidebar');
    }),
    vscode.commands.registerCommand('rursor.reloadMcp', async () => {
      try {
        const { promise } = client.request<{
          servers: Array<{ name: string; connected: boolean; toolCount: number; error?: string }>;
        }>('reload_mcp');
        const res = await promise;
        const servers = res.servers ?? [];
        const connected = servers.filter((s) => s.connected).length;
        await agentProvider?.refreshStatus();
        vscode.window.showInformationMessage(
          `Rursor MCP reloaded: ${connected}/${servers.length} servers connected.`
        );
      } catch (err) {
        vscode.window.showErrorMessage(
          `Failed to reload MCP servers: ${err instanceof Error ? err.message : String(err)}`
        );
      }
    })
  );
}

export function deactivate(): void { }
