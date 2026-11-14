import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

interface ChatTurn {
  role: 'user' | 'assistant';
  content: string;
}

export class AgentPanelProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'rursor.agentView';
  private view: vscode.WebviewView | undefined;
  private history: ChatTurn[] = [];
  private activeRequestId: number | undefined;

  constructor(
    private readonly client: SidecarClient,
    private readonly onModelChange: (model: string) => Promise<void>
  ) {}

  resolveWebviewView(webviewView: vscode.WebviewView): void {
    this.view = webviewView;
    webviewView.webview.options = { enableScripts: true };
    webviewView.webview.html = this.renderHtml();

    webviewView.webview.onDidReceiveMessage(async (msg: { type: string; text?: string; model?: string; includeWorkspace?: boolean }) => {
      if (msg.type === 'send' && msg.text) {
        await this.handleUserPrompt(msg.text, Boolean(msg.includeWorkspace), msg.model);
      } else if (msg.type === 'stop' && this.activeRequestId !== undefined) {
        await this.client.cancel(this.activeRequestId);
        this.activeRequestId = undefined;
      } else if (msg.type === 'clear') {
        this.history = [];
      } else if (msg.type === 'setKey') {
        await vscode.commands.executeCommand('rursor.setApiKey');
      } else if (msg.type === 'modelChanged' && msg.model) {
        await this.onModelChange(msg.model);
      }
    });
  }

  private async handleUserPrompt(
    prompt: string,
    includeWorkspace: boolean,
    model?: string
  ): Promise<void> {
    this.history.push({ role: 'user', content: prompt });

    const contextFiles: Array<{ path: string; content: string }> = [];
    const active = vscode.window.activeTextEditor;
    if (active) {
      const doc = active.document;
      const content = active.selection.isEmpty
        ? doc.getText().slice(0, 16_000)
        : doc.getText(active.selection);
      contextFiles.push({
        path: vscode.workspace.asRelativePath(doc.uri, false),
        content,
      });
    }

    const { id, promise } = this.client.request<{ text: string }>(
      'agent_chat',
      {
        messages: this.history,
        contextFiles,
        useWorkspaceSearch: includeWorkspace,
        model,
      },
      (delta) => {
        void this.view?.webview.postMessage({ type: 'delta', text: delta });
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

  private renderHtml(): string {
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<style>
  body { font-family: var(--vscode-font-family); color: var(--vscode-foreground); background: var(--vscode-sideBar-background); margin: 0; display: flex; flex-direction: column; height: 100vh; }
  header { display: flex; gap: 6px; padding: 8px; border-bottom: 1px solid var(--vscode-panel-border); align-items: center; }
  select, button { background: var(--vscode-button-secondaryBackground); color: var(--vscode-button-secondaryForeground); border: none; padding: 4px 8px; border-radius: 4px; cursor: pointer; font-size: 12px; }
  button.primary { background: var(--vscode-button-background); color: var(--vscode-button-foreground); }
  #messages { flex: 1; overflow-y: auto; padding: 10px; display: flex; flex-direction: column; gap: 10px; }
  .msg { padding: 8px 10px; border-radius: 6px; white-space: pre-wrap; font-size: 13px; line-height: 1.45; }
  .user { background: var(--vscode-input-background); align-self: flex-end; max-width: 90%; }
  .assistant { background: var(--vscode-editor-background); border: 1px solid var(--vscode-panel-border); }
  .err { color: var(--vscode-errorForeground); }
  footer { padding: 8px; border-top: 1px solid var(--vscode-panel-border); display: flex; flex-direction: column; gap: 6px; }
  textarea { width: 100%; box-sizing: border-box; min-height: 56px; resize: vertical; background: var(--vscode-input-background); color: var(--vscode-input-foreground); border: 1px solid var(--vscode-input-border); border-radius: 4px; padding: 6px; font-family: inherit; }
  .row { display: flex; justify-content: space-between; align-items: center; font-size: 12px; }
</style>
</head>
<body>
  <header>
    <select id="model">
      <option value="gemini-2.5-flash">gemini-2.5-flash</option>
      <option value="gemini-2.5-pro">gemini-2.5-pro</option>
    </select>
    <button id="keyBtn">API Key</button>
    <button id="clearBtn">Clear</button>
  </header>
  <div id="messages"></div>
  <footer>
    <textarea id="prompt" placeholder="Ask Rursor (active file attached automatically)…"></textarea>
    <div class="row">
      <label><input type="checkbox" id="wsCheck" checked /> @workspace search</label>
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
  const wsEl = document.getElementById('wsCheck');
  let activeAssistantEl = null;

  function addMessage(cls, text) {
    const div = document.createElement('div');
    div.className = 'msg ' + cls;
    div.textContent = text;
    messagesEl.appendChild(div);
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return div;
  }

  function send() {
    const text = promptEl.value.trim();
    if (!text) return;
    addMessage('user', text);
    activeAssistantEl = addMessage('assistant', '');
    promptEl.value = '';
    vscode.postMessage({ type: 'send', text, model: modelEl.value, includeWorkspace: wsEl.checked });
  }

  document.getElementById('sendBtn').onclick = send;
  document.getElementById('stopBtn').onclick = () => vscode.postMessage({ type: 'stop' });
  document.getElementById('clearBtn').onclick = () => { messagesEl.innerHTML = ''; vscode.postMessage({ type: 'clear' }); };
  document.getElementById('keyBtn').onclick = () => vscode.postMessage({ type: 'setKey' });
  modelEl.onchange = () => vscode.postMessage({ type: 'modelChanged', model: modelEl.value });
  promptEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });

  window.addEventListener('message', (event) => {
    const msg = event.data;
    if (msg.type === 'delta') {
      if (!activeAssistantEl) activeAssistantEl = addMessage('assistant', '');
      activeAssistantEl.textContent += msg.text;
      messagesEl.scrollTop = messagesEl.scrollHeight;
    } else if (msg.type === 'done') {
      activeAssistantEl = null;
    } else if (msg.type === 'error') {
      addMessage('err', msg.text);
      activeAssistantEl = null;
    }
  });
</script>
</body>
</html>`;
  }
}
