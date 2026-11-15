import * as crypto from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

export const BROWSER_STUDIO_VIEW_TYPE = 'rursor.browserStudio';
export const OPEN_BROWSER_STUDIO_COMMAND = 'rursor.openBrowserStudio';

const DEFAULT_STUDIO_URL = 'http://localhost:3000';

export type StudioViewportPreset = 'desktop' | 'mobile';

export interface StudioViewportDimensions {
  width: number;
  height: number;
}

export const VIEWPORT_PRESETS: Record<StudioViewportPreset, StudioViewportDimensions> = {
  desktop: { width: 1280, height: 800 },
  mobile: { width: 390, height: 844 },
};

export interface ElementBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface SourceHint {
  file: string;
  line: number;
}

export interface SomElement {
  id: number;
  tag: string;
  inputType?: string | null;
  role?: string | null;
  label: string;
  placeholder?: string | null;
  href?: string | null;
  selector: string;
  bounds: ElementBounds;
  sourceHint?: SourceHint | null;
}

export interface BrowserSnapshot {
  url: string;
  title: string;
  viewportWidth: number;
  viewportHeight: number;
  drawSomBadges: boolean;
  elements: SomElement[];
  screenshotBase64?: string;
  screenshotPath?: string;
  textSummary: string;
  timestampMs: number;
}

export interface NavigateResult {
  url: string;
  title: string;
  status: number;
  viewportWidth: number;
  viewportHeight: number;
}

export type BrowserAction =
  | {
      kind: 'click';
      somId?: number;
      selector?: string;
      x?: number;
      y?: number;
    }
  | {
      kind: 'type';
      somId?: number;
      selector?: string;
      text: string;
      pressEnter?: boolean;
    }
  | {
      kind: 'scroll';
      deltaX: number;
      deltaY: number;
    }
  | {
      kind: 'evaluate';
      expression: string;
    }
  | {
      kind: 'clear_overlay';
    };

export interface BrowserActionResult {
  ok: boolean;
  action: string;
  resolvedX?: number;
  resolvedY?: number;
  value?: unknown;
  url: string;
  title: string;
}

export interface BrowserConsoleEntry {
  level: string;
  text: string;
  url?: string | null;
  line?: number | null;
  timestampMs: number;
}

export interface BrowserNetworkError {
  requestId: string;
  url: string;
  method: string;
  status?: number | null;
  errorText: string;
  resourceType?: string | null;
  timestampMs: number;
}

export interface BrowserDiagnostics {
  consoleLogs: BrowserConsoleEntry[];
  networkErrors: BrowserNetworkError[];
}

export interface PdfExportResult {
  outputPath: string;
  bytesWritten: number;
}

export type WalkthroughStep =
  | {
      kind: 'navigate';
      url: string;
      viewportWidth?: number;
      viewportHeight?: number;
    }
  | {
      kind: 'click';
      somId?: number;
      selector?: string;
      x?: number;
      y?: number;
    }
  | {
      kind: 'type';
      somId?: number;
      selector?: string;
      text: string;
      pressEnter?: boolean;
    }
  | {
      kind: 'scroll';
      deltaX: number;
      deltaY: number;
    }
  | {
      kind: 'wait_ms';
      ms: number;
    };

export interface WalkthroughRecording {
  outputMp4Path: string;
  frameCount: number;
  stepCount: number;
  durationMs: number;
  bytesWritten: number;
  encoder: string;
}

export interface BrowserStudioImageAttachment {
  mimeType: string;
  data: string;
  name?: string;
}

export interface BrowserStudioHooks {
  onSendToAgent?: (
    prompt: string,
    images?: BrowserStudioImageAttachment[],
    mode?: 'agent' | 'ask' | 'plan' | 'take-control'
  ) => Promise<void>;
  onRunInlineEdit?: (instruction?: string) => Promise<void>;
}

interface StudioIncomingMessage {
  type: string;
  url?: string;
  viewport?: StudioViewportPreset;
  drawSomBadges?: boolean;
  inspectMode?: boolean;
  x?: number;
  y?: number;
  somId?: number;
  selector?: string;
  text?: string;
  pressEnter?: boolean;
  deltaX?: number;
  deltaY?: number;
}

export function normalizeStudioUrl(rawUrl: string): string {
  const trimmed = rawUrl.trim();
  if (!trimmed) {
    return DEFAULT_STUDIO_URL;
  }
  if (/^(localhost|127\.0\.0\.1|\[::1\])(?::\d+)?(\/.*)?$/i.test(trimmed)) {
    return `http://${trimmed}`;
  }
  const parsed = new URL(trimmed);
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') {
    throw new Error(`Unsupported URL scheme "${parsed.protocol}". Use http:// or https://.`);
  }
  return parsed.toString();
}

/**
 * Matches a viewport `(x, y)` click against `snapshot.elements`, preferring the tightest
 * containing bounding box and falling back to the closest element center within 20px.
 */
export function findElementAtViewportPoint(
  elements: readonly SomElement[],
  x: number,
  y: number
): SomElement | undefined {
  let bestContaining: SomElement | undefined;
  let bestArea = Number.POSITIVE_INFINITY;

  for (const el of elements) {
    const { bounds } = el;
    const inside =
      x >= bounds.x &&
      x <= bounds.x + bounds.width &&
      y >= bounds.y &&
      y <= bounds.y + bounds.height;
    if (inside) {
      const area = Math.max(1, bounds.width * bounds.height);
      if (area < bestArea) {
        bestArea = area;
        bestContaining = el;
      }
    }
  }
  if (bestContaining) {
    return bestContaining;
  }

  let nearest: SomElement | undefined;
  let bestDistSq = 20 * 20;
  for (const el of elements) {
    const cx = el.bounds.x + el.bounds.width / 2;
    const cy = el.bounds.y + el.bounds.height / 2;
    const distSq = (x - cx) * (x - cx) + (y - cy) * (y - cy);
    if (distSq <= bestDistSq) {
      bestDistSq = distSq;
      nearest = el;
    }
  }
  return nearest;
}

/**
 * Registers the `rursor.openBrowserStudio` command and manages the singleton `BrowserStudioPanel`.
 */
export function registerBrowserStudio(
  context: vscode.ExtensionContext,
  client: SidecarClient,
  hooks?: BrowserStudioHooks
): vscode.Disposable {
  const cmd = vscode.commands.registerCommand(
    OPEN_BROWSER_STUDIO_COMMAND,
    (initialUrl?: string) => {
      BrowserStudioPanel.createOrShow(
        context.extensionUri,
        client,
        typeof initialUrl === 'string' ? initialUrl : undefined,
        hooks
      );
    }
  );
  context.subscriptions.push(cmd);
  return cmd;
}

/**
 * Interactive Rursor Browser & Computer-Use Studio (`rursor.browserStudio`) backed by
 * native Chromium CDP, Set-of-Mark `[1][2]` overlays, Inspect -> Code (`Cmd+K`),
 * PDF export, and `.mp4` video walkthrough recording.
 */
export class BrowserStudioPanel implements vscode.Disposable {
  public static readonly viewType = BROWSER_STUDIO_VIEW_TYPE;
  public static currentPanel: BrowserStudioPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private readonly client: SidecarClient;
  private hooks: BrowserStudioHooks | undefined;
  private readonly disposables: vscode.Disposable[] = [];

  private history: string[] = [DEFAULT_STUDIO_URL];
  private historyIndex = 0;
  private viewportPreset: StudioViewportPreset = 'desktop';
  private drawSomBadges = true;
  private inspectMode = false;
  private busy = false;
  private statusMessage = 'Ready — enter a local or web URL to launch Chromium CDP.';
  private errorBanner: string | null = null;

  private snapshot: BrowserSnapshot | null = null;
  private diagnostics: BrowserDiagnostics = { consoleLogs: [], networkErrors: [] };

  private isRecordingWalkthrough = false;
  private recordedSteps: WalkthroughStep[] = [];
  private lastRecordingPath: string | null = null;

  private constructor(
    panel: vscode.WebviewPanel,
    client: SidecarClient,
    initialUrl?: string,
    hooks?: BrowserStudioHooks
  ) {
    this.panel = panel;
    this.client = client;
    this.hooks = hooks;

    if (initialUrl) {
      try {
        this.history = [normalizeStudioUrl(initialUrl)];
        this.historyIndex = 0;
      } catch (err) {
        this.errorBanner = err instanceof Error ? err.message : String(err);
      }
    }

    this.panel.webview.html = this.renderHtml();

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage(
      async (msg: StudioIncomingMessage) => {
        await this.handleMessage(msg);
      },
      null,
      this.disposables
    );

    if (initialUrl) {
      void this.navigateTo(this.getCurrentUrl(), false);
    } else {
      this.postState();
    }
  }

  public static createOrShow(
    _extensionUri: vscode.Uri,
    client: SidecarClient,
    initialUrl?: string,
    hooks?: BrowserStudioHooks
  ): BrowserStudioPanel {
    const column = vscode.ViewColumn.Beside;
    if (BrowserStudioPanel.currentPanel) {
      if (hooks) {
        BrowserStudioPanel.currentPanel.hooks = hooks;
      }
      BrowserStudioPanel.currentPanel.panel.reveal(column);
      if (initialUrl) {
        void BrowserStudioPanel.currentPanel.navigateTo(initialUrl, true);
      }
      return BrowserStudioPanel.currentPanel;
    }

    const panel = vscode.window.createWebviewPanel(
      BrowserStudioPanel.viewType,
      'Rursor Browser Studio',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );

    BrowserStudioPanel.currentPanel = new BrowserStudioPanel(panel, client, initialUrl, hooks);
    return BrowserStudioPanel.currentPanel;
  }

  public getCurrentUrl(): string {
    return this.history[this.historyIndex] ?? DEFAULT_STUDIO_URL;
  }

  public getLatestSnapshot(): BrowserSnapshot | null {
    return this.snapshot;
  }

  public getLatestDiagnostics(): BrowserDiagnostics {
    return this.diagnostics;
  }

  private getArtifactsRoot(): string {
    const wsRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (wsRoot) {
      return path.join(wsRoot, '.rursor', 'artifacts');
    }
    const home = process.env.HOME || '/tmp';
    return path.join(home, '.rursor', 'artifacts');
  }

  public async navigateTo(rawUrl: string, pushHistory = true): Promise<void> {
    let normalized: string;
    try {
      normalized = normalizeStudioUrl(rawUrl);
    } catch (err) {
      this.errorBanner = err instanceof Error ? err.message : String(err);
      this.postState();
      return;
    }

    if (pushHistory && normalized !== this.getCurrentUrl()) {
      this.history = this.history.slice(0, this.historyIndex + 1);
      this.history.push(normalized);
      this.historyIndex = this.history.length - 1;
    }

    const dims = VIEWPORT_PRESETS[this.viewportPreset];
    if (this.isRecordingWalkthrough) {
      this.recordedSteps.push({
        kind: 'navigate',
        url: normalized,
        viewportWidth: dims.width,
        viewportHeight: dims.height,
      });
    }

    await this.runStudioTask(`Navigating to ${normalized}...`, async () => {
      const nav = await this.client.request<NavigateResult>('browser_navigate', {
        url: normalized,
        viewportWidth: dims.width,
        viewportHeight: dims.height,
      }).promise;
      await this.refreshSnapshotAndDiagnostics();
      this.statusMessage = `Loaded ${nav.title || nav.url} (HTTP ${nav.status})`;
    });
  }

  public async goBack(): Promise<void> {
    if (this.historyIndex <= 0) {
      return;
    }
    this.historyIndex -= 1;
    await this.navigateTo(this.getCurrentUrl(), false);
  }

  public async reload(): Promise<void> {
    await this.navigateTo(this.getCurrentUrl(), false);
  }

  public async setViewportPreset(preset: StudioViewportPreset): Promise<void> {
    if (this.viewportPreset === preset && this.snapshot) {
      return;
    }
    this.viewportPreset = preset;
    if (this.snapshot) {
      await this.navigateTo(this.getCurrentUrl(), false);
    } else {
      this.postState();
    }
  }

  public async setSomOverlay(enabled: boolean): Promise<void> {
    this.drawSomBadges = enabled;
    if (!this.snapshot) {
      this.postState();
      return;
    }
    await this.runStudioTask(
      enabled ? 'Rendering Set-of-Mark [1][2] overlay...' : 'Clearing Set-of-Mark overlay...',
      async () => {
        if (!enabled) {
          await this.client.request<BrowserActionResult>('browser_act', {
            action: { kind: 'clear_overlay' },
          }).promise;
        }
        await this.refreshSnapshotAndDiagnostics();
        this.statusMessage = enabled
          ? `Set-of-Mark active (${this.snapshot?.elements.length ?? 0} interactive elements)`
          : 'Set-of-Mark badges hidden';
      }
    );
  }

  public async performAction(action: BrowserAction, label: string): Promise<void> {
    if (this.isRecordingWalkthrough) {
      if (action.kind === 'click') {
        this.recordedSteps.push({
          kind: 'click',
          somId: action.somId,
          selector: action.selector,
          x: action.x,
          y: action.y,
        });
      } else if (action.kind === 'type') {
        this.recordedSteps.push({
          kind: 'type',
          somId: action.somId,
          selector: action.selector,
          text: action.text,
          pressEnter: action.pressEnter,
        });
      } else if (action.kind === 'scroll') {
        this.recordedSteps.push({
          kind: 'scroll',
          deltaX: action.deltaX,
          deltaY: action.deltaY,
        });
      }
    }

    await this.runStudioTask(label, async () => {
      await this.client.request<BrowserActionResult>('browser_act', { action }).promise;
      await this.refreshSnapshotAndDiagnostics();
      this.statusMessage = `Completed ${action.kind} — ${this.snapshot?.elements.length ?? 0} elements indexed`;
    });
  }

  /**
   * Handles Inspect -> Code (`Cmd+K`) on either `(x, y)` viewport click or a sidebar `SomElement`.
   */
  public async inspectElementToCode(element: SomElement | undefined, clickX?: number, clickY?: number): Promise<void> {
    const currentUrl = this.snapshot?.url || this.getCurrentUrl();
    if (!element) {
      const fallbackPrompt = [
        `Inspect page UI at (${Math.round(clickX ?? 0)}, ${Math.round(clickY ?? 0)}) on ${currentUrl}.`,
        'Locate the component rendering this region in the workspace and help me edit it.',
      ].join('\n');
      await this.dispatchPromptToAgent(fallbackPrompt, true);
      return;
    }

    if (element.sourceHint?.file) {
      const resolvedPath = this.resolveWorkspaceFilePath(element.sourceHint.file);
      if (resolvedPath && fs.existsSync(resolvedPath)) {
        const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(resolvedPath));
        const targetLine = Math.max(0, Math.min((element.sourceHint.line || 1) - 1, doc.lineCount - 1));
        const lineRange = doc.lineAt(targetLine).range;
        await vscode.window.showTextDocument(doc, {
          viewColumn: vscode.ViewColumn.One,
          selection: lineRange,
          preserveFocus: false,
        });

        const presetInstruction = `Update [${element.id}] <${element.tag}> "${element.label}" (${element.selector}): `;
        if (this.hooks?.onRunInlineEdit) {
          await this.hooks.onRunInlineEdit();
        } else {
          await vscode.commands.executeCommand('rursor.inlineEdit');
        }
        this.statusMessage = `Opened ${path.basename(resolvedPath)}:${targetLine + 1} for [${element.id}] (${presetInstruction.trim()})`;
        this.postState();
        return;
      }
    }

    // Fallback when React/DOM sourceHint is unavailable: pre-fill & dispatch Agent prompt with selector + label
    const promptLines = [
      `Locate and update UI element **[${element.id}] \`<${element.tag}>\`** rendered on \`${currentUrl}\`:`,
      `- **Visible Label**: ${element.label || '(empty)'}`,
      `- **CSS Selector**: \`${element.selector}\``,
      `- **Bounds**: \`(${element.bounds.x}, ${element.bounds.y}, ${element.bounds.width}x${element.bounds.height})\``,
    ];
    if (element.sourceHint?.file) {
      promptLines.push(`- **Source Hint**: \`${element.sourceHint.file}:${element.sourceHint.line}\``);
    }
    promptLines.push('', 'Find the source file/component in this workspace that renders this element and apply the requested UI improvement.');
    await this.dispatchPromptToAgent(promptLines.join('\n'), true);
    this.statusMessage = `Sent [${element.id}] ${element.selector} to Rursor Agent`;
    this.postState();
  }

  public async toggleWalkthroughRecording(): Promise<void> {
    if (!this.isRecordingWalkthrough) {
      const dims = VIEWPORT_PRESETS[this.viewportPreset];
      const startUrl = this.snapshot?.url || this.getCurrentUrl();
      this.isRecordingWalkthrough = true;
      this.recordedSteps = [
        {
          kind: 'navigate',
          url: startUrl,
          viewportWidth: dims.width,
          viewportHeight: dims.height,
        },
      ];
      this.statusMessage =
        'Recording walkthrough steps — click elements, type, or scroll, then click "Save .mp4" to encode.';
      this.postState();
      return;
    }

    // Finalize recording and encode MP4 via sidecar ffmpeg pipeline
    this.isRecordingWalkthrough = false;
    const stepsToEncode: WalkthroughStep[] =
      this.recordedSteps.length > 1
        ? [...this.recordedSteps, { kind: 'wait_ms', ms: 400 }]
        : [
            ...this.recordedSteps,
            { kind: 'wait_ms', ms: 500 },
            { kind: 'scroll', deltaX: 0, deltaY: 260 },
            { kind: 'wait_ms', ms: 500 },
          ];
    this.recordedSteps = [];

    const outDir = path.join(this.getArtifactsRoot(), 'walkthroughs');
    fs.mkdirSync(outDir, { recursive: true });
    const outputMp4Path = path.join(outDir, `walkthrough-${Date.now()}.mp4`);

    await this.runStudioTask(
      `Encoding ${stepsToEncode.length}-step .mp4 walkthrough with ffmpeg...`,
      async () => {
        const rec = await this.client.request<WalkthroughRecording>(
          'browser_record_walkthrough',
          {
            steps: stepsToEncode,
            outputMp4Path,
          }
        ).promise;
        this.lastRecordingPath = rec.outputMp4Path;
        await this.refreshSnapshotAndDiagnostics();
        this.statusMessage = `Saved walkthrough MP4 (${rec.frameCount} frames, ${Math.round(
          rec.bytesWritten / 1024
        )} KB) -> ${path.basename(rec.outputMp4Path)}`;

        const choice = await vscode.window.showInformationMessage(
          `Rursor Walkthrough recorded (${rec.frameCount} frames via ${rec.encoder}).`,
          'Open Video',
          'Reveal in Finder'
        );
        const fileUri = vscode.Uri.file(rec.outputMp4Path);
        if (choice === 'Open Video') {
          await vscode.commands.executeCommand('vscode.open', fileUri);
        } else if (choice === 'Reveal in Finder') {
          await vscode.commands.executeCommand('revealFileInOS', fileUri);
        }
      }
    );
  }

  public async exportPdf(): Promise<void> {
    const outDir = path.join(this.getArtifactsRoot(), 'pdfs');
    fs.mkdirSync(outDir, { recursive: true });
    const outputPath = path.join(outDir, `page-${Date.now()}.pdf`);

    await this.runStudioTask('Exporting page PDF via Page.printToPDF...', async () => {
      const res = await this.client.request<PdfExportResult>('browser_export_pdf', {
        outputPath,
      }).promise;
      this.statusMessage = `Exported PDF (${Math.round(res.bytesWritten / 1024)} KB) -> ${path.basename(
        res.outputPath
      )}`;

      const choice = await vscode.window.showInformationMessage(
        `Exported PDF (${Math.round(res.bytesWritten / 1024)} KB): ${path.basename(res.outputPath)}`,
        'Open PDF',
        'Reveal in Finder'
      );
      const fileUri = vscode.Uri.file(res.outputPath);
      if (choice === 'Open PDF') {
        await vscode.commands.executeCommand('vscode.open', fileUri);
      } else if (choice === 'Reveal in Finder') {
        await vscode.commands.executeCommand('revealFileInOS', fileUri);
      }
    });
  }

  public async sendDiagnosticsAndScreenshotToAgent(): Promise<void> {
    const url = this.snapshot?.url || this.getCurrentUrl();
    const title = this.snapshot?.title || 'Untitled Page';
    const consoleErrors = this.diagnostics.consoleLogs.filter(
      (c) => c.level === 'error' || c.level === 'exception' || c.level === 'warning' || c.level === 'warn'
    );
    const logsToInclude =
      consoleErrors.length > 0 ? consoleErrors : this.diagnostics.consoleLogs.slice(-15);

    const sections: string[] = [
      `Please diagnose and fix the browser runtime/network issues observed on **${title}** (\`${url}\`):`,
      '',
    ];

    if (logsToInclude.length > 0) {
      sections.push('### Console & Uncaught Exceptions');
      for (const entry of logsToInclude.slice(-20)) {
        const loc = entry.url ? ` (${entry.url}${entry.line ? `:${entry.line}` : ''})` : '';
        sections.push(`- \`[${entry.level.toUpperCase()}]\` ${entry.text}${loc}`);
      }
      sections.push('');
    } else {
      sections.push('### Console Logs\n- *(No console errors buffered)*\n');
    }

    if (this.diagnostics.networkErrors.length > 0) {
      sections.push('### Failed Network Requests');
      for (const net of this.diagnostics.networkErrors.slice(-20)) {
        const code = net.status ? `HTTP ${net.status}` : net.errorText;
        sections.push(`- \`${net.method}\` \`${net.url}\` -> **${code}** (${net.errorText})`);
      }
      sections.push('');
    } else {
      sections.push('### Network Requests\n- *(No failed network requests buffered)*\n');
    }

    if (this.snapshot?.textSummary) {
      sections.push('### Visible DOM Summary');
      sections.push('```text');
      sections.push(this.snapshot.textSummary.slice(0, 1200));
      sections.push('```');
    }

    await this.dispatchPromptToAgent(sections.join('\n'), true);
    this.statusMessage = 'Sent page errors + screenshot to Rursor Agent';
    this.postState();
  }

  private async dispatchPromptToAgent(prompt: string, attachScreenshot: boolean): Promise<void> {
    const images: BrowserStudioImageAttachment[] = [];
    if (attachScreenshot && this.snapshot?.screenshotBase64) {
      images.push({
        mimeType: 'image/png',
        data: this.snapshot.screenshotBase64,
        name: 'browser-studio-snapshot.png',
      });
    }

    if (this.hooks?.onSendToAgent) {
      await this.hooks.onSendToAgent(prompt, images, 'agent');
      return;
    }

    await vscode.env.clipboard.writeText(prompt);
    await vscode.commands.executeCommand('rursor.openAgent');
    vscode.window.showInformationMessage(
      'Rursor Browser Studio copied the diagnostic prompt to your clipboard and focused the Agent panel.'
    );
  }

  private resolveWorkspaceFilePath(rawFile: string): string | null {
    const cleaned = rawFile.replace(/^file:\/\//i, '').trim();
    if (!cleaned) {
      return null;
    }
    if (path.isAbsolute(cleaned) && fs.existsSync(cleaned)) {
      return cleaned;
    }
    const folders = vscode.workspace.workspaceFolders ?? [];
    for (const folder of folders) {
      const candidate = path.resolve(folder.uri.fsPath, cleaned.replace(/^\/+/, ''));
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
    return path.isAbsolute(cleaned) ? cleaned : null;
  }

  private async refreshSnapshotAndDiagnostics(): Promise<void> {
    const [snap, diag] = await Promise.all([
      this.client.request<BrowserSnapshot>('browser_snapshot', {
        drawSomBadges: this.drawSomBadges,
        includeScreenshot: true,
      }).promise,
      this.client.request<BrowserDiagnostics>('browser_diagnostics', {}).promise,
    ]);
    this.snapshot = snap;
    this.diagnostics = diag;
  }

  private async runStudioTask(statusText: string, fn: () => Promise<void>): Promise<void> {
    this.busy = true;
    this.errorBanner = null;
    this.statusMessage = statusText;
    this.postState();
    try {
      await fn();
    } catch (err) {
      this.errorBanner = err instanceof Error ? err.message : String(err);
      this.statusMessage = 'Browser operation failed';
    } finally {
      this.busy = false;
      this.postState();
    }
  }

  private async handleMessage(msg: StudioIncomingMessage): Promise<void> {
    switch (msg.type) {
      case 'ready':
        this.postState();
        break;
      case 'navigate':
        if (typeof msg.url === 'string') {
          await this.navigateTo(msg.url, true);
        }
        break;
      case 'back':
        await this.goBack();
        break;
      case 'reload':
        await this.reload();
        break;
      case 'setViewport':
        if (msg.viewport === 'desktop' || msg.viewport === 'mobile') {
          await this.setViewportPreset(msg.viewport);
        }
        break;
      case 'toggleSom':
        await this.setSomOverlay(
          typeof msg.drawSomBadges === 'boolean' ? msg.drawSomBadges : !this.drawSomBadges
        );
        break;
      case 'toggleInspectMode':
        this.inspectMode =
          typeof msg.inspectMode === 'boolean' ? msg.inspectMode : !this.inspectMode;
        this.statusMessage = this.inspectMode
          ? 'Inspect -> Code (Cmd+K) active: click any element on the canvas to jump to source.'
          : 'Interactive Browser Mode active: click anywhere on the viewport to interact.';
        this.postState();
        break;
      case 'canvasClick': {
        const x = typeof msg.x === 'number' ? msg.x : 0;
        const y = typeof msg.y === 'number' ? msg.y : 0;
        if (this.inspectMode) {
          const matched = findElementAtViewportPoint(this.snapshot?.elements ?? [], x, y);
          await this.inspectElementToCode(matched, x, y);
        } else {
          await this.performAction(
            { kind: 'click', x: Math.round(x * 10) / 10, y: Math.round(y * 10) / 10 },
            `Clicking (${Math.round(x)}, ${Math.round(y)})...`
          );
        }
        break;
      }
      case 'elementClick':
        if (typeof msg.somId === 'number') {
          await this.performAction(
            { kind: 'click', somId: msg.somId },
            `Clicking [${msg.somId}]...`
          );
        }
        break;
      case 'elementInspect':
        if (typeof msg.somId === 'number') {
          const el = this.snapshot?.elements.find((item) => item.id === msg.somId);
          await this.inspectElementToCode(el);
        }
        break;
      case 'typeText':
        if (typeof msg.text === 'string' && msg.text.length > 0) {
          await this.performAction(
            {
              kind: 'type',
              somId: typeof msg.somId === 'number' && msg.somId > 0 ? msg.somId : undefined,
              selector: typeof msg.selector === 'string' && msg.selector ? msg.selector : undefined,
              text: msg.text,
              pressEnter: Boolean(msg.pressEnter),
            },
            `Typing into ${msg.somId ? `[${msg.somId}]` : 'focused element'}...`
          );
        }
        break;
      case 'scroll':
        await this.performAction(
          {
            kind: 'scroll',
            deltaX: typeof msg.deltaX === 'number' ? msg.deltaX : 0,
            deltaY: typeof msg.deltaY === 'number' ? msg.deltaY : 360,
          },
          'Scrolling viewport...'
        );
        break;
      case 'recordWalkthrough':
        await this.toggleWalkthroughRecording();
        break;
      case 'exportPdf':
        await this.exportPdf();
        break;
      case 'clearDiagnostics':
        await this.client
          .request('browser_clear_diagnostics', {})
          .promise.catch(() => undefined);
        this.diagnostics = { consoleLogs: [], networkErrors: [] };
        this.postState();
        break;
      case 'sendErrorsToAgent':
        await this.sendDiagnosticsAndScreenshotToAgent();
        break;
      default:
        break;
    }
  }

  private postState(): void {
    const dims = VIEWPORT_PRESETS[this.viewportPreset];
    void this.panel.webview.postMessage({
      type: 'studioState',
      url: this.snapshot?.url || this.getCurrentUrl(),
      canGoBack: this.historyIndex > 0,
      viewportPreset: this.viewportPreset,
      viewportWidth: this.snapshot?.viewportWidth ?? dims.width,
      viewportHeight: this.snapshot?.viewportHeight ?? dims.height,
      drawSomBadges: this.drawSomBadges,
      inspectMode: this.inspectMode,
      busy: this.busy,
      statusMessage: this.statusMessage,
      errorBanner: this.errorBanner,
      isRecordingWalkthrough: this.isRecordingWalkthrough,
      recordedStepCount: this.recordedSteps.length,
      lastRecordingPath: this.lastRecordingPath,
      snapshot: this.snapshot,
      diagnostics: this.diagnostics,
    });
  }

  public dispose(): void {
    BrowserStudioPanel.currentPanel = undefined;
    while (this.disposables.length > 0) {
      this.disposables.pop()?.dispose();
    }
    this.panel.dispose();
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
    content="default-src 'none'; img-src ${cspSource} data: https: http:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';"
  />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Rursor Browser Studio</title>
  <style>
    :root {
      color-scheme: dark;
      --obsidian-bg: #0B0D12;
      --obsidian-surface: #131822;
      --obsidian-elevated: #19202E;
      --obsidian-border: #232B3D;
      --obsidian-fg: #E6EAF2;
      --obsidian-muted: #8B95A7;
      --obsidian-accent: #7C5CFF;
      --obsidian-accent-hover: #9075FF;
      --obsidian-accent-soft: rgba(124, 92, 255, 0.16);
      --obsidian-danger: #F87171;
      --obsidian-warn: #FBBF24;
      --obsidian-ok: #34D399;
    }

    * { box-sizing: border-box; }

    html, body {
      margin: 0;
      padding: 0;
      width: 100%;
      height: 100%;
      background: var(--obsidian-bg);
      color: var(--obsidian-fg);
      font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter", sans-serif;
      font-size: 12px;
      display: flex;
      flex-direction: column;
      overflow: hidden;
      user-select: none;
    }

    .toolbar {
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 8px 10px;
      background: var(--obsidian-surface);
      border-bottom: 1px solid var(--obsidian-border);
      flex-wrap: wrap;
      flex-shrink: 0;
    }

    .subbar {
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 6px 10px;
      background: #0F131C;
      border-bottom: 1px solid var(--obsidian-border);
      flex-shrink: 0;
    }

    button {
      background: var(--obsidian-elevated);
      color: var(--obsidian-fg);
      border: 1px solid var(--obsidian-border);
      border-radius: 6px;
      padding: 5px 9px;
      font-size: 11.5px;
      font-weight: 500;
      cursor: pointer;
      white-space: nowrap;
      transition: border-color 0.14s ease, background 0.14s ease;
    }

    button:hover:not(:disabled) {
      border-color: var(--obsidian-accent);
      background: #20283A;
    }

    button:disabled {
      opacity: 0.42;
      cursor: default;
    }

    button.active {
      background: var(--obsidian-accent);
      color: #FFFFFF;
      border-color: var(--obsidian-accent);
      box-shadow: 0 0 12px rgba(124, 92, 255, 0.35);
    }

    button.recording {
      background: rgba(248, 113, 113, 0.2);
      color: #FCA5A5;
      border-color: var(--obsidian-danger);
    }

    button.agent-cta {
      background: var(--obsidian-accent);
      color: #FFFFFF;
      border-color: var(--obsidian-accent);
      font-weight: 600;
    }

    button.agent-cta:hover:not(:disabled) {
      background: var(--obsidian-accent-hover);
    }

    .segmented {
      display: inline-flex;
      border-radius: 6px;
      overflow: hidden;
      border: 1px solid var(--obsidian-border);
    }

    .segmented button {
      border: none;
      border-radius: 0;
      padding: 5px 8px;
    }

    .segmented button + button {
      border-left: 1px solid var(--obsidian-border);
    }

    .url-form {
      display: flex;
      flex: 1;
      min-width: 180px;
      gap: 5px;
    }

    input[type="text"], input[type="number"] {
      background: var(--obsidian-bg);
      color: var(--obsidian-fg);
      border: 1px solid var(--obsidian-border);
      border-radius: 6px;
      padding: 5px 9px;
      font-size: 12px;
      outline: none;
    }

    input[type="text"]:focus, input[type="number"]:focus {
      border-color: var(--obsidian-accent);
      box-shadow: 0 0 0 1px rgba(124, 92, 255, 0.35);
    }

    .url-input {
      flex: 1;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 11.5px;
    }

    .type-som-input {
      width: 64px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    }

    .type-text-input {
      flex: 1;
      min-width: 120px;
    }

    .error-banner {
      display: none;
      padding: 6px 12px;
      background: rgba(248, 113, 113, 0.14);
      border-bottom: 1px solid rgba(248, 113, 113, 0.45);
      color: #FECACA;
      font-size: 11.5px;
      user-select: text;
    }

    .error-banner.visible {
      display: block;
    }

    .workspace {
      flex: 1;
      display: flex;
      min-height: 0;
      overflow: hidden;
    }

    .stage-pane {
      flex: 1;
      display: flex;
      flex-direction: column;
      min-width: 0;
      background: var(--obsidian-bg);
    }

    .canvas-scroll {
      flex: 1;
      display: flex;
      align-items: center;
      justify-content: center;
      overflow: auto;
      padding: 14px;
      position: relative;
    }

    .viewport-frame {
      position: relative;
      background: var(--obsidian-surface);
      border: 1px solid var(--obsidian-border);
      border-radius: 8px;
      box-shadow: 0 18px 42px rgba(0, 0, 0, 0.55);
      overflow: hidden;
      max-width: 100%;
      max-height: 100%;
      display: inline-flex;
      align-items: center;
      justify-content: center;
    }

    .viewport-frame.inspect-mode {
      border-color: var(--obsidian-accent);
      box-shadow: 0 0 0 1px var(--obsidian-accent), 0 18px 42px rgba(0, 0, 0, 0.6);
      cursor: crosshair;
    }

    #screenshotImg {
      display: none;
      max-width: 100%;
      max-height: calc(100vh - 250px);
      width: auto;
      height: auto;
      object-fit: contain;
    }

    #screenshotImg.loaded {
      display: block;
    }

    .empty-stage {
      padding: 42px 32px;
      text-align: center;
      color: var(--obsidian-muted);
      max-width: 420px;
    }

    .empty-stage h3 {
      margin: 0 0 8px 0;
      font-size: 14px;
      color: var(--obsidian-fg);
    }

    .hover-highlight {
      position: absolute;
      display: none;
      border: 2px solid var(--obsidian-accent);
      background: rgba(124, 92, 255, 0.18);
      border-radius: 4px;
      pointer-events: none;
      z-index: 10;
    }

    .hover-tooltip {
      position: absolute;
      left: 0;
      top: -22px;
      background: var(--obsidian-accent);
      color: #FFFFFF;
      font: 600 10px/1.2 ui-monospace, SFMono-Regular, Menlo, monospace;
      padding: 2px 6px;
      border-radius: 4px;
      white-space: nowrap;
      pointer-events: none;
    }

    .inspector-sidebar {
      width: 285px;
      background: var(--obsidian-surface);
      border-left: 1px solid var(--obsidian-border);
      display: flex;
      flex-direction: column;
      min-height: 0;
      flex-shrink: 0;
    }

    .sidebar-header {
      padding: 8px 10px;
      border-bottom: 1px solid var(--obsidian-border);
      display: flex;
      align-items: center;
      justify-content: space-between;
      font-weight: 600;
      font-size: 11.5px;
    }

    .pill {
      background: var(--obsidian-accent-soft);
      color: #C4B5FD;
      border: 1px solid rgba(124, 92, 255, 0.38);
      border-radius: 999px;
      padding: 1px 7px;
      font-size: 10.5px;
      font-weight: 600;
    }

    .pill.danger {
      background: rgba(248, 113, 113, 0.16);
      color: #FCA5A5;
      border-color: rgba(248, 113, 113, 0.4);
    }

    .elements-list {
      flex: 1;
      overflow-y: auto;
      padding: 6px;
      display: flex;
      flex-direction: column;
      gap: 5px;
    }

    .el-card {
      background: var(--obsidian-bg);
      border: 1px solid var(--obsidian-border);
      border-radius: 6px;
      padding: 6px 8px;
      display: flex;
      flex-direction: column;
      gap: 4px;
    }

    .el-card:hover {
      border-color: var(--obsidian-accent);
    }

    .el-row-top {
      display: flex;
      align-items: center;
      gap: 6px;
      min-width: 0;
    }

    .som-badge {
      background: var(--obsidian-accent);
      color: #FFFFFF;
      font: 700 10px/1.2 ui-monospace, SFMono-Regular, Menlo, monospace;
      padding: 2px 5px;
      border-radius: 4px;
      flex-shrink: 0;
    }

    .el-tag {
      color: #93C5FD;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 11px;
      flex-shrink: 0;
    }

    .el-label {
      color: var(--obsidian-fg);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      flex: 1;
    }

    .el-meta {
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 10px;
      color: var(--obsidian-muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      user-select: text;
    }

    .el-source {
      color: var(--obsidian-ok);
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 10px;
    }

    .el-actions {
      display: flex;
      gap: 4px;
      margin-top: 2px;
    }

    .el-actions button {
      padding: 2px 6px;
      font-size: 10.5px;
    }

    .drawer {
      height: 155px;
      background: var(--obsidian-surface);
      border-top: 1px solid var(--obsidian-border);
      display: flex;
      flex-direction: column;
      flex-shrink: 0;
    }

    .drawer-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 6px 10px;
      border-bottom: 1px solid var(--obsidian-border);
      gap: 8px;
    }

    .drawer-left, .drawer-right {
      display: flex;
      align-items: center;
      gap: 6px;
    }

    .drawer-logs {
      flex: 1;
      overflow-y: auto;
      padding: 6px 10px;
      font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
      font-size: 11px;
      user-select: text;
    }

    .log-row {
      padding: 2px 0;
      border-bottom: 1px solid rgba(35, 43, 61, 0.5);
      word-break: break-word;
    }

    .log-row.err { color: var(--obsidian-danger); }
    .log-row.warn { color: var(--obsidian-warn); }
    .log-row.info { color: var(--obsidian-fg); }

    .statusbar {
      padding: 4px 10px;
      background: #080A0E;
      border-top: 1px solid var(--obsidian-border);
      color: var(--obsidian-muted);
      font-size: 11px;
      display: flex;
      justify-content: space-between;
      align-items: center;
      flex-shrink: 0;
    }
  </style>
</head>
<body>
  <div class="toolbar">
    <button id="backBtn" title="Back" disabled>&#8592;</button>
    <button id="reloadBtn" title="Reload Snapshot">&#8635;</button>
    <form class="url-form" id="urlForm">
      <input
        id="urlInput"
        class="url-input"
        type="text"
        value="http://localhost:3000"
        placeholder="http://localhost:3000"
        spellcheck="false"
      />
      <button type="submit" id="goBtn">Go</button>
    </form>
    <div class="segmented" title="Viewport Switcher">
      <button id="vpDesktopBtn" class="active" type="button">Desktop 1280x800</button>
      <button id="vpMobileBtn" type="button">Mobile 390x844</button>
    </div>
    <button id="somToggleBtn" class="active" type="button" title="Toggle Set-of-Mark [1][2] Overlay">
      [1][2] SoM
    </button>
    <button id="inspectToggleBtn" type="button" title="Inspect Element -> Jump to Source & Inline Edit (Cmd+K)">
      Inspect &#8594; Code (Cmd+K)
    </button>
    <button id="recordMp4Btn" type="button" title="Record Walkthrough Video (.mp4)">
      &#9679; Record .mp4
    </button>
    <button id="exportPdfBtn" type="button" title="Export Page to PDF">
      Export PDF
    </button>
  </div>

  <div class="subbar">
    <span style="color: var(--obsidian-muted);">Quick Act:</span>
    <input id="typeSomId" class="type-som-input" type="number" min="1" max="120" placeholder="[id]" />
    <input id="typeTextInput" class="type-text-input" type="text" placeholder="Type text into [id] or focused input..." />
    <label style="display:inline-flex;align-items:center;gap:4px;color:var(--obsidian-muted);">
      <input id="pressEnterChk" type="checkbox" checked /> Enter
    </label>
    <button id="sendTypeBtn" type="button">Type</button>
    <button id="scrollUpBtn" type="button" title="Scroll Up">&#8593; Scroll</button>
    <button id="scrollDownBtn" type="button" title="Scroll Down">&#8595; Scroll</button>
  </div>

  <div id="errorBanner" class="error-banner"></div>

  <div class="workspace">
    <div class="stage-pane">
      <div class="canvas-scroll">
        <div id="viewportFrame" class="viewport-frame">
          <img id="screenshotImg" alt="CDP Live Viewport" />
          <div id="hoverHighlight" class="hover-highlight">
            <div id="hoverTooltip" class="hover-tooltip"></div>
          </div>
          <div id="emptyStage" class="empty-stage">
            <h3>Rursor Chromium CDP Studio</h3>
            <p>
              Enter a local dev server URL (e.g. <code>http://localhost:3000</code>) and press <b>Go</b>
              to launch headless Chromium with Set-of-Mark <code>[1][2]</code> computer-use badges,
              <b>Inspect &#8594; Code (Cmd+K)</b>, and <b>.mp4</b> walkthrough recording.
            </p>
          </div>
        </div>
      </div>

      <div class="drawer">
        <div class="drawer-header">
          <div class="drawer-left">
            <strong>Console &amp; Network Diagnostics</strong>
            <span id="diagCountPill" class="pill">0 issues</span>
          </div>
          <div class="drawer-right">
            <button id="clearDiagBtn" type="button">Clear</button>
            <button id="sendAgentBtn" class="agent-cta" type="button">
              Send Page Errors + Screenshot to Rursor Agent
            </button>
          </div>
        </div>
        <div id="drawerLogs" class="drawer-logs"></div>
      </div>
    </div>

    <aside class="inspector-sidebar">
      <div class="sidebar-header">
        <span>Set-of-Mark Elements</span>
        <span id="somCountPill" class="pill">0</span>
      </div>
      <div id="elementsList" class="elements-list"></div>
    </aside>
  </div>

  <div class="statusbar">
    <span id="statusText">Ready</span>
    <span id="coordsText">1280 x 800</span>
  </div>

  <script nonce="${nonce}">
    (function () {
      const vscode = acquireVsCodeApi();

      const backBtn = document.getElementById('backBtn');
      const reloadBtn = document.getElementById('reloadBtn');
      const urlForm = document.getElementById('urlForm');
      const urlInput = document.getElementById('urlInput');
      const goBtn = document.getElementById('goBtn');
      const vpDesktopBtn = document.getElementById('vpDesktopBtn');
      const vpMobileBtn = document.getElementById('vpMobileBtn');
      const somToggleBtn = document.getElementById('somToggleBtn');
      const inspectToggleBtn = document.getElementById('inspectToggleBtn');
      const recordMp4Btn = document.getElementById('recordMp4Btn');
      const exportPdfBtn = document.getElementById('exportPdfBtn');

      const typeSomId = document.getElementById('typeSomId');
      const typeTextInput = document.getElementById('typeTextInput');
      const pressEnterChk = document.getElementById('pressEnterChk');
      const sendTypeBtn = document.getElementById('sendTypeBtn');
      const scrollUpBtn = document.getElementById('scrollUpBtn');
      const scrollDownBtn = document.getElementById('scrollDownBtn');

      const errorBanner = document.getElementById('errorBanner');
      const viewportFrame = document.getElementById('viewportFrame');
      const screenshotImg = document.getElementById('screenshotImg');
      const hoverHighlight = document.getElementById('hoverHighlight');
      const hoverTooltip = document.getElementById('hoverTooltip');
      const emptyStage = document.getElementById('emptyStage');
      const elementsList = document.getElementById('elementsList');
      const somCountPill = document.getElementById('somCountPill');
      const diagCountPill = document.getElementById('diagCountPill');
      const drawerLogs = document.getElementById('drawerLogs');
      const clearDiagBtn = document.getElementById('clearDiagBtn');
      const sendAgentBtn = document.getElementById('sendAgentBtn');
      const statusText = document.getElementById('statusText');
      const coordsText = document.getElementById('coordsText');

      let state = {
        viewportWidth: 1280,
        viewportHeight: 800,
        inspectMode: false,
        busy: false,
        elements: [],
      };

      function mapClientToViewport(clientX, clientY) {
        const rect = screenshotImg.getBoundingClientRect();
        if (!rect.width || !rect.height) {
          return { x: 0, y: 0 };
        }
        const x = ((clientX - rect.left) / rect.width) * state.viewportWidth;
        const y = ((clientY - rect.top) / rect.height) * state.viewportHeight;
        return {
          x: Math.max(0, Math.min(state.viewportWidth, x)),
          y: Math.max(0, Math.min(state.viewportHeight, y)),
        };
      }

      function findElementAt(x, y) {
        let best = null;
        let bestArea = Infinity;
        for (let i = 0; i < state.elements.length; i++) {
          const el = state.elements[i];
          const b = el.bounds;
          if (x >= b.x && x <= b.x + b.width && y >= b.y && y <= b.y + b.height) {
            const area = Math.max(1, b.width * b.height);
            if (area < bestArea) {
              bestArea = area;
              best = el;
            }
          }
        }
        return best;
      }

      function showElementOverlay(el) {
        if (!el || !screenshotImg.classList.contains('loaded')) {
          hoverHighlight.style.display = 'none';
          return;
        }
        const rect = screenshotImg.getBoundingClientRect();
        if (!rect.width || !rect.height) {
          hoverHighlight.style.display = 'none';
          return;
        }
        const scaleX = rect.width / state.viewportWidth;
        const scaleY = rect.height / state.viewportHeight;
        hoverHighlight.style.display = 'block';
        hoverHighlight.style.left = (el.bounds.x * scaleX) + 'px';
        hoverHighlight.style.top = (el.bounds.y * scaleY) + 'px';
        hoverHighlight.style.width = Math.max(6, el.bounds.width * scaleX) + 'px';
        hoverHighlight.style.height = Math.max(6, el.bounds.height * scaleY) + 'px';
        const srcTag = el.sourceHint
          ? ' -> ' + el.sourceHint.file.split('/').pop() + ':' + el.sourceHint.line
          : '';
        hoverTooltip.textContent = '[' + el.id + '] <' + el.tag + '>' + srcTag;
      }

      urlForm.addEventListener('submit', function (e) {
        e.preventDefault();
        vscode.postMessage({ type: 'navigate', url: urlInput.value });
      });

      backBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'back' });
      });

      reloadBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'reload' });
      });

      vpDesktopBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'setViewport', viewport: 'desktop' });
      });

      vpMobileBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'setViewport', viewport: 'mobile' });
      });

      somToggleBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'toggleSom' });
      });

      inspectToggleBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'toggleInspectMode' });
      });

      recordMp4Btn.addEventListener('click', function () {
        vscode.postMessage({ type: 'recordWalkthrough' });
      });

      exportPdfBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'exportPdf' });
      });

      sendTypeBtn.addEventListener('click', function () {
        const text = typeTextInput.value;
        if (!text) return;
        const somId = parseInt(typeSomId.value, 10);
        vscode.postMessage({
          type: 'typeText',
          somId: Number.isFinite(somId) && somId > 0 ? somId : undefined,
          text: text,
          pressEnter: Boolean(pressEnterChk.checked),
        });
        typeTextInput.value = '';
      });

      typeTextInput.addEventListener('keydown', function (e) {
        if (e.key === 'Enter') {
          e.preventDefault();
          sendTypeBtn.click();
        }
      });

      scrollUpBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'scroll', deltaX: 0, deltaY: -360 });
      });

      scrollDownBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'scroll', deltaX: 0, deltaY: 360 });
      });

      clearDiagBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'clearDiagnostics' });
      });

      sendAgentBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'sendErrorsToAgent' });
      });

      screenshotImg.addEventListener('mousemove', function (e) {
        const pt = mapClientToViewport(e.clientX, e.clientY);
        coordsText.textContent =
          Math.round(pt.x) + ', ' + Math.round(pt.y) + ' / ' + state.viewportWidth + 'x' + state.viewportHeight;
        if (state.inspectMode) {
          const hit = findElementAt(pt.x, pt.y);
          showElementOverlay(hit);
        } else {
          hoverHighlight.style.display = 'none';
        }
      });

      screenshotImg.addEventListener('mouseleave', function () {
        hoverHighlight.style.display = 'none';
      });

      screenshotImg.addEventListener('click', function (e) {
        if (state.busy) return;
        const pt = mapClientToViewport(e.clientX, e.clientY);
        vscode.postMessage({ type: 'canvasClick', x: pt.x, y: pt.y });
      });

      function renderElements(elements) {
        elementsList.textContent = '';
        somCountPill.textContent = String(elements.length);
        if (!elements.length) {
          const empty = document.createElement('div');
          empty.style.color = 'var(--obsidian-muted)';
          empty.style.padding = '12px 6px';
          empty.textContent = 'No interactive elements indexed yet.';
          elementsList.appendChild(empty);
          return;
        }

        for (let i = 0; i < elements.length; i++) {
          const el = elements[i];
          const card = document.createElement('div');
          card.className = 'el-card';

          card.addEventListener('mouseenter', function () {
            showElementOverlay(el);
          });
          card.addEventListener('mouseleave', function () {
            hoverHighlight.style.display = 'none';
          });

          const top = document.createElement('div');
          top.className = 'el-row-top';

          const badge = document.createElement('span');
          badge.className = 'som-badge';
          badge.textContent = '[' + el.id + ']';

          const tag = document.createElement('span');
          tag.className = 'el-tag';
          tag.textContent = '<' + el.tag + '>';

          const lbl = document.createElement('span');
          lbl.className = 'el-label';
          lbl.textContent = el.label || el.placeholder || el.selector;

          top.appendChild(badge);
          top.appendChild(tag);
          top.appendChild(lbl);
          card.appendChild(top);

          const meta = document.createElement('div');
          meta.className = 'el-meta';
          meta.textContent = el.selector;
          card.appendChild(meta);

          if (el.sourceHint && el.sourceHint.file) {
            const src = document.createElement('div');
            src.className = 'el-source';
            src.textContent = el.sourceHint.file + ':' + el.sourceHint.line;
            card.appendChild(src);
          }

          const actions = document.createElement('div');
          actions.className = 'el-actions';

          const clickBtn = document.createElement('button');
          clickBtn.textContent = 'Click';
          clickBtn.addEventListener('click', function () {
            vscode.postMessage({ type: 'elementClick', somId: el.id });
          });

          const codeBtn = document.createElement('button');
          codeBtn.textContent = el.sourceHint ? 'Open Code (Cmd+K)' : 'Ask Agent';
          codeBtn.addEventListener('click', function () {
            vscode.postMessage({ type: 'elementInspect', somId: el.id });
          });

          const focusTypeBtn = document.createElement('button');
          focusTypeBtn.textContent = 'Type';
          focusTypeBtn.addEventListener('click', function () {
            typeSomId.value = String(el.id);
            typeTextInput.focus();
          });

          actions.appendChild(clickBtn);
          actions.appendChild(codeBtn);
          actions.appendChild(focusTypeBtn);
          card.appendChild(actions);

          elementsList.appendChild(card);
        }
      }

      function renderDiagnostics(diag) {
        drawerLogs.textContent = '';
        const consoleLogs = (diag && diag.consoleLogs) || [];
        const networkErrors = (diag && diag.networkErrors) || [];
        const errConsoleCount = consoleLogs.filter(function (c) {
          return c.level === 'error' || c.level === 'exception';
        }).length;
        const issueTotal = errConsoleCount + networkErrors.length;

        diagCountPill.textContent = issueTotal + (issueTotal === 1 ? ' issue' : ' issues');
        diagCountPill.className = issueTotal > 0 ? 'pill danger' : 'pill';

        if (!consoleLogs.length && !networkErrors.length) {
          const none = document.createElement('div');
          none.style.color = 'var(--obsidian-muted)';
          none.textContent = 'No console logs or network errors captured on this page.';
          drawerLogs.appendChild(none);
          return;
        }

        for (let i = 0; i < networkErrors.length; i++) {
          const net = networkErrors[i];
          const row = document.createElement('div');
          row.className = 'log-row err';
          const statusPart = net.status ? 'HTTP ' + net.status : net.errorText;
          row.textContent = '[NET ' + net.method + '] ' + net.url + ' -> ' + statusPart;
          drawerLogs.appendChild(row);
        }

        for (let j = 0; j < consoleLogs.length; j++) {
          const c = consoleLogs[j];
          const row = document.createElement('div');
          const isErr = c.level === 'error' || c.level === 'exception';
          const isWarn = c.level === 'warning' || c.level === 'warn';
          row.className = 'log-row ' + (isErr ? 'err' : isWarn ? 'warn' : 'info');
          const loc = c.url ? ' (' + c.url + (c.line ? ':' + c.line : '') + ')' : '';
          row.textContent = '[' + String(c.level).toUpperCase() + '] ' + c.text + loc;
          drawerLogs.appendChild(row);
        }
      }

      window.addEventListener('message', function (event) {
        const msg = event.data;
        if (!msg || msg.type !== 'studioState') return;

        state.viewportWidth = msg.viewportWidth || 1280;
        state.viewportHeight = msg.viewportHeight || 800;
        state.inspectMode = Boolean(msg.inspectMode);
        state.busy = Boolean(msg.busy);
        state.elements = (msg.snapshot && msg.snapshot.elements) || [];

        if (document.activeElement !== urlInput && msg.url) {
          urlInput.value = msg.url;
        }
        backBtn.disabled = !msg.canGoBack || state.busy;
        reloadBtn.disabled = state.busy;
        goBtn.disabled = state.busy;
        exportPdfBtn.disabled = state.busy || !msg.snapshot;

        vpDesktopBtn.classList.toggle('active', msg.viewportPreset === 'desktop');
        vpMobileBtn.classList.toggle('active', msg.viewportPreset === 'mobile');
        somToggleBtn.classList.toggle('active', Boolean(msg.drawSomBadges));
        inspectToggleBtn.classList.toggle('active', state.inspectMode);
        viewportFrame.classList.toggle('inspect-mode', state.inspectMode);

        if (msg.isRecordingWalkthrough) {
          recordMp4Btn.classList.add('recording');
          recordMp4Btn.textContent = '■ Save .mp4 (' + (msg.recordedStepCount || 1) + ')';
        } else {
          recordMp4Btn.classList.remove('recording');
          recordMp4Btn.textContent = '● Record .mp4';
        }

        if (msg.errorBanner) {
          errorBanner.textContent = msg.errorBanner;
          errorBanner.classList.add('visible');
        } else {
          errorBanner.textContent = '';
          errorBanner.classList.remove('visible');
        }

        if (msg.snapshot && msg.snapshot.screenshotBase64) {
          screenshotImg.src = 'data:image/png;base64,' + msg.snapshot.screenshotBase64;
          screenshotImg.classList.add('loaded');
          emptyStage.style.display = 'none';
        } else {
          screenshotImg.classList.remove('loaded');
          emptyStage.style.display = 'block';
        }

        statusText.textContent = msg.statusMessage || 'Ready';
        coordsText.textContent = state.viewportWidth + ' x ' + state.viewportHeight;

        renderElements(state.elements);
        renderDiagnostics(msg.diagnostics);
      });

      vscode.postMessage({ type: 'ready' });
    })();
  </script>
</body>
</html>`;
  }
}
