import * as vscode from 'vscode';
import { AgentPanelProvider, RursorDiffContentProvider } from './agentPanel';
import { RursorInlineCompletionProvider } from './inlineCompletion';
import { InlineEditSessionManager, RursorCodeActionProvider } from './inlineEdit';
import { SidecarClient } from './sidecarClient';

const SECRET_GEMINI_KEY = 'rursor.geminiApiKey';

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

  const syncConfig = async (modelOverride?: string): Promise<void> => {
    const cfg = vscode.workspace.getConfiguration('rursor');
    const apiKey = await context.secrets.get(SECRET_GEMINI_KEY);
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const model = modelOverride ?? cfg.get<string>('defaultModel', 'gemini-2.5-flash');
    const autocompleteModel = cfg.get<string>('autocomplete.model', 'gemini-2.5-flash-lite');
    const { promise } = client.request('set_config', {
      apiKey,
      model,
      autocompleteModel,
      workspaceRoot,
    });
    await promise.catch(() => undefined);
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

  const agentProvider = new AgentPanelProvider(client, diffProvider, async (model) => {
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
    vscode.commands.registerCommand('rursor.inlineEdit', async () => {
      await inlineEditManager.runInlineEdit();
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
    })
  );
}

export function deactivate(): void {}
