import * as crypto from 'node:crypto';
import * as vscode from 'vscode';

const DEFAULT_PREVIEW_URL = 'http://localhost:3000';
const INSPECT_TIMEOUT_MS = 5_000;
const MAX_TEXT_SUMMARY_BYTES = 8 * 1024;
const MAX_CONSOLE_LOGS = 200;

export type ViewportMode = 'desktop' | 'mobile';

export interface PreviewLogEntry {
  timestamp: string;
  level: 'info' | 'warn' | 'error';
  source: string;
  message: string;
}

export interface WebInspectionResult {
  url: string;
  status: number;
  statusText: string;
  ok: boolean;
  contentType: string;
  headers: Record<string, string>;
  title: string;
  metaErrors: string[];
  scriptErrors: string[];
  errors: string[];
  textSummary: string;
  truncated: boolean;
}

interface WebviewIncomingMessage {
  type: string;
  url?: string;
  viewport?: ViewportMode;
  level?: 'info' | 'warn' | 'error';
  source?: string;
  message?: string;
}

/**
 * Validates that a URL uses `http:` or `https:` and has a valid host (including localhost/127.0.0.1).
 * Throws an Error if the URL is invalid or uses an unsupported scheme.
 */
export function validatePreviewUrl(rawUrl: string): string {
  const trimmed = rawUrl.trim();
  if (!trimmed) {
    throw new Error('URL cannot be empty.');
  }

  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    throw new Error(`Invalid URL: "${trimmed}". Expected an http:// or https:// URL.`);
  }

  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new Error(
      `Unsupported URL scheme "${parsed.protocol}". Only http:// and https:// URLs are allowed.`
    );
  }

  if (!parsed.hostname) {
    throw new Error(`URL "${trimmed}" is missing a hostname.`);
  }

  return parsed.toString();
}

function normalizeUserUrlInput(rawUrl: string): string {
  const trimmed = rawUrl.trim();
  if (!trimmed) {
    return DEFAULT_PREVIEW_URL;
  }
  if (/^(localhost|127\.0\.0\.1|\[::1\])(?::\d+)?(\/.*)?$/i.test(trimmed)) {
    return `http://${trimmed}`;
  }
  return trimmed;
}

function decodeHtmlEntities(input: string): string {
  return input
    .replace(/&nbsp;/gi, ' ')
    .replace(/&amp;/gi, '&')
    .replace(/&lt;/gi, '<')
    .replace(/&gt;/gi, '>')
    .replace(/&quot;/gi, '"')
    .replace(/&#39;|&apos;/gi, "'")
    .replace(/&#(\d+);/g, (_match, dec: string) => {
      const code = Number.parseInt(dec, 10);
      return Number.isFinite(code) && code > 0 ? String.fromCodePoint(code) : '';
    })
    .replace(/&#x([0-9a-f]+);/gi, (_match, hex: string) => {
      const code = Number.parseInt(hex, 16);
      return Number.isFinite(code) && code > 0 ? String.fromCodePoint(code) : '';
    });
}

function extractTitle(html: string): string {
  const match = html.match(/<title\b[^>]*>([\s\S]*?)<\/title>/i);
  if (!match) {
    return '';
  }
  return decodeHtmlEntities(match[1].replace(/\s+/g, ' ').trim());
}

function extractMetaAndScriptErrors(
  html: string,
  status: number,
  statusText: string,
  contentType: string,
  title: string
): { metaErrors: string[]; scriptErrors: string[] } {
  const metaErrors: string[] = [];
  const scriptErrors: string[] = [];

  if (status >= 400) {
    metaErrors.push(`HTTP ${status}${statusText ? ` ${statusText}` : ''}`);
  }

  const isHtml =
    contentType.toLowerCase().includes('text/html') ||
    contentType.toLowerCase().includes('application/xhtml+xml') ||
    /<html[\s>]/i.test(html);

  if (isHtml && !title) {
    metaErrors.push('Missing <title> element in HTML document.');
  }

  // Check <meta name="error" ...> or <meta http-equiv="status" ...>
  const metaTagRegex = /<meta\b[^>]*>/gi;
  let metaMatch: RegExpExecArray | null;
  while ((metaMatch = metaTagRegex.exec(html)) !== null) {
    const tag = metaMatch[0];
    const nameMatch = tag.match(/\bname\s*=\s*["']([^"']+)["']/i);
    const contentMatch = tag.match(/\bcontent\s*=\s*["']([^"']*)["']/i);
    const nameVal = nameMatch?.[1]?.toLowerCase() ?? '';
    const contentVal = contentMatch?.[1]?.trim() ?? '';
    if ((nameVal === 'error' || nameVal === 'robots-error') && contentVal) {
      metaErrors.push(`Meta error: ${decodeHtmlEntities(contentVal)}`);
    }
  }

  // Detect common dev-server / framework error overlays and inline script error markers
  const overlayPatterns: Array<{ regex: RegExp; label: string }> = [
    { regex: /vite-error-overlay/i, label: 'Detected Vite error overlay in page markup.' },
    { regex: /__next_error__/i, label: 'Detected Next.js error boundary (__next_error__).' },
    {
      regex: /Application error:\s*a client-side exception has occurred/i,
      label: 'Detected Next.js client-side exception message.',
    },
    {
      regex: /<pre\b[^>]*class=["'][^"']*(?:error|stack|traceback)[^"']*["'][^>]*>([\s\S]{1,300})/i,
      label: 'Detected rendered error/stacktrace block',
    },
  ];

  for (const pattern of overlayPatterns) {
    const match = html.match(pattern.regex);
    if (match) {
      if (match[1]) {
        const snippet = decodeHtmlEntities(match[1].replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim());
        scriptErrors.push(`${pattern.label}: ${snippet}`);
      } else {
        scriptErrors.push(pattern.label);
      }
    }
  }

  // Check inline <script> blocks for blatant fatal error markers or unclosed scripts
  const openScriptCount = (html.match(/<script\b[^>]*>/gi) ?? []).length;
  const closeScriptCount = (html.match(/<\/script>/gi) ?? []).length;
  if (openScriptCount > closeScriptCount) {
    scriptErrors.push(
      `Malformed HTML: found ${openScriptCount} <script> opening tags but ${closeScriptCount} </script> closing tags.`
    );
  }

  const inlineScriptRegex = /<script\b(?![^>]*\bsrc\s*=)[^>]*>([\s\S]*?)<\/script>/gi;
  let scriptMatch: RegExpExecArray | null;
  while ((scriptMatch = inlineScriptRegex.exec(html)) !== null) {
    const body = scriptMatch[1];
    if (/\b(?:SyntaxError|ReferenceError|TypeError)\s*:/.test(body) && !/catch\s*\(/.test(body)) {
      scriptErrors.push('Inline script contains uncaught JS error literal.');
      break;
    }
  }

  return { metaErrors, scriptErrors };
}

function extractVisibleText(html: string): { textSummary: string; truncated: boolean } {
  const withoutHiddenBlocks = html
    .replace(/<!--[\s\S]*?-->/g, ' ')
    .replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, ' ')
    .replace(/<style\b[^>]*>[\s\S]*?<\/style>/gi, ' ')
    .replace(/<noscript\b[^>]*>[\s\S]*?<\/noscript>/gi, ' ')
    .replace(/<svg\b[^>]*>[\s\S]*?<\/svg>/gi, ' ')
    .replace(/<template\b[^>]*>[\s\S]*?<\/template>/gi, ' ');

  const withLineBreaks = withoutHiddenBlocks.replace(
    /<\/?(?:p|div|section|article|header|footer|main|nav|aside|li|ul|ol|h[1-6]|tr|br|hr)\b[^>]*>/gi,
    '\n'
  );

  const stripped = withLineBreaks.replace(/<[^>]+>/g, ' ');
  const decoded = decodeHtmlEntities(stripped);

  const normalizedLines = decoded
    .split('\n')
    .map((line) => line.replace(/[ \t\r]+/g, ' ').trim())
    .filter((line) => line.length > 0);

  const joined = normalizedLines.join('\n');
  return truncateUtf8(joined, MAX_TEXT_SUMMARY_BYTES);
}

function truncateUtf8(text: string, maxBytes: number): { textSummary: string; truncated: boolean } {
  const buf = Buffer.from(text, 'utf8');
  if (buf.byteLength <= maxBytes) {
    return { textSummary: text, truncated: false };
  }

  let end = maxBytes;
  // Walk back from UTF-8 continuation bytes (10xxxxxx) so we never split a multi-byte char
  while (end > 0 && (buf[end] & 0b1100_0000) === 0b1000_0000) {
    end -= 1;
  }
  return {
    textSummary: buf.subarray(0, end).toString('utf8'),
    truncated: true,
  };
}

/**
 * Validates a localhost/HTTP(S) URL, fetches it with a 5-second timeout via Node `fetch`,
 * and returns HTTP status, headers, `<title>`, meta/script errors, and an 8 KB visible text summary.
 */
export async function inspectWebUrl(rawUrl: string): Promise<WebInspectionResult> {
  const validatedUrl = validatePreviewUrl(rawUrl);

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), INSPECT_TIMEOUT_MS);

  let response: Response;
  try {
    response = await fetch(validatedUrl, {
      method: 'GET',
      redirect: 'follow',
      signal: controller.signal,
      headers: {
        Accept: 'text/html,application/xhtml+xml,application/json;q=0.9,text/plain;q=0.8,*/*;q=0.5',
        'User-Agent': 'RursorWebPreview/1.0',
      },
    });
  } catch (err) {
    const message =
      err instanceof Error && err.name === 'AbortError'
        ? `Timed out after ${INSPECT_TIMEOUT_MS / 1000}s while fetching ${validatedUrl}`
        : `Failed to fetch ${validatedUrl}: ${err instanceof Error ? err.message : String(err)}`;
    throw new Error(message);
  } finally {
    clearTimeout(timer);
  }

  const headers: Record<string, string> = {};
  response.headers.forEach((value, key) => {
    headers[key] = value;
  });

  const contentType = response.headers.get('content-type') ?? '';
  const rawBody = await response.text();

  const isHtml =
    contentType.toLowerCase().includes('text/html') ||
    contentType.toLowerCase().includes('application/xhtml+xml') ||
    /<(?:!doctype|html|head|body|title)\b/i.test(rawBody);

  const title = isHtml ? extractTitle(rawBody) : '';
  const { metaErrors, scriptErrors } = isHtml
    ? extractMetaAndScriptErrors(rawBody, response.status, response.statusText, contentType, title)
    : {
        metaErrors:
          response.status >= 400
            ? [`HTTP ${response.status}${response.statusText ? ` ${response.statusText}` : ''}`]
            : [],
        scriptErrors: [],
      };

  const errors = Array.from(new Set([...metaErrors, ...scriptErrors]));
  const { textSummary, truncated } = isHtml
    ? extractVisibleText(rawBody)
    : truncateUtf8(rawBody.trim(), MAX_TEXT_SUMMARY_BYTES);

  return {
    url: response.url || validatedUrl,
    status: response.status,
    statusText: response.statusText,
    ok: response.ok,
    contentType,
    headers,
    title,
    metaErrors,
    scriptErrors,
    errors,
    textSummary,
    truncated,
  };
}

/**
 * Manages a singleton VS Code WebviewPanel (`rursor.webPreview`) providing an in-editor
 * live browser preview with URL bar, history Back, Reload, Open External, Desktop/Mobile
 * viewport toggle, and a captured console/error log drawer.
 */
export class RursorWebPreviewPanel implements vscode.Disposable {
  public static readonly viewType = 'rursor.webPreview';
  public static currentPanel: RursorWebPreviewPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private readonly disposables: vscode.Disposable[] = [];
  private history: string[] = [DEFAULT_PREVIEW_URL];
  private historyIndex = 0;
  private viewport: ViewportMode = 'desktop';
  private logs: PreviewLogEntry[] = [];
  private reloadNonce = 0;

  private constructor(panel: vscode.WebviewPanel, initialUrl?: string) {
    this.panel = panel;

    if (initialUrl) {
      try {
        const validated = validatePreviewUrl(normalizeUserUrlInput(initialUrl));
        this.history = [validated];
        this.historyIndex = 0;
      } catch (err) {
        this.appendLog(
          'error',
          'preview',
          err instanceof Error ? err.message : String(err)
        );
      }
    }

    this.panel.webview.html = this.renderHtml();

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);

    this.panel.webview.onDidReceiveMessage(
      async (msg: WebviewIncomingMessage) => {
        await this.handleWebviewMessage(msg);
      },
      null,
      this.disposables
    );

    this.postState();
    void this.probeUrlForLogs(this.getCurrentUrl());
  }

  /**
   * Creates the singleton Web Preview panel or reveals the existing one,
   * optionally navigating to `initialUrl`.
   */
  public static createOrShow(
    _extensionUri?: vscode.Uri,
    initialUrl?: string
  ): RursorWebPreviewPanel {
    const column = vscode.ViewColumn.Beside;

    if (RursorWebPreviewPanel.currentPanel) {
      RursorWebPreviewPanel.currentPanel.panel.reveal(column);
      if (initialUrl) {
        RursorWebPreviewPanel.currentPanel.navigate(initialUrl);
      }
      return RursorWebPreviewPanel.currentPanel;
    }

    const panel = vscode.window.createWebviewPanel(
      RursorWebPreviewPanel.viewType,
      'Rursor Web Preview',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );

    RursorWebPreviewPanel.currentPanel = new RursorWebPreviewPanel(panel, initialUrl);
    return RursorWebPreviewPanel.currentPanel;
  }

  public getCurrentUrl(): string {
    return this.history[this.historyIndex] ?? DEFAULT_PREVIEW_URL;
  }

  public getLogs(): PreviewLogEntry[] {
    return [...this.logs];
  }

  public navigate(rawUrl: string): void {
    try {
      const validated = validatePreviewUrl(normalizeUserUrlInput(rawUrl));
      const current = this.getCurrentUrl();
      if (validated !== current) {
        this.history = this.history.slice(0, this.historyIndex + 1);
        this.history.push(validated);
        this.historyIndex = this.history.length - 1;
      } else {
        this.reloadNonce += 1;
      }
      this.appendLog('info', 'navigation', `Loaded ${validated}`);
      this.postState();
      void this.probeUrlForLogs(validated);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      this.appendLog('error', 'navigation', message);
      this.postState();
    }
  }

  public goBack(): void {
    if (this.historyIndex > 0) {
      this.historyIndex -= 1;
      const target = this.getCurrentUrl();
      this.appendLog('info', 'navigation', `Back to ${target}`);
      this.postState();
      void this.probeUrlForLogs(target);
    }
  }

  public reload(): void {
    this.reloadNonce += 1;
    const current = this.getCurrentUrl();
    this.appendLog('info', 'navigation', `Reloaded ${current}`);
    this.postState();
    void this.probeUrlForLogs(current);
  }

  public setViewport(viewport: ViewportMode): void {
    this.viewport = viewport;
    this.postState();
  }

  public dispose(): void {
    RursorWebPreviewPanel.currentPanel = undefined;
    while (this.disposables.length > 0) {
      const disposable = this.disposables.pop();
      disposable?.dispose();
    }
    this.panel.dispose();
  }

  private async handleWebviewMessage(msg: WebviewIncomingMessage): Promise<void> {
    switch (msg.type) {
      case 'ready':
        this.postState();
        break;
      case 'navigate':
        if (typeof msg.url === 'string') {
          this.navigate(msg.url);
        }
        break;
      case 'back':
        this.goBack();
        break;
      case 'reload':
        this.reload();
        break;
      case 'openExternal': {
        const target = typeof msg.url === 'string' ? msg.url : this.getCurrentUrl();
        try {
          const validated = validatePreviewUrl(normalizeUserUrlInput(target));
          await vscode.env.openExternal(vscode.Uri.parse(validated));
        } catch (err) {
          this.appendLog(
            'error',
            'external',
            err instanceof Error ? err.message : String(err)
          );
          this.postState();
        }
        break;
      }
      case 'setViewport':
        if (msg.viewport === 'desktop' || msg.viewport === 'mobile') {
          this.setViewport(msg.viewport);
        }
        break;
      case 'log':
        if (typeof msg.message === 'string' && msg.message.trim()) {
          this.appendLog(
            msg.level === 'warn' || msg.level === 'error' ? msg.level : 'info',
            typeof msg.source === 'string' && msg.source ? msg.source : 'iframe',
            msg.message
          );
          this.postState();
        }
        break;
      case 'clearLogs':
        this.logs = [];
        this.postState();
        break;
      default:
        break;
    }
  }

  private async probeUrlForLogs(url: string): Promise<void> {
    try {
      const info = await inspectWebUrl(url);
      if (info.status >= 400) {
        this.appendLog('error', 'http', `${info.status} ${info.statusText} (${info.url})`);
      } else {
        const titleLabel = info.title ? ` — "${info.title}"` : '';
        this.appendLog('info', 'http', `${info.status} OK${titleLabel}`);
      }
      for (const errMsg of info.errors) {
        this.appendLog('error', 'inspect', errMsg);
      }
    } catch (err) {
      this.appendLog(
        'warn',
        'inspect',
        err instanceof Error ? err.message : String(err)
      );
    }
    this.postState();
  }

  private appendLog(level: 'info' | 'warn' | 'error', source: string, message: string): void {
    const now = new Date();
    const timestamp = now.toISOString().slice(11, 19);
    this.logs.push({ timestamp, level, source, message });
    if (this.logs.length > MAX_CONSOLE_LOGS) {
      this.logs.splice(0, this.logs.length - MAX_CONSOLE_LOGS);
    }
  }

  private postState(): void {
    void this.panel.webview.postMessage({
      type: 'state',
      url: this.getCurrentUrl(),
      canGoBack: this.historyIndex > 0,
      viewport: this.viewport,
      reloadNonce: this.reloadNonce,
      logs: this.logs,
    });
  }

  private renderHtml(): string {
    const nonce = crypto.randomBytes(16).toString('hex');
    const cspSource = this.panel.webview.cspSource;

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta
    http-equiv="Content-Security-Policy"
    content="default-src 'none'; frame-src http: https:; img-src ${cspSource} http: https: data:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';"
  />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Rursor Web Preview</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: var(--vscode-editor-background, #141416);
      --panel: var(--vscode-sideBar-background, #1c1d21);
      --border: var(--vscode-panel-border, #2e3038);
      --fg: var(--vscode-editor-foreground, #e5e7eb);
      --muted: var(--vscode-descriptionForeground, #9ca3af);
      --accent: var(--vscode-button-background, #2563eb);
      --accent-fg: var(--vscode-button-foreground, #ffffff);
      --input-bg: var(--vscode-input-background, #111215);
      --err: #f87171;
      --warn: #fbbf24;
      --info: #60a5fa;
    }
    * { box-sizing: border-box; }
    html, body {
      margin: 0;
      padding: 0;
      width: 100%;
      height: 100%;
      background: var(--bg);
      color: var(--fg);
      font-family: var(--vscode-font-family, -apple-system, BlinkMacSystemFont, sans-serif);
      font-size: 12px;
      display: flex;
      flex-direction: column;
      overflow: hidden;
    }
    .toolbar {
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 8px 10px;
      background: var(--panel);
      border-bottom: 1px solid var(--border);
      flex-shrink: 0;
    }
    button {
      background: var(--input-bg);
      color: var(--fg);
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 4px 8px;
      font-size: 12px;
      cursor: pointer;
      white-space: nowrap;
    }
    button:hover:not(:disabled) {
      border-color: var(--accent);
    }
    button:disabled {
      opacity: 0.45;
      cursor: default;
    }
    button.active {
      background: var(--accent);
      color: var(--accent-fg);
      border-color: var(--accent);
    }
    .url-form {
      display: flex;
      flex: 1;
      gap: 4px;
      min-width: 120px;
    }
    .url-input {
      flex: 1;
      min-width: 80px;
      background: var(--input-bg);
      color: var(--fg);
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 4px 8px;
      font-size: 12px;
      outline: none;
    }
    .url-input:focus {
      border-color: var(--accent);
    }
    .viewport-group {
      display: flex;
      border-radius: 4px;
      overflow: hidden;
    }
    .viewport-group button {
      border-radius: 0;
    }
    .viewport-group button:first-child {
      border-top-left-radius: 4px;
      border-bottom-left-radius: 4px;
    }
    .viewport-group button:last-child {
      border-top-right-radius: 4px;
      border-bottom-right-radius: 4px;
      border-left: none;
    }
    .stage {
      flex: 1;
      display: flex;
      align-items: stretch;
      justify-content: center;
      background: #0d0e11;
      overflow: auto;
      padding: 0;
    }
    .stage.mobile {
      padding: 16px;
    }
    .frame-shell {
      width: 100%;
      height: 100%;
      background: #ffffff;
      border: none;
      display: flex;
      flex-direction: column;
    }
    .stage.mobile .frame-shell {
      width: 390px;
      max-width: 390px;
      min-width: 390px;
      height: 100%;
      border: 1px solid var(--border);
      border-radius: 12px;
      overflow: hidden;
      box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45);
    }
    iframe {
      width: 100%;
      height: 100%;
      border: none;
      background: #ffffff;
    }
    .drawer {
      background: var(--panel);
      border-top: 1px solid var(--border);
      display: flex;
      flex-direction: column;
      flex-shrink: 0;
    }
    .drawer-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 5px 10px;
      border-bottom: 1px solid var(--border);
      font-weight: 600;
      user-select: none;
    }
    .drawer-actions {
      display: flex;
      align-items: center;
      gap: 6px;
    }
    .badge {
      display: inline-block;
      min-width: 18px;
      padding: 1px 5px;
      border-radius: 999px;
      background: var(--input-bg);
      color: var(--muted);
      font-size: 11px;
      text-align: center;
    }
    .badge.has-errors {
      background: rgba(248, 113, 113, 0.2);
      color: var(--err);
    }
    .drawer-body {
      height: 140px;
      overflow-y: auto;
      padding: 6px 10px;
      font-family: var(--vscode-editor-font-family, monospace);
      font-size: 11px;
      line-height: 1.45;
    }
    .drawer.collapsed .drawer-body {
      display: none;
    }
    .log-row {
      display: flex;
      gap: 8px;
      padding: 2px 0;
      border-bottom: 1px solid rgba(255, 255, 255, 0.04);
      word-break: break-word;
    }
    .log-time { color: var(--muted); flex-shrink: 0; }
    .log-src { color: var(--muted); flex-shrink: 0; }
    .log-info { color: var(--info); }
    .log-warn { color: var(--warn); }
    .log-error { color: var(--err); }
    .empty-logs { color: var(--muted); font-style: italic; }
  </style>
</head>
<body>
  <div class="toolbar">
    <button id="backBtn" title="Back" disabled>Back</button>
    <button id="reloadBtn" title="Reload preview">Reload</button>
    <form id="urlForm" class="url-form">
      <input id="urlInput" class="url-input" type="text" value="${DEFAULT_PREVIEW_URL}" spellcheck="false" />
      <button type="submit">Go</button>
    </form>
    <div class="viewport-group" role="group" aria-label="Viewport">
      <button id="desktopBtn" class="active" type="button">Desktop</button>
      <button id="mobileBtn" type="button">Mobile 390px</button>
    </div>
    <button id="externalBtn" type="button" title="Open in system browser">Open External</button>
  </div>

  <div id="stage" class="stage">
    <div class="frame-shell">
      <iframe
        id="previewFrame"
        title="Rursor Web Preview"
        src="${DEFAULT_PREVIEW_URL}"
        sandbox="allow-scripts allow-same-origin allow-forms allow-popups allow-modals"
      ></iframe>
    </div>
  </div>

  <div id="logDrawer" class="drawer">
    <div class="drawer-header">
      <span>Console &amp; Inspection Logs <span id="logCount" class="badge">0</span></span>
      <div class="drawer-actions">
        <button id="clearLogsBtn" type="button">Clear</button>
        <button id="toggleLogsBtn" type="button">Hide</button>
      </div>
    </div>
    <div id="logBody" class="drawer-body">
      <div class="empty-logs">No console or inspection messages captured yet.</div>
    </div>
  </div>

  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const backBtn = document.getElementById('backBtn');
    const reloadBtn = document.getElementById('reloadBtn');
    const urlForm = document.getElementById('urlForm');
    const urlInput = document.getElementById('urlInput');
    const desktopBtn = document.getElementById('desktopBtn');
    const mobileBtn = document.getElementById('mobileBtn');
    const externalBtn = document.getElementById('externalBtn');
    const stage = document.getElementById('stage');
    const previewFrame = document.getElementById('previewFrame');
    const logDrawer = document.getElementById('logDrawer');
    const logCount = document.getElementById('logCount');
    const logBody = document.getElementById('logBody');
    const clearLogsBtn = document.getElementById('clearLogsBtn');
    const toggleLogsBtn = document.getElementById('toggleLogsBtn');

    let lastUrl = '';
    let lastReloadNonce = -1;

    backBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'back' });
    });

    reloadBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'reload' });
    });

    urlForm.addEventListener('submit', (e) => {
      e.preventDefault();
      vscode.postMessage({ type: 'navigate', url: urlInput.value });
    });

    desktopBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'setViewport', viewport: 'desktop' });
    });

    mobileBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'setViewport', viewport: 'mobile' });
    });

    externalBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'openExternal', url: urlInput.value });
    });

    clearLogsBtn.addEventListener('click', () => {
      vscode.postMessage({ type: 'clearLogs' });
    });

    toggleLogsBtn.addEventListener('click', () => {
      const collapsed = logDrawer.classList.toggle('collapsed');
      toggleLogsBtn.textContent = collapsed ? 'Show' : 'Hide';
    });

    previewFrame.addEventListener('load', () => {
      try {
        const win = previewFrame.contentWindow;
        if (win && win.console) {
          // Attach same-origin console capture when preview target permits DOM access
          const origError = win.console.error.bind(win.console);
          const origWarn = win.console.warn.bind(win.console);
          win.console.error = (...args) => {
            vscode.postMessage({ type: 'log', level: 'error', source: 'console', message: args.map(String).join(' ') });
            origError(...args);
          };
          win.console.warn = (...args) => {
            vscode.postMessage({ type: 'log', level: 'warn', source: 'console', message: args.map(String).join(' ') });
            origWarn(...args);
          };
          win.addEventListener('error', (ev) => {
            vscode.postMessage({ type: 'log', level: 'error', source: 'runtime', message: ev.message || 'Uncaught error' });
          });
          win.addEventListener('unhandledrejection', (ev) => {
            vscode.postMessage({ type: 'log', level: 'error', source: 'promise', message: String(ev.reason || 'Unhandled rejection') });
          });
        }
      } catch {
        // Cross-origin frame: rely on postMessage forwarding and inspectWebUrl logs
      }
    });

    previewFrame.addEventListener('error', () => {
      vscode.postMessage({
        type: 'log',
        level: 'error',
        source: 'iframe',
        message: 'Failed to load iframe preview.',
      });
    });

    window.addEventListener('message', (event) => {
      const msg = event.data;
      if (!msg || typeof msg !== 'object') return;

      if (msg.type === 'rursor-preview-console') {
        vscode.postMessage({
          type: 'log',
          level: msg.level || 'info',
          source: msg.source || 'page',
          message: String(msg.message || ''),
        });
        return;
      }

      if (msg.type === 'state') {
        urlInput.value = msg.url || '${DEFAULT_PREVIEW_URL}';
        backBtn.disabled = !msg.canGoBack;

        const isMobile = msg.viewport === 'mobile';
        stage.classList.toggle('mobile', isMobile);
        desktopBtn.classList.toggle('active', !isMobile);
        mobileBtn.classList.toggle('active', isMobile);

        if (msg.url && (msg.url !== lastUrl || msg.reloadNonce !== lastReloadNonce)) {
          lastUrl = msg.url;
          lastReloadNonce = msg.reloadNonce;
          previewFrame.src = msg.url;
        }

        renderLogs(Array.isArray(msg.logs) ? msg.logs : []);
      }
    });

    function renderLogs(logs) {
      logCount.textContent = String(logs.length);
      const hasErrors = logs.some((l) => l.level === 'error');
      logCount.classList.toggle('has-errors', hasErrors);

      if (logs.length === 0) {
        logBody.innerHTML = '<div class="empty-logs">No console or inspection messages captured yet.</div>';
        return;
      }

      logBody.innerHTML = '';
      for (const entry of logs) {
        const row = document.createElement('div');
        row.className = 'log-row';

        const t = document.createElement('span');
        t.className = 'log-time';
        t.textContent = entry.timestamp || '';

        const src = document.createElement('span');
        src.className = 'log-src';
        src.textContent = '[' + (entry.source || 'log') + ']';

        const msg = document.createElement('span');
        msg.className = 'log-' + (entry.level || 'info');
        msg.textContent = entry.message || '';

        row.appendChild(t);
        row.appendChild(src);
        row.appendChild(msg);
        logBody.appendChild(row);
      }
      logBody.scrollTop = logBody.scrollHeight;
    }

    vscode.postMessage({ type: 'ready' });
  </script>
</body>
</html>`;
  }
}
