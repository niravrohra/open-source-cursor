import * as vscode from 'vscode';
import { AgentPanelProvider } from './agentPanel';
import { runInlineEdit } from './inlineEdit';
import { SidecarClient } from './sidecarClient';

const SECRET_GEMINI_KEY = 'rursor.geminiApiKey';

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const client = new SidecarClient(context.extensionPath);
  context.subscriptions.push(client);

  const statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  statusBar.text = '$(sparkle) Rursor';
  statusBar.command = 'rursor.status';
  statusBar.show();
  context.subscriptions.push(statusBar);

  const syncConfig = async (model?: string): Promise<void> => {
    const apiKey = await context.secrets.get(SECRET_GEMINI_KEY);
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const { promise } = client.request('set_config', {
      apiKey,
      model,
      workspaceRoot,
    });
    await promise.catch(() => undefined);
  };

  void syncConfig();

  const agentProvider = new AgentPanelProvider(client, async (model) => {
    await syncConfig(model);
  });
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(AgentPanelProvider.viewType, agentProvider)
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
      await runInlineEdit(client);
    }),
    vscode.commands.registerCommand('rursor.openAgent', async () => {
      await vscode.commands.executeCommand('workbench.view.extension.rursor-sidebar');
    })
  );
}

export function deactivate(): void {}
