import * as crypto from 'node:crypto';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

export type StudioTab = 'gallery' | 'deck' | 'video' | 'office';
export type DeckTheme = 'obsidian' | 'executive' | 'minimal';
export type SlideLayoutKind =
  | 'title'
  | 'bullets'
  | 'two_column'
  | 'metrics'
  | 'code'
  | 'quote';

export type ArtifactCategory = 'deck' | 'video' | 'spreadsheet' | 'document' | 'image' | 'genui';

export interface SlideMetricSpec {
  label: string;
  value: string;
  delta: string;
}

export interface SlideSpec {
  layout: SlideLayoutKind;
  title: string;
  subtitle: string;
  bullets: string[];
  rightBullets: string[];
  metrics: SlideMetricSpec[];
  code?: string;
  speakerNotes?: string;
}

export interface PresentationSpec {
  title: string;
  subtitle: string;
  author: string;
  theme: DeckTheme;
  outputPath: string;
  slides: SlideSpec[];
}

export interface PresentationArtifactResult {
  pptxPath: string;
  htmlDeckPath: string;
  relativePptxPath?: string;
  relativeHtmlDeckPath?: string;
  slideCount: number;
  bytes: number;
}

export interface VideoSceneSpec {
  heading: string;
  subheading: string;
  bullets: string[];
  codeSnippet?: string;
  narration?: string;
  durationSecs: number;
  accentHex?: string;
}

export interface VideoStoryboardSpec {
  title: string;
  outputPath: string;
  width?: number;
  height?: number;
  fps?: number;
  voice?: string;
  scenes: VideoSceneSpec[];
}

export interface VeoVideoSpec {
  prompt: string;
  outputPath: string;
  model?: string;
  aspectRatio?: string;
  durationSecs?: number;
}

export interface VideoArtifactResult {
  videoPath: string;
  relativeVideoPath?: string;
  width: number;
  height: number;
  fps: number;
  durationSecs: number;
  sceneCount: number;
  hasVoiceover: boolean;
  engine: string;
  bytes: number;
}

export interface ImageAssetSpec {
  prompt: string;
  outputPath: string;
  mode?: 'auto' | 'cloud' | 'local';
  model?: string;
  width?: number;
  height?: number;
  svgMarkup?: string;
  subtitle?: string;
  bullets?: string[];
  accentHex?: string;
}

export interface ImageAssetResult {
  imagePath: string;
  relativeImagePath?: string;
  svgPath?: string;
  width: number;
  height: number;
  mimeType: string;
  engine: string;
  bytes: number;
}

export type SpreadsheetCellInput =
  | string
  | number
  | boolean
  | null
  | { type: 'string' | 'number' | 'formula'; value: string | number };

export interface SheetSpec {
  name: string;
  headers: string[];
  rows: SpreadsheetCellInput[][];
}

export interface SpreadsheetSpec {
  outputPath: string;
  sheets: SheetSpec[];
}

export interface SpreadsheetArtifactResult {
  xlsxPath: string;
  relativeXlsxPath?: string;
  sheetCount: number;
  rowCount: number;
  bytes: number;
}

export interface DocTableSpec {
  headers: string[];
  rows: string[][];
}

export interface DocSectionSpec {
  heading: string;
  paragraphs: string[];
  bullets: string[];
  codeBlock?: string;
  table?: DocTableSpec;
}

export interface DocumentSpec {
  title: string;
  subtitle: string;
  outputPath: string;
  sections: DocSectionSpec[];
}

export interface DocumentArtifactResult {
  docxPath: string;
  relativeDocxPath?: string;
  sectionCount: number;
  bytes: number;
}

export interface WorkspaceArtifactItem {
  id: string;
  name: string;
  relativePath: string;
  fsPath: string;
  ext: string;
  category: ArtifactCategory;
  sizeBytes: number;
  mtimeMs: number;
  companionDeckFsPath?: string;
  companionPptxFsPath?: string;
}

export interface InlineGenUiOptions {
  frameId?: string;
  minHeightPx?: number;
  maxHeightPx?: number;
}

interface StudioIncomingMessage {
  type: string;
  tab?: StudioTab;
  fsPath?: string;
  prompt?: string;
  title?: string;
  subtitle?: string;
  author?: string;
  theme?: DeckTheme;
  slideCount?: number;
  outputPath?: string;
  useWorkspaceSearch?: boolean;
  engine?: 'local' | 'veo';
  width?: number;
  height?: number;
  fps?: number;
  voice?: string;
  scenes?: VideoSceneSpec[];
  officeKind?: 'xlsx' | 'docx' | 'image';
  svgMarkup?: string;
  genUiTitle?: string;
  genUiHtml?: string;
}

function createNonce(): string {
  return crypto.randomBytes(16).toString('hex');
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return '0 B';
  }
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(1)} KB`;
  }
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

function classifyArtifact(fsPath: string): ArtifactCategory {
  const lower = fsPath.toLowerCase();
  if (lower.endsWith('.pptx') || lower.endsWith('.deck.html')) {
    return 'deck';
  }
  if (lower.endsWith('.mp4') || lower.endsWith('.webm') || lower.endsWith('.mov')) {
    return 'video';
  }
  if (lower.endsWith('.xlsx') || lower.endsWith('.csv')) {
    return 'spreadsheet';
  }
  if (lower.endsWith('.docx')) {
    return 'document';
  }
  if (
    lower.endsWith('.png') ||
    lower.endsWith('.svg') ||
    lower.endsWith('.jpg') ||
    lower.endsWith('.jpeg') ||
    lower.endsWith('.webp')
  ) {
    return 'image';
  }
  return 'genui';
}

function extractJsonObject<T>(rawText: string): T | undefined {
  const trimmed = rawText.trim();
  const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/i);
  const candidate = fenced ? fenced[1].trim() : trimmed;
  const firstBrace = candidate.indexOf('{');
  const lastBrace = candidate.lastIndexOf('}');
  if (firstBrace === -1 || lastBrace <= firstBrace) {
    return undefined;
  }
  try {
    return JSON.parse(candidate.slice(firstBrace, lastBrace + 1)) as T;
  } catch {
    return undefined;
  }
}

// ============================================================================
// 3. Sandboxed Inline Generative UI Helper (`buildInlineGenUiSrcdoc`)
// ============================================================================

/**
 * Host listener snippet that Agent chat webviews can include to auto-resize
 * `<iframe data-rursor-genui-id="...">` elements in response to `ResizeObserver` `postMessage`.
 */
export const INLINE_GENUI_HOST_RESIZE_SCRIPT = `
window.addEventListener('message', (event) => {
  const data = event && event.data;
  if (!data || data.type !== 'rursor-genui-resize' || typeof data.frameId !== 'string') {
    return;
  }
  const frame = document.querySelector('iframe[data-rursor-genui-id="' + data.frameId.replace(/"/g, '') + '"]');
  if (frame && typeof data.height === 'number' && Number.isFinite(data.height)) {
    const minH = Number(frame.getAttribute('data-min-height') || 120);
    const maxH = Number(frame.getAttribute('data-max-height') || 680);
    const clamped = Math.max(minH, Math.min(maxH, Math.ceil(data.height)));
    frame.style.height = clamped + 'px';
  }
});
`;

/**
 * Builds the inner HTML5 document injected into `<iframe srcdoc="...">` with
 * Rursor Obsidian CSS variables (`--bg`, `--card`, `--accent`, `--fg`) and
 * an automatic `ResizeObserver` `postMessage` height reporter.
 */
export function buildInlineGenUiDocument(
  title: string,
  htmlPayload: string,
  frameId = 'genui_default'
): string {
  const safeTitle = escapeHtml(title.trim() || 'Interactive Widget');
  const safeFrameIdJson = JSON.stringify(frameId);
  const safeTitleJson = JSON.stringify(title.trim() || 'Interactive Widget');

  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data: blob:; font-src data:;">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${safeTitle}</title>
<style>
  :root {
    --bg: #0B0D12;
    --card: #131822;
    --accent: #7C5CFF;
    --accent-alt: #2AD8A7;
    --fg: #F3F5F9;
    --muted: #9AA4B8;
    --border: #252E42;
    --radius: 10px;
  }
  * { box-sizing: border-box; }
  html, body {
    margin: 0;
    padding: 0;
    background: var(--bg);
    color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Inter', sans-serif;
    font-size: 13px;
    line-height: 1.5;
    overflow-x: hidden;
  }
  .rursor-genui-root {
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .rursor-genui-hdr {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding-bottom: 8px;
    border-bottom: 1px solid var(--border);
  }
  .rursor-genui-title {
    font-weight: 600;
    font-size: 12px;
    letter-spacing: 0.02em;
    color: var(--fg);
  }
  .rursor-genui-pill {
    font-size: 10px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    padding: 2px 7px;
    border-radius: 999px;
    background: rgba(124, 92, 255, 0.16);
    color: var(--accent);
    border: 1px solid rgba(124, 92, 255, 0.35);
  }
  .rursor-genui-body {
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    padding: 12px;
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  th, td {
    text-align: left;
    padding: 7px 10px;
    border-bottom: 1px solid var(--border);
  }
  th {
    color: var(--muted);
    font-weight: 600;
    text-transform: uppercase;
    font-size: 11px;
    letter-spacing: 0.04em;
  }
  button {
    background: var(--accent);
    color: #ffffff;
    border: none;
    border-radius: 6px;
    padding: 5px 11px;
    font-size: 12px;
    font-weight: 600;
    cursor: pointer;
  }
  button:hover { opacity: 0.92; }
  input, select {
    background: var(--bg);
    color: var(--fg);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 5px 8px;
    font-size: 12px;
  }
</style>
</head>
<body>
  <div class="rursor-genui-root" id="rursorGenUiRoot">
    <div class="rursor-genui-hdr">
      <span class="rursor-genui-title">${safeTitle}</span>
      <span class="rursor-genui-pill">Sandboxed GenUI</span>
    </div>
    <div class="rursor-genui-body" id="rursorGenUiMount">
${htmlPayload}
    </div>
  </div>
  <script>
  (function() {
    const frameId = ${safeFrameIdJson};
    const widgetTitle = ${safeTitleJson};
    const root = document.getElementById('rursorGenUiRoot');
    let lastHeight = 0;
    function postHeight() {
      if (!root) return;
      const rect = root.getBoundingClientRect();
      const h = Math.ceil(Math.max(rect.height, document.documentElement.scrollHeight));
      if (Math.abs(h - lastHeight) >= 2) {
        lastHeight = h;
        window.parent.postMessage({
          type: 'rursor-genui-resize',
          frameId: frameId,
          title: widgetTitle,
          height: h
        }, '*');
      }
    }
    if (typeof ResizeObserver !== 'undefined' && root) {
      const ro = new ResizeObserver(() => postHeight());
      ro.observe(root);
    }
    window.addEventListener('load', postHeight);
    setTimeout(postHeight, 40);
    setTimeout(postHeight, 220);
  })();
  </script>
</body>
</html>`;
}

/**
 * Produces a safe `sandbox="allow-scripts"` `<iframe srcdoc="...">` HTML snippet
 * (strictly isolated without `allow-same-origin`) injected with Rursor Obsidian CSS
 * variables (`--bg`, `--card`, `--accent`, `--fg`) and auto-height `ResizeObserver` `postMessage`.
 */
export function buildInlineGenUiSrcdoc(
  title: string,
  htmlPayload: string,
  options?: InlineGenUiOptions
): string {
  const frameId =
    options?.frameId?.replace(/[^a-zA-Z0-9_-]/g, '') ||
    `genui_${crypto.randomBytes(6).toString('hex')}`;
  const minHeight = Math.max(80, options?.minHeightPx ?? 160);
  const maxHeight = Math.max(minHeight, options?.maxHeightPx ?? 680);

  const srcdocDocument = buildInlineGenUiDocument(title, htmlPayload, frameId);
  const escapedSrcdocAttr = escapeHtml(srcdocDocument);
  const escapedTitleAttr = escapeHtml(title.trim() || 'Rursor Generative UI');

  return `<iframe class="rursor-inline-genui-frame" title="${escapedTitleAttr}" data-rursor-genui-id="${frameId}" data-min-height="${minHeight}" data-max-height="${maxHeight}" sandbox="allow-scripts" referrerpolicy="no-referrer" style="width:100%;height:${minHeight}px;border:1px solid #252E42;border-radius:10px;background:#0B0D12;display:block;" srcdoc="${escapedSrcdocAttr}"></iframe>`;
}

// ============================================================================
// 2. In-Editor Slide Deck & Video Preview Panel (`openArtifactPreview`)
// ============================================================================

async function findSiblingFile(targetPath: string, newSuffix: string): Promise<string | undefined> {
  const dir = path.dirname(targetPath);
  const base = path.basename(targetPath);
  const stem = base.endsWith('.deck.html')
    ? base.slice(0, -'.deck.html'.length)
    : base.replace(/\.[^.]+$/, '');
  const candidate = path.join(dir, `${stem}${newSuffix}`);
  try {
    const stat = await fs.stat(candidate);
    return stat.isFile() ? candidate : undefined;
  } catch {
    return undefined;
  }
}

/**
 * Opens an in-editor Rursor Artifact Preview tab for `.deck.html` / `.pptx` slide decks,
 * `.mp4` / `.webm` videos (via `webview.asWebviewUri`), images, or Office documents.
 */
export async function openArtifactPreview(
  uri: vscode.Uri,
  extensionUri?: vscode.Uri
): Promise<vscode.WebviewPanel> {
  const fsPath = uri.fsPath;
  const lower = fsPath.toLowerCase();
  const baseName = path.basename(fsPath);

  // If caller passed a `.pptx` that has a synchronized `<stem>.deck.html`, resolve both!
  let deckHtmlPath: string | undefined;
  let pptxPath: string | undefined;

  if (lower.endsWith('.deck.html')) {
    deckHtmlPath = fsPath;
    pptxPath = await findSiblingFile(fsPath, '.pptx');
  } else if (lower.endsWith('.pptx')) {
    pptxPath = fsPath;
    deckHtmlPath = await findSiblingFile(fsPath, '.deck.html');
  }

  const workspaceRoots = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri);
  const resourceRoots: vscode.Uri[] = [
    vscode.Uri.file(path.dirname(fsPath)),
    ...workspaceRoots,
    ...(extensionUri ? [extensionUri] : []),
  ];

  const panel = vscode.window.createWebviewPanel(
    'rursor.artifactPreview',
    `Preview: ${baseName}`,
    vscode.ViewColumn.Beside,
    {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: resourceRoots,
    }
  );

  const renderPreview = async (): Promise<void> => {
    const nonce = createNonce();
    const cspSource = panel.webview.cspSource;

    if (deckHtmlPath) {
      const deckRawHtml = await fs.readFile(deckHtmlPath, 'utf8').catch(() => '');
      const escapedDeckSrcdoc = escapeHtml(deckRawHtml);
      const hasPptx = Boolean(pptxPath);
      const pptxLabel = pptxPath ? path.basename(pptxPath) : '';

      panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}' 'unsafe-inline'; frame-src 'self' data: blob:; img-src ${cspSource} data: blob:;">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${escapeHtml(baseName)}</title>
<style>
  :root {
    --bg: #0B0D12;
    --card: #131822;
    --border: #252E42;
    --fg: #F3F5F9;
    --muted: #9AA4B8;
    --accent: #7C5CFF;
    --accent-alt: #2AD8A7;
  }
  * { box-sizing: border-box; }
  html, body {
    margin: 0; padding: 0; width: 100%; height: 100%;
    background: var(--bg); color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', sans-serif;
    display: flex; flex-direction: column; overflow: hidden;
  }
  .bar {
    display: flex; align-items: center; justify-content: space-between;
    padding: 8px 14px; background: var(--card); border-bottom: 1px solid var(--border);
    gap: 10px; flex-shrink: 0;
  }
  .bar-left { display: flex; align-items: center; gap: 10px; min-width: 0; }
  .pill {
    font-size: 10px; font-weight: 700; letter-spacing: 0.08em; text-transform: uppercase;
    padding: 3px 8px; border-radius: 999px; background: rgba(124,92,255,0.18);
    color: var(--accent); border: 1px solid rgba(124,92,255,0.4);
  }
  .filename { font-size: 13px; font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .actions { display: flex; align-items: center; gap: 8px; flex-shrink: 0; }
  button {
    background: #1A2232; color: var(--fg); border: 1px solid var(--border);
    border-radius: 6px; padding: 5px 11px; font-size: 12px; font-weight: 500; cursor: pointer;
  }
  button:hover { border-color: var(--accent); }
  button.primary { background: var(--accent); border-color: var(--accent); color: #fff; font-weight: 600; }
  iframe { flex: 1; width: 100%; border: none; background: #06080d; }
</style>
</head>
<body>
  <div class="bar">
    <div class="bar-left">
      <span class="pill">16:9 Slide Deck</span>
      <span class="filename">${escapeHtml(path.basename(deckHtmlPath))}</span>
    </div>
    <div class="actions">
      ${
        hasPptx
          ? `<button class="primary" id="openPptxBtn" title="${escapeHtml(pptxLabel)}">Open .pptx in Keynote / PowerPoint</button>`
          : ''
      }
      <button id="revealBtn">Reveal in Finder</button>
      <button id="reloadBtn">Reload</button>
    </div>
  </div>
  <iframe id="deckFrame" sandbox="allow-scripts" srcdoc="${escapedDeckSrcdoc}"></iframe>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const openPptxBtn = document.getElementById('openPptxBtn');
    if (openPptxBtn) {
      openPptxBtn.addEventListener('click', () => vscode.postMessage({ type: 'openPptx' }));
    }
    document.getElementById('revealBtn').addEventListener('click', () => {
      vscode.postMessage({ type: 'reveal' });
    });
    document.getElementById('reloadBtn').addEventListener('click', () => {
      vscode.postMessage({ type: 'reload' });
    });
    const frame = document.getElementById('deckFrame');
    window.addEventListener('load', () => frame && frame.focus());
  </script>
</body>
</html>`;
      return;
    }

    if (lower.endsWith('.mp4') || lower.endsWith('.webm') || lower.endsWith('.mov')) {
      const mediaWebviewUri = panel.webview.asWebviewUri(uri);
      const stat = await fs.stat(fsPath).catch(() => undefined);
      const sizeStr = stat ? formatBytes(stat.size) : '';

      panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; media-src ${cspSource} blob:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${escapeHtml(baseName)}</title>
<style>
  :root {
    --bg: #0B0D12;
    --card: #131822;
    --border: #252E42;
    --fg: #F3F5F9;
    --muted: #9AA4B8;
    --accent: #7C5CFF;
    --accent-alt: #2AD8A7;
  }
  * { box-sizing: border-box; }
  html, body {
    margin: 0; padding: 0; width: 100%; height: 100%;
    background: var(--bg); color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', sans-serif;
    display: flex; flex-direction: column; overflow: hidden;
  }
  .bar {
    display: flex; align-items: center; justify-content: space-between;
    padding: 8px 14px; background: var(--card); border-bottom: 1px solid var(--border);
  }
  .bar-left { display: flex; align-items: center; gap: 10px; }
  .pill {
    font-size: 10px; font-weight: 700; letter-spacing: 0.08em; text-transform: uppercase;
    padding: 3px 8px; border-radius: 999px; background: rgba(42,216,167,0.16);
    color: var(--accent-alt); border: 1px solid rgba(42,216,167,0.38);
  }
  .meta { color: var(--muted); font-size: 12px; }
  .actions { display: flex; align-items: center; gap: 8px; }
  button, select {
    background: #1A2232; color: var(--fg); border: 1px solid var(--border);
    border-radius: 6px; padding: 5px 10px; font-size: 12px; cursor: pointer;
  }
  button:hover, select:hover { border-color: var(--accent); }
  .stage {
    flex: 1; display: flex; align-items: center; justify-content: center;
    padding: 20px; overflow: hidden; background: radial-gradient(circle at 80% 15%, rgba(124,92,255,0.14), transparent 55%), var(--bg);
  }
  video {
    max-width: 100%; max-height: 100%; border-radius: 12px;
    border: 1px solid var(--border); background: #000;
    box-shadow: 0 24px 64px rgba(0,0,0,0.6);
  }
</style>
</head>
<body>
  <div class="bar">
    <div class="bar-left">
      <span class="pill">Video Player</span>
      <strong>${escapeHtml(baseName)}</strong>
      <span class="meta">${escapeHtml(sizeStr)}</span>
    </div>
    <div class="actions">
      <select id="speedSelect" title="Playback Speed">
        <option value="0.75">0.75x</option>
        <option value="1" selected>1.0x</option>
        <option value="1.25">1.25x</option>
        <option value="1.5">1.5x</option>
        <option value="2">2.0x</option>
      </select>
      <button id="openExtBtn">Open in QuickTime</button>
      <button id="revealBtn">Reveal in Finder</button>
    </div>
  </div>
  <div class="stage">
    <video id="player" src="${mediaWebviewUri.toString()}" controls autoplay playsinline preload="metadata"></video>
  </div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const player = document.getElementById('player');
    document.getElementById('speedSelect').addEventListener('change', (e) => {
      if (player) player.playbackRate = Number(e.target.value) || 1;
    });
    document.getElementById('openExtBtn').addEventListener('click', () => {
      vscode.postMessage({ type: 'openExternal' });
    });
    document.getElementById('revealBtn').addEventListener('click', () => {
      vscode.postMessage({ type: 'reveal' });
    });
  </script>
</body>
</html>`;
      return;
    }

    if (
      lower.endsWith('.png') ||
      lower.endsWith('.svg') ||
      lower.endsWith('.jpg') ||
      lower.endsWith('.jpeg') ||
      lower.endsWith('.webp')
    ) {
      const imgUri = panel.webview.asWebviewUri(uri);
      panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src ${cspSource} data:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<style>
  body { margin: 0; height: 100vh; background: #0B0D12; color: #F3F5F9; font-family: -apple-system, sans-serif; display: flex; flex-direction: column; }
  .bar { display: flex; justify-content: space-between; align-items: center; padding: 8px 14px; background: #131822; border-bottom: 1px solid #252E42; }
  button { background: #1A2232; color: #F3F5F9; border: 1px solid #252E42; border-radius: 6px; padding: 5px 10px; cursor: pointer; }
  .stage { flex: 1; display: flex; align-items: center; justify-content: center; padding: 24px; overflow: auto; }
  img { max-width: 100%; max-height: 100%; border-radius: 10px; border: 1px solid #252E42; }
</style>
</head>
<body>
  <div class="bar">
    <strong>${escapeHtml(baseName)}</strong>
    <div>
      <button id="openExtBtn">Open Externally</button>
      <button id="revealBtn">Reveal in Finder</button>
    </div>
  </div>
  <div class="stage"><img src="${imgUri.toString()}" alt="${escapeHtml(baseName)}" /></div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    document.getElementById('openExtBtn').addEventListener('click', () => vscode.postMessage({ type: 'openExternal' }));
    document.getElementById('revealBtn').addEventListener('click', () => vscode.postMessage({ type: 'reveal' }));
  </script>
</body>
</html>`;
      return;
    }

    // Office Document Launch Card (.pptx without .deck.html, .xlsx, .docx)
    const stat = await fs.stat(fsPath).catch(() => undefined);
    const appLabel = lower.endsWith('.pptx')
      ? 'Keynote / PowerPoint'
      : lower.endsWith('.xlsx')
      ? 'Excel / Numbers'
      : 'Word / Pages';

    panel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}';">
<style>
  body { margin: 0; height: 100vh; background: #0B0D12; color: #F3F5F9; font-family: -apple-system, sans-serif; display: flex; align-items: center; justify-content: center; }
  .card { background: #131822; border: 1px solid #252E42; border-radius: 14px; padding: 32px 38px; max-width: 480px; text-align: center; }
  h2 { margin: 0 0 8px; font-size: 18px; }
  p { margin: 0 0 20px; color: #9AA4B8; font-size: 13px; }
  .btns { display: flex; gap: 10px; justify-content: center; }
  button { background: #1A2232; color: #F3F5F9; border: 1px solid #252E42; border-radius: 8px; padding: 8px 14px; font-size: 13px; cursor: pointer; }
  button.primary { background: #7C5CFF; border-color: #7C5CFF; color: #fff; font-weight: 600; }
</style>
</head>
<body>
  <div class="card">
    <h2>${escapeHtml(baseName)}</h2>
    <p>OpenXML Artifact (${stat ? escapeHtml(formatBytes(stat.size)) : 'Ready'})</p>
    <div class="btns">
      <button class="primary" id="openExtBtn">Open in ${escapeHtml(appLabel)}</button>
      <button id="revealBtn">Reveal in Finder</button>
    </div>
  </div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    document.getElementById('openExtBtn').addEventListener('click', () => vscode.postMessage({ type: 'openExternal' }));
    document.getElementById('revealBtn').addEventListener('click', () => vscode.postMessage({ type: 'reveal' }));
  </script>
</body>
</html>`;
  };

  await renderPreview();

  panel.webview.onDidReceiveMessage(async (msg: { type?: string }) => {
    if (msg.type === 'openPptx' && pptxPath) {
      await vscode.env.openExternal(vscode.Uri.file(pptxPath));
    } else if (msg.type === 'openExternal') {
      const target = pptxPath && lower.endsWith('.pptx') ? vscode.Uri.file(pptxPath) : uri;
      await vscode.env.openExternal(target);
    } else if (msg.type === 'reveal') {
      const target = pptxPath && lower.endsWith('.deck.html') ? vscode.Uri.file(pptxPath) : uri;
      await vscode.commands.executeCommand('revealFileInOS', target);
    } else if (msg.type === 'reload') {
      await renderPreview();
    }
  });

  return panel;
}

// ============================================================================
// 1. Rursor Media & Artifact Studio Manager (`ArtifactStudioManager`)
// ============================================================================

export class ArtifactStudioManager implements vscode.Disposable {
  public static readonly viewType = 'rursor.artifactStudio';

  private studioPanel: vscode.WebviewPanel | undefined;
  private activeTab: StudioTab = 'gallery';
  private watcher: vscode.FileSystemWatcher | undefined;
  private readonly statusBarItem: vscode.StatusBarItem;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly client: SidecarClient,
    private readonly extensionUri: vscode.Uri
  ) {
    this.statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 96);
    this.statusBarItem.text = '$(device-camera-video) Studio';
    this.statusBarItem.tooltip = 'Open Rursor Media & Artifact Studio (Cmd+Shift+A)';
    this.statusBarItem.command = 'rursor.openArtifactStudio';
    this.statusBarItem.show();
    this.disposables.push(this.statusBarItem);

    this.watcher = vscode.workspace.createFileSystemWatcher(
      '**/*.{pptx,html,mp4,webm,xlsx,docx,png}'
    );
    const triggerRefresh = () => {
      if (this.studioPanel) {
        void this.pushGalleryState();
      }
    };
    this.disposables.push(
      this.watcher,
      this.watcher.onDidCreate(triggerRefresh),
      this.watcher.onDidChange(triggerRefresh),
      this.watcher.onDidDelete(triggerRefresh)
    );
  }

  /**
   * Opens or focuses the Rursor Media & Artifact Studio Panel (`rursor.openArtifactStudio`, `Cmd+Shift+A`).
   */
  public async openArtifactStudio(initialTab?: StudioTab): Promise<void> {
    if (initialTab) {
      this.activeTab = initialTab;
    }

    if (this.studioPanel) {
      this.studioPanel.reveal(vscode.ViewColumn.One);
      void this.studioPanel.webview.postMessage({
        type: 'switchTab',
        tab: this.activeTab,
      });
      await this.pushGalleryState();
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      ArtifactStudioManager.viewType,
      'Rursor Media & Artifact Studio',
      vscode.ViewColumn.One,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [
          this.extensionUri,
          ...(vscode.workspace.workspaceFolders ?? []).map((f) => f.uri),
        ],
      }
    );

    this.studioPanel = panel;
    panel.webview.html = this.buildStudioWebviewHtml(panel.webview, this.activeTab);

    panel.onDidDispose(() => {
      if (this.studioPanel === panel) {
        this.studioPanel = undefined;
      }
    });

    panel.webview.onDidReceiveMessage((msg: StudioIncomingMessage) => {
      void this.handleStudioMessage(msg);
    });

    await this.pushGalleryState();
  }

  /**
   * Guided command (`rursor.createSlideDeck`) to generate a synchronized `.pptx` + `.deck.html` presentation.
   */
  public async createSlideDeck(
    prefillPrompt?: string
  ): Promise<PresentationArtifactResult | undefined> {
    if (!prefillPrompt) {
      const input = await vscode.window.showInputBox({
        title: 'Rursor AI Slide Deck Builder (.pptx + Interactive HTML5)',
        prompt:
          'Describe the presentation topic, audience, KPIs, or architecture to generate (or leave empty to open Studio Deck Builder)',
        placeHolder:
          'e.g. Q4 Engineering Architecture Review: Sub-2ms AST Gate, Parallel Subagents & Benchmarks',
      });
      if (input === undefined) {
        return undefined;
      }
      if (!input.trim()) {
        await this.openArtifactStudio('deck');
        return undefined;
      }
      prefillPrompt = input.trim();
    }

    return vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: 'Rursor Studio: Generating OpenXML .pptx + HTML5 Deck...',
        cancellable: false,
      },
      async (progress) => {
        progress.report({ message: 'Synthesizing slide structure & KPI cards...' });
        const slug =
          prefillPrompt
            .toLowerCase()
            .replace(/[^a-z0-9]+/g, '-')
            .replace(/^-|-$/g, '')
            .slice(0, 32) || 'presentation';
        const outputPath = `decks/${slug}.pptx`;
        const spec = await this.synthesizePresentationSpec({
          prompt: prefillPrompt,
          title: '',
          subtitle: '',
          author: 'Rursor Studio',
          theme: 'obsidian',
          slideCount: 6,
          outputPath,
          useWorkspaceSearch: true,
        });

        progress.report({ message: 'Packaging 16:9 OpenXML .pptx & companion .deck.html...' });
        const { promise } = this.client.request<PresentationArtifactResult>(
          'create_presentation',
          { spec }
        );
        const result = await promise;
        await this.pushGalleryState();
        if (result.htmlDeckPath) {
          await this.openArtifactPreview(vscode.Uri.file(result.htmlDeckPath));
        }
        return result;
      }
    );
  }

  /**
   * Guided command (`rursor.createExplainerVideo`) to generate a multi-scene `.mp4` explainer video.
   */
  public async createExplainerVideo(
    prefillPrompt?: string
  ): Promise<VideoArtifactResult | undefined> {
    if (!prefillPrompt) {
      const input = await vscode.window.showInputBox({
        title: 'Rursor Explainer Video Studio (.mp4)',
        prompt:
          'Describe the explainer video topic & scenes (or leave empty to open Studio Video Storyboard)',
        placeHolder:
          'e.g. 3-scene walkthrough of Rursor local sidecar indexing, syntax gate, and worktrees',
      });
      if (input === undefined) {
        return undefined;
      }
      if (!input.trim()) {
        await this.openArtifactStudio('video');
        return undefined;
      }
      prefillPrompt = input.trim();
    }

    return vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: 'Rursor Studio: Rendering Apple Silicon H.264 .mp4 Video...',
        cancellable: false,
      },
      async (progress) => {
        progress.report({ message: 'Drafting scene storyboard & narration...' });
        const slug =
          prefillPrompt
            .toLowerCase()
            .replace(/[^a-z0-9]+/g, '-')
            .replace(/^-|-$/g, '')
            .slice(0, 32) || 'explainer';
        const spec = await this.synthesizeVideoStoryboardSpec({
          prompt: prefillPrompt,
          title: prefillPrompt.slice(0, 64),
          outputPath: `videos/${slug}.mp4`,
          width: 1280,
          height: 720,
          fps: 24,
          voice: 'Samantha',
        });

        progress.report({ message: 'Rasterizing scenes, TTS voiceover & FFmpeg H.264...' });
        const { promise } = this.client.request<VideoArtifactResult>('render_explainer_video', {
          spec,
        });
        const result = await promise;
        await this.pushGalleryState();
        if (result.videoPath) {
          await this.openArtifactPreview(vscode.Uri.file(result.videoPath));
        }
        return result;
      }
    );
  }

  /**
   * Opens an artifact URI inside the In-Editor Slide Deck & Video Preview Panel.
   */
  public async openArtifactPreview(uri: vscode.Uri): Promise<vscode.WebviewPanel> {
    return openArtifactPreview(uri, this.extensionUri);
  }

  /**
   * Scans the active workspace for `*.pptx`, `*.deck.html`, `*.mp4`, `*.webm`,
   * `*.xlsx`, `*.docx`, and `.rursor/artifacts/*`.
   */
  public async scanWorkspaceArtifacts(): Promise<WorkspaceArtifactItem[]> {
    const folders = vscode.workspace.workspaceFolders;
    if (!folders || folders.length === 0) {
      return [];
    }
    const rootPath = folders[0].uri.fsPath;
    const exclude = '**/{node_modules,target,.git,out,dist}/**';

    const [officeAndMediaUris, deckHtmlUris, dotRursorUris] = await Promise.all([
      vscode.workspace.findFiles('**/*.{pptx,mp4,webm,xlsx,docx}', exclude, 200),
      vscode.workspace.findFiles('**/*.deck.html', exclude, 200),
      vscode.workspace.findFiles('.rursor/artifacts/**/*', exclude, 200),
    ]);

    const seen = new Set<string>();
    const allUris: vscode.Uri[] = [];
    for (const u of [...officeAndMediaUris, ...deckHtmlUris, ...dotRursorUris]) {
      if (!seen.has(u.fsPath)) {
        seen.add(u.fsPath);
        allUris.push(u);
      }
    }

    const items: WorkspaceArtifactItem[] = [];
    for (const u of allUris) {
      const stat = await fs.stat(u.fsPath).catch(() => undefined);
      if (!stat || !stat.isFile()) {
        continue;
      }

      const rel = path.relative(rootPath, u.fsPath).replace(/\\/g, '/');
      const lower = u.fsPath.toLowerCase();
      const ext = lower.endsWith('.deck.html')
        ? '.deck.html'
        : path.extname(u.fsPath).toLowerCase();
      const companionDeckFsPath = lower.endsWith('.pptx')
        ? await findSiblingFile(u.fsPath, '.deck.html')
        : lower.endsWith('.deck.html')
        ? u.fsPath
        : undefined;
      const companionPptxFsPath = lower.endsWith('.deck.html')
        ? await findSiblingFile(u.fsPath, '.pptx')
        : lower.endsWith('.pptx')
        ? u.fsPath
        : undefined;

      items.push({
        id: u.fsPath,
        name: path.basename(u.fsPath),
        relativePath: rel,
        fsPath: u.fsPath,
        ext,
        category: classifyArtifact(u.fsPath),
        sizeBytes: stat.size,
        mtimeMs: stat.mtimeMs,
        companionDeckFsPath,
        companionPptxFsPath,
      });
    }

    items.sort((a, b) => b.mtimeMs - a.mtimeMs);
    return items;
  }

  private async pushGalleryState(): Promise<void> {
    if (!this.studioPanel) {
      return;
    }
    const artifacts = await this.scanWorkspaceArtifacts();
    void this.studioPanel.webview.postMessage({
      type: 'artifactsUpdated',
      artifacts,
      workspaceRoot: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null,
    });
  }

  private postStudioStatus(message: string, isError = false): void {
    void this.studioPanel?.webview.postMessage({
      type: 'studioStatus',
      message,
      isError,
    });
  }

  private async handleStudioMessage(msg: StudioIncomingMessage): Promise<void> {
    try {
      switch (msg.type) {
        case 'refreshGallery': {
          await this.pushGalleryState();
          break;
        }
        case 'previewArtifact': {
          if (msg.fsPath) {
            await this.openArtifactPreview(vscode.Uri.file(msg.fsPath));
          }
          break;
        }
        case 'openExternal': {
          if (msg.fsPath) {
            await vscode.env.openExternal(vscode.Uri.file(msg.fsPath));
          }
          break;
        }
        case 'revealInFinder': {
          if (msg.fsPath) {
            await vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(msg.fsPath));
          }
          break;
        }
        case 'generateDeck': {
          this.postStudioStatus('Synthesizing slide deck outline & building OpenXML .pptx...');
          const spec = await this.synthesizePresentationSpec({
            prompt: msg.prompt ?? '',
            title: msg.title ?? '',
            subtitle: msg.subtitle ?? '',
            author: msg.author ?? 'Rursor Studio',
            theme: msg.theme ?? 'obsidian',
            slideCount: msg.slideCount ?? 6,
            outputPath: msg.outputPath || 'decks/presentation.pptx',
            useWorkspaceSearch: msg.useWorkspaceSearch ?? true,
          });
          const { promise } = this.client.request<PresentationArtifactResult>(
            'create_presentation',
            { spec }
          );
          const res = await promise;
          this.postStudioStatus(
            `Created ${res.relativePptxPath ?? res.pptxPath} (${res.slideCount} slides, ${formatBytes(res.bytes)})`
          );
          await this.pushGalleryState();
          if (res.htmlDeckPath) {
            await this.openArtifactPreview(vscode.Uri.file(res.htmlDeckPath));
          }
          break;
        }
        case 'draftVideoScenes': {
          this.postStudioStatus('Drafting AI storyboard scenes...');
          const storyboard = await this.synthesizeVideoStoryboardSpec({
            prompt: msg.prompt ?? 'Project Architecture Overview',
            title: msg.title || 'Architecture Explainer',
            outputPath: msg.outputPath || 'videos/explainer.mp4',
            width: msg.width ?? 1280,
            height: msg.height ?? 720,
            fps: msg.fps ?? 24,
            voice: msg.voice,
          });
          void this.studioPanel?.webview.postMessage({
            type: 'storyboardDrafted',
            storyboard,
          });
          this.postStudioStatus(`Drafted ${storyboard.scenes.length} scenes. Ready to render .mp4!`);
          break;
        }
        case 'generateVideo': {
          const outputPath = msg.outputPath || 'videos/explainer.mp4';
          if (msg.engine === 'veo') {
            this.postStudioStatus(
              'Submitting Cloud Gemini Veo (predictLongRunning) video generation...'
            );
            const veoSpec: VeoVideoSpec = {
              prompt: msg.prompt || msg.title || 'Cinematic software architecture visualization',
              outputPath,
              model: 'veo-2.0-generate-001',
              aspectRatio: '16:9',
              durationSecs: 6,
            };
            const { promise } = this.client.request<VideoArtifactResult>('generate_veo_video', {
              spec: veoSpec,
            });
            const res = await promise;
            this.postStudioStatus(
              `Veo MP4 ready: ${res.relativeVideoPath ?? res.videoPath} (${formatBytes(res.bytes)})`
            );
            await this.pushGalleryState();
            await this.openArtifactPreview(vscode.Uri.file(res.videoPath));
          } else {
            this.postStudioStatus(
              'Rendering Apple Silicon H.264 .mp4 scenes + macOS neural voiceover...'
            );
            const scenes =
              msg.scenes && msg.scenes.length > 0
                ? msg.scenes
                : (
                    await this.synthesizeVideoStoryboardSpec({
                      prompt: msg.prompt ?? msg.title ?? 'Rursor Architecture Walkthrough',
                      title: msg.title || 'Rursor Explainer',
                      outputPath,
                      width: msg.width ?? 1280,
                      height: msg.height ?? 720,
                      fps: msg.fps ?? 24,
                      voice: msg.voice,
                    })
                  ).scenes;

            const spec: VideoStoryboardSpec = {
              title: msg.title || 'Rursor Explainer',
              outputPath,
              width: msg.width ?? 1280,
              height: msg.height ?? 720,
              fps: msg.fps ?? 24,
              voice: msg.voice || undefined,
              scenes,
            };
            const { promise } = this.client.request<VideoArtifactResult>(
              'render_explainer_video',
              { spec }
            );
            const res = await promise;
            this.postStudioStatus(
              `Rendered ${res.relativeVideoPath ?? res.videoPath} (${res.sceneCount} scenes, ${formatBytes(res.bytes)})`
            );
            await this.pushGalleryState();
            await this.openArtifactPreview(vscode.Uri.file(res.videoPath));
          }
          break;
        }
        case 'generateOfficeArtifact': {
          await this.handleOfficeGeneration(msg);
          break;
        }
        case 'renderGenUiPreview': {
          const iframeHtml = buildInlineGenUiSrcdoc(
            msg.genUiTitle || 'Live Interactive KPI Widget',
            msg.genUiHtml || '<div>No HTML payload</div>',
            { minHeightPx: 220, maxHeightPx: 520 }
          );
          void this.studioPanel?.webview.postMessage({
            type: 'genUiRendered',
            iframeHtml,
          });
          break;
        }
      }
    } catch (err) {
      const errMsg = err instanceof Error ? err.message : String(err);
      this.postStudioStatus(`Error: ${errMsg}`, true);
      void vscode.window.showErrorMessage(`Rursor Media Studio: ${errMsg}`);
    }
  }

  private async handleOfficeGeneration(msg: StudioIncomingMessage): Promise<void> {
    if (msg.officeKind === 'xlsx') {
      this.postStudioStatus('Building OpenXML .xlsx spreadsheet with formulas...');
      const spec: SpreadsheetSpec = {
        outputPath: msg.outputPath || 'reports/metrics.xlsx',
        sheets: [
          {
            name: msg.title || 'Engineering Metrics',
            headers: ['Initiative', 'Q3 Baseline', 'Q4 Actual', 'Delta'],
            rows: [
              ['AST Syntax Gate (ms)', 8.4, 1.2, '=C2-B2'],
              ['RepoMap Indexing (ms)', 420, 95, '=C3-B3'],
              ['Parallel Subagents', 2, 6, '=C4-B4'],
              ['Total', '=SUM(B2:B4)', '=SUM(C2:C4)', '=SUM(D2:D4)'],
            ],
          },
        ],
      };
      const { promise } = this.client.request<SpreadsheetArtifactResult>('create_spreadsheet', {
        spec,
      });
      const res = await promise;
      this.postStudioStatus(`Created ${res.relativeXlsxPath ?? res.xlsxPath} (${formatBytes(res.bytes)})`);
      await this.pushGalleryState();
      return;
    }

    if (msg.officeKind === 'docx') {
      this.postStudioStatus('Building OpenXML .docx technical specification...');
      const spec: DocumentSpec = {
        title: msg.title || 'Technical Design Specification',
        subtitle: msg.subtitle || 'Generated by Rursor Media & Office Studio',
        outputPath: msg.outputPath || 'docs/design_spec.docx',
        sections: [
          {
            heading: '1. Executive Summary & Goals',
            paragraphs: [
              msg.prompt ||
                'This specification documents the system architecture, key trade-offs, and verification milestones.',
            ],
            bullets: [
              'Strict workspace path confinement for all generated artifacts',
              'Zero-stub OpenXML (.pptx, .xlsx, .docx) and H.264 MP4 pipelines',
            ],
            table: {
              headers: ['Subsystem', 'Target SLA', 'Verification'],
              rows: [
                ['Rust Sidecar RPC', '< 5ms dispatch', 'cargo test'],
                ['VS Code Studio UI', '60fps Webview', 'tsc --noEmit'],
              ],
            },
          },
        ],
      };
      const { promise } = this.client.request<DocumentArtifactResult>('create_document', { spec });
      const res = await promise;
      this.postStudioStatus(`Created ${res.relativeDocxPath ?? res.docxPath} (${formatBytes(res.bytes)})`);
      await this.pushGalleryState();
      return;
    }

    if (msg.officeKind === 'image') {
      this.postStudioStatus('Generating PNG visual asset...');
      const spec: ImageAssetSpec = {
        prompt: msg.prompt || msg.title || 'Rursor Architecture Card',
        subtitle: msg.subtitle || 'Multimodal Studio Visual Asset',
        outputPath: msg.outputPath || 'assets/diagram.png',
        mode: msg.svgMarkup?.trim() ? 'local' : 'auto',
        svgMarkup: msg.svgMarkup?.trim() || undefined,
        width: 1280,
        height: 720,
      };
      const { promise } = this.client.request<ImageAssetResult>('generate_image_asset', { spec });
      const res = await promise;
      this.postStudioStatus(`Created ${res.relativeImagePath ?? res.imagePath} (${formatBytes(res.bytes)})`);
      await this.pushGalleryState();
      await this.openArtifactPreview(vscode.Uri.file(res.imagePath));
    }
  }

  private async synthesizePresentationSpec(input: {
    prompt: string;
    title: string;
    subtitle: string;
    author: string;
    theme: DeckTheme;
    slideCount: number;
    outputPath: string;
    useWorkspaceSearch: boolean;
  }): Promise<PresentationSpec> {
    const targetSlides = Math.max(3, Math.min(14, input.slideCount || 6));
    if (input.prompt.trim()) {
      try {
        const model = vscode.workspace
          .getConfiguration('rursor')
          .get<string>('defaultModel', 'gemini-3.8-flash');
        const schemaPrompt = [
          `Design a ${targetSlides}-slide executive & technical presentation as strict JSON only (no prose outside JSON).`,
          `Topic / Instruction: ${input.prompt.trim()}`,
          input.title ? `Preferred Title: ${input.title}` : '',
          `JSON Schema:`,
          `{"title":"...","subtitle":"...","slides":[{"layout":"title|bullets|two_column|metrics|code|quote","title":"...","subtitle":"...","bullets":["..."],"rightBullets":["..."],"metrics":[{"label":"...","value":"...","delta":"+12%"}],"code":"optional snippet","speakerNotes":"..."}]}`,
          `Include varied layouts (start with "title", include at least one "metrics" KPI slide and one "two_column" or "code" slide).`,
        ]
          .filter(Boolean)
          .join('\n');

        const { promise } = this.client.request<{ text?: string }>('agent_chat', {
          mode: 'ask',
          model,
          useWorkspaceSearch: input.useWorkspaceSearch,
          includeGitDiff: false,
          messages: [{ role: 'user', content: schemaPrompt }],
        });
        const res = await promise;
        const parsed = extractJsonObject<Partial<PresentationSpec>>(res.text ?? '');
        if (parsed && Array.isArray(parsed.slides) && parsed.slides.length > 0) {
          return {
            title: input.title || parsed.title || 'Rursor Architecture Presentation',
            subtitle: input.subtitle || parsed.subtitle || '',
            author: input.author || 'Rursor Studio',
            theme: input.theme,
            outputPath: input.outputPath,
            slides: parsed.slides.map((s) => ({
              layout: (s.layout as SlideLayoutKind) || 'bullets',
              title: s.title ?? '',
              subtitle: s.subtitle ?? '',
              bullets: Array.isArray(s.bullets) ? s.bullets.map(String) : [],
              rightBullets: Array.isArray(s.rightBullets) ? s.rightBullets.map(String) : [],
              metrics: Array.isArray(s.metrics)
                ? s.metrics.map((m) => ({
                    label: String(m.label ?? ''),
                    value: String(m.value ?? ''),
                    delta: String(m.delta ?? ''),
                  }))
                : [],
              code: typeof s.code === 'string' ? s.code : undefined,
              speakerNotes: typeof s.speakerNotes === 'string' ? s.speakerNotes : undefined,
            })),
          };
        }
      } catch {
        // Fall through to deterministic offline deck builder when no API key is set
      }
    }

    const effectiveTitle =
      input.title.trim() || input.prompt.trim().slice(0, 68) || 'Rursor System Architecture';
    return {
      title: effectiveTitle,
      subtitle: input.subtitle.trim() || 'Architecture, Key Metrics & Implementation Roadmap',
      author: input.author || 'Rursor Studio',
      theme: input.theme,
      outputPath: input.outputPath,
      slides: [
        {
          layout: 'title',
          title: effectiveTitle,
          subtitle: input.subtitle.trim() || 'Generated by Rursor Multimodal Media Studio',
          bullets: ['Synchronized OpenXML 16:9 .pptx + Interactive HTML5 Slide Deck'],
          rightBullets: [],
          metrics: [],
          speakerNotes: `Opening overview for ${effectiveTitle}.`,
        },
        {
          layout: 'metrics',
          title: 'Key Performance Indicators',
          subtitle: 'Measured Latency, Reliability & Developer Velocity',
          bullets: ['Deterministic local-first execution on Apple Silicon'],
          rightBullets: [],
          metrics: [
            { label: 'AST Syntax Gate', value: '< 2ms', delta: '+99.9% valid edits' },
            { label: 'Slide & Doc Build', value: '18ms', delta: 'Zero external deps' },
            { label: 'Context Recall', value: '96.4%', delta: '+24% vs baseline' },
          ],
          speakerNotes: 'Highlight the three core performance KPIs.',
        },
        {
          layout: 'two_column',
          title: 'Architecture Comparison',
          subtitle: 'Local Rust Sidecar + VS Code Obsidian Studio',
          bullets: [
            'Rust sidecar handles indexing, sandboxing & OpenXML packaging',
            'Strict workspace path confinement prevents directory traversal',
          ],
          rightBullets: [
            'VS Code extension delivers 60fps previews & inline GenUI iframes',
            'Seamless cloud escalation to Gemini 2.5, Imagen 3 & Veo 2.0',
          ],
          metrics: [],
          speakerNotes: 'Contrast sidecar responsibilities with extension studio UI.',
        },
        {
          layout: 'code',
          title: 'Implementation Snapshot',
          subtitle: 'Workspace-Confined Artifact Pipeline',
          bullets: ['Typed JSON-RPC contract between TypeScript & Rust'],
          rightBullets: [],
          metrics: [],
          code: `pub fn create_presentation(\n    workspace_root: &Path,\n    spec: &PresentationSpec,\n) -> Result<PresentationArtifact, String> {\n    let (pptx_abs, rel) = resolve_confined_output_path(workspace_root, &spec.output_path)?;\n    package_openxml_zip(&entries, &pptx_abs)\n}`,
          speakerNotes: 'Walk through how confinement is enforced prior to writing files.',
        },
      ],
    };
  }

  private async synthesizeVideoStoryboardSpec(input: {
    prompt: string;
    title: string;
    outputPath: string;
    width: number;
    height: number;
    fps: number;
    voice?: string;
  }): Promise<VideoStoryboardSpec> {
    if (input.prompt.trim()) {
      try {
        const model = vscode.workspace
          .getConfiguration('rursor')
          .get<string>('defaultModel', 'gemini-3.8-flash');
        const ask = [
          `Create a 3-to-4 scene technical explainer video storyboard as strict JSON only.`,
          `Topic: ${input.prompt.trim()}`,
          `Schema: {"title":"...","scenes":[{"heading":"...","subheading":"...","bullets":["..."],"codeSnippet":"optional short snippet","narration":"1-2 spoken sentences","durationSecs":4.0,"accentHex":"#7C5CFF"}]}`,
        ].join('\n');
        const { promise } = this.client.request<{ text?: string }>('agent_chat', {
          mode: 'ask',
          model,
          useWorkspaceSearch: true,
          includeGitDiff: false,
          messages: [{ role: 'user', content: ask }],
        });
        const res = await promise;
        const parsed = extractJsonObject<{ title?: string; scenes?: VideoSceneSpec[] }>(
          res.text ?? ''
        );
        if (parsed && Array.isArray(parsed.scenes) && parsed.scenes.length > 0) {
          return {
            title: input.title || parsed.title || 'Rursor Explainer',
            outputPath: input.outputPath,
            width: input.width,
            height: input.height,
            fps: input.fps,
            voice: input.voice,
            scenes: parsed.scenes.map((s) => ({
              heading: String(s.heading ?? 'Overview'),
              subheading: String(s.subheading ?? ''),
              bullets: Array.isArray(s.bullets) ? s.bullets.map(String) : [],
              codeSnippet: typeof s.codeSnippet === 'string' ? s.codeSnippet : undefined,
              narration: typeof s.narration === 'string' ? s.narration : undefined,
              durationSecs: Number(s.durationSecs) > 0.5 ? Number(s.durationSecs) : 3.5,
              accentHex: typeof s.accentHex === 'string' ? s.accentHex : '#7C5CFF',
            })),
          };
        }
      } catch {
        // Offline deterministic fallback
      }
    }

    const topic = input.prompt.trim() || input.title.trim() || 'Rursor Architecture';
    return {
      title: input.title || topic,
      outputPath: input.outputPath,
      width: input.width,
      height: input.height,
      fps: input.fps,
      voice: input.voice,
      scenes: [
        {
          heading: topic.slice(0, 56),
          subheading: 'Local-First AI Engineering & Multimodal Studio',
          bullets: [
            'Native Rust sidecar paired with VS Code Obsidian Studio',
            'Deterministic OpenXML decks, docs, sheets & MP4 video',
          ],
          narration: `Welcome to this walkthrough of ${topic}.`,
          durationSecs: 3.5,
          accentHex: '#7C5CFF',
        },
        {
          heading: 'Verified Execution Pipeline',
          subheading: 'Sub-2ms Syntax Gate + APFS Isolation',
          bullets: [
            'Every edit is syntax-checked before touching disk',
            'Strict workspace confinement protects host files',
          ],
          codeSnippet: 'const { promise } = client.request("render_explainer_video", { spec });\nconst video = await promise;',
          narration: 'Every generated code edit and media artifact stays verified inside your workspace.',
          durationSecs: 4.0,
          accentHex: '#2AD8A7',
        },
      ],
    };
  }

  private buildStudioWebviewHtml(_webview: vscode.Webview, initialTab: StudioTab): string {
    const nonce = createNonce();
    const sampleGenUiHtml = `<div style="display:grid;grid-template-columns:repeat(3,1fr);gap:10px;margin-bottom:10px;">
  <div style="padding:10px;border-radius:8px;background:#0B0D12;border:1px solid var(--border);">
    <div style="font-size:11px;color:var(--muted);">P99 LATENCY</div>
    <div style="font-size:22px;font-weight:800;color:var(--accent-alt);">1.8ms</div>
  </div>
  <div style="padding:10px;border-radius:8px;background:#0B0D12;border:1px solid var(--border);">
    <div style="font-size:11px;color:var(--muted);">ARTIFACTS</div>
    <div style="font-size:22px;font-weight:800;color:var(--fg);">PPTX + MP4</div>
  </div>
  <div style="padding:10px;border-radius:8px;background:#0B0D12;border:1px solid var(--border);">
    <div style="font-size:11px;color:var(--muted);">SANDBOX</div>
    <div style="font-size:22px;font-weight:800;color:var(--accent);">Isolated</div>
  </div>
</div>
<button onclick="const el=document.getElementById('extraRows');el.style.display=el.style.display==='none'?'block':'none';">Toggle Benchmark Table (Tests ResizeObserver)</button>
<div id="extraRows" style="display:none;margin-top:10px;">
  <table>
    <thead><tr><th>Generator</th><th>Format</th><th>Engine</th></tr></thead>
    <tbody>
      <tr><td>create_presentation</td><td>.pptx + .deck.html</td><td>OpenXML + HTML5</td></tr>
      <tr><td>render_explainer_video</td><td>.mp4 (H.264)</td><td>Apple Silicon FFmpeg + TTS</td></tr>
      <tr><td>create_spreadsheet</td><td>.xlsx</td><td>OpenXML Formulas</td></tr>
    </tbody>
  </table>
</div>`;

    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'nonce-${nonce}'; frame-src data: blob:;">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Rursor Media & Artifact Studio</title>
<style>
  :root {
    --bg: #0B0D12;
    --card: #131822;
    --card-hover: #182030;
    --border: #252E42;
    --fg: #F3F5F9;
    --muted: #9AA4B8;
    --accent: #7C5CFF;
    --accent-alt: #2AD8A7;
    --danger: #FF6B81;
  }
  * { box-sizing: border-box; }
  html, body {
    margin: 0; padding: 0; width: 100%; height: 100%;
    background: var(--bg); color: var(--fg);
    font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Inter', sans-serif;
    font-size: 13px;
  }
  .shell { display: flex; flex-direction: column; height: 100vh; overflow: hidden; }
  .top-hdr {
    display: flex; align-items: center; justify-content: space-between;
    padding: 14px 22px; background: var(--card); border-bottom: 1px solid var(--border);
    gap: 16px; flex-wrap: wrap;
  }
  .brand { display: flex; align-items: center; gap: 10px; }
  .brand-dot {
    width: 10px; height: 10px; border-radius: 50%;
    background: linear-gradient(135deg, var(--accent), var(--accent-alt));
    box-shadow: 0 0 12px var(--accent);
  }
  .brand h1 { margin: 0; font-size: 16px; font-weight: 700; letter-spacing: -0.01em; }
  .brand span { color: var(--muted); font-size: 12px; }
  .tabs { display: flex; gap: 6px; }
  .tab-btn {
    background: transparent; color: var(--muted); border: 1px solid transparent;
    border-radius: 8px; padding: 7px 13px; font-size: 12px; font-weight: 600; cursor: pointer;
  }
  .tab-btn:hover { color: var(--fg); background: rgba(255,255,255,0.04); }
  .tab-btn.active {
    background: rgba(124, 92, 255, 0.16); color: var(--fg);
    border-color: rgba(124, 92, 255, 0.55);
  }
  .status-banner {
    display: none; padding: 8px 22px; font-size: 12px;
    background: rgba(124, 92, 255, 0.14); border-bottom: 1px solid var(--border); color: var(--fg);
  }
  .status-banner.error { background: rgba(255, 107, 129, 0.18); color: var(--danger); }
  .status-banner.visible { display: block; }
  .content { flex: 1; overflow-y: auto; padding: 20px 22px; }
  .pane { display: none; max-width: 1120px; margin: 0 auto; }
  .pane.active { display: block; }
  .filter-row {
    display: flex; align-items: center; justify-content: space-between;
    gap: 12px; margin-bottom: 16px; flex-wrap: wrap;
  }
  .chips { display: flex; gap: 6px; flex-wrap: wrap; }
  .chip {
    background: var(--card); color: var(--muted); border: 1px solid var(--border);
    border-radius: 999px; padding: 5px 12px; font-size: 12px; cursor: pointer;
  }
  .chip.active { background: var(--accent); color: #fff; border-color: var(--accent); font-weight: 600; }
  .gallery-grid {
    display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px;
  }
  .art-card {
    background: var(--card); border: 1px solid var(--border); border-radius: 12px;
    padding: 16px; display: flex; flex-direction: column; gap: 10px;
    transition: border-color 140ms ease, background 140ms ease;
  }
  .art-card:hover { border-color: var(--accent); background: var(--card-hover); }
  .art-top { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
  .ext-badge {
    font-size: 10px; font-weight: 800; text-transform: uppercase; letter-spacing: 0.08em;
    padding: 3px 8px; border-radius: 6px; background: rgba(124,92,255,0.18); color: var(--accent);
  }
  .ext-badge.video { background: rgba(42,216,167,0.16); color: var(--accent-alt); }
  .art-name { font-weight: 600; font-size: 14px; word-break: break-all; }
  .art-path { font-size: 11px; color: var(--muted); word-break: break-all; }
  .art-actions { display: flex; gap: 6px; flex-wrap: wrap; margin-top: auto; padding-top: 6px; }
  button {
    background: #1A2232; color: var(--fg); border: 1px solid var(--border);
    border-radius: 7px; padding: 6px 11px; font-size: 12px; font-weight: 500; cursor: pointer;
  }
  button:hover { border-color: var(--accent); }
  button.primary { background: var(--accent); border-color: var(--accent); color: #fff; font-weight: 600; }
  button.emerald { background: rgba(42,216,167,0.18); border-color: rgba(42,216,167,0.5); color: var(--accent-alt); font-weight: 600; }
  .form-card {
    background: var(--card); border: 1px solid var(--border); border-radius: 12px;
    padding: 20px; display: flex; flex-direction: column; gap: 14px; margin-bottom: 18px;
  }
  .form-card h2 { margin: 0; font-size: 15px; }
  .form-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); gap: 12px; }
  label { display: flex; flex-direction: column; gap: 5px; font-size: 12px; color: var(--muted); font-weight: 500; }
  input, select, textarea {
    background: var(--bg); color: var(--fg); border: 1px solid var(--border);
    border-radius: 7px; padding: 8px 10px; font-size: 13px; font-family: inherit; outline: none;
  }
  input:focus, select:focus, textarea:focus { border-color: var(--accent); }
  textarea { min-height: 82px; resize: vertical; }
  .scene-item {
    background: var(--bg); border: 1px solid var(--border); border-radius: 10px;
    padding: 12px; display: flex; flex-direction: column; gap: 8px;
  }
  .empty-state {
    text-align: center; padding: 48px 20px; background: var(--card);
    border: 1px dashed var(--border); border-radius: 12px; color: var(--muted);
  }
</style>
</head>
<body>
<div class="shell">
  <header class="top-hdr">
    <div class="brand">
      <div class="brand-dot"></div>
      <div>
        <h1>Rursor Media &amp; Artifact Studio</h1>
        <span>OpenXML PPTX/XLSX/DOCX • Apple Silicon MP4 + Gemini Veo • Sandboxed GenUI</span>
      </div>
    </div>
    <nav class="tabs">
      <button class="tab-btn" data-tab="gallery">Artifact Gallery</button>
      <button class="tab-btn" data-tab="deck">AI Deck Builder (.pptx)</button>
      <button class="tab-btn" data-tab="video">AI Video Storyboard (.mp4)</button>
      <button class="tab-btn" data-tab="office">Office &amp; GenUI Lab</button>
    </nav>
  </header>
  <div class="status-banner" id="statusBanner"></div>

  <main class="content">
    <!-- TAB 1: GALLERY -->
    <section class="pane" id="pane-gallery">
      <div class="filter-row">
        <div class="chips" id="filterChips">
          <button class="chip active" data-cat="all">All Artifacts</button>
          <button class="chip" data-cat="deck">Slide Decks</button>
          <button class="chip" data-cat="video">Videos</button>
          <button class="chip" data-cat="spreadsheet">Spreadsheets</button>
          <button class="chip" data-cat="document">Documents</button>
          <button class="chip" data-cat="image">Images</button>
        </div>
        <div style="display:flex;gap:8px;align-items:center;">
          <input type="search" id="searchInput" placeholder="Filter by filename or path..." style="width:220px;" />
          <button id="refreshBtn">Refresh</button>
        </div>
      </div>
      <div id="galleryContainer" class="gallery-grid"></div>
    </section>

    <!-- TAB 2: AI DECK BUILDER -->
    <section class="pane" id="pane-deck">
      <div class="form-card">
        <h2>Guided AI Slide Deck Builder (16:9 OpenXML .pptx + Interactive .deck.html)</h2>
        <label>
          Presentation Prompt / Architecture Brief
          <textarea id="deckPrompt" placeholder="Describe your presentation topic, target audience, KPIs, code architecture, or paste meeting notes..."></textarea>
        </label>
        <div class="form-grid">
          <label>Deck Title (optional override)<input type="text" id="deckTitle" placeholder="Q4 Systems Architecture Review" /></label>
          <label>Subtitle<input type="text" id="deckSubtitle" placeholder="Performance, Reliability & Roadmap" /></label>
          <label>Author / Team<input type="text" id="deckAuthor" value="Rursor Engineering" /></label>
        </div>
        <div class="form-grid">
          <label>Visual Theme
            <select id="deckTheme">
              <option value="obsidian" selected>Rursor Obsidian (#0B0D12)</option>
              <option value="executive">Executive Slate (#0F172A)</option>
              <option value="minimal">Minimal Light (#FFFFFF)</option>
            </select>
          </label>
          <label>Slide Count<input type="number" id="deckSlideCount" min="3" max="14" value="6" /></label>
          <label>Output Path (.pptx)<input type="text" id="deckOutputPath" value="decks/architecture.pptx" /></label>
        </div>
        <div style="display:flex;justify-content:space-between;align-items:center;margin-top:4px;">
          <label style="flex-direction:row;align-items:center;gap:8px;color:var(--fg);">
            <input type="checkbox" id="deckUseSearch" checked /> Ground slides in indexed workspace files
          </label>
          <button class="primary" id="buildDeckBtn">Generate .pptx + Preview Deck</button>
        </div>
      </div>
    </section>

    <!-- TAB 3: AI VIDEO STORYBOARD -->
    <section class="pane" id="pane-video">
      <div class="form-card">
        <h2>Programmatic Explainer Video &amp; Cloud Gemini Veo Studio (.mp4)</h2>
        <div class="form-grid">
          <label>Video Engine
            <select id="videoEngine">
              <option value="local" selected>Local Apple Silicon Engine (FFmpeg H.264 + macOS TTS)</option>
              <option value="veo">Cloud Gemini Veo 2.0 (veo-2.0-generate-001)</option>
            </select>
          </label>
          <label>Video Title<input type="text" id="videoTitle" value="Rursor Architecture Walkthrough" /></label>
          <label>Output Path (.mp4)<input type="text" id="videoOutputPath" value="videos/explainer.mp4" /></label>
        </div>
        <label>
          Storyboard Topic / Veo Visual Prompt
          <textarea id="videoPrompt" placeholder="Explain how the Rust sidecar indexes AST symbols, enforces Tree-sitter syntax gates, and isolates worktrees..."></textarea>
        </label>
        <div class="form-grid" id="localVideoOptions">
          <label>Resolution
            <select id="videoRes">
              <option value="1280x720" selected>1280 x 720 (HD 16:9)</option>
              <option value="1920x1080">1920 x 1080 (Full HD 16:9)</option>
              <option value="960x540">960 x 540 (Fast Preview)</option>
            </select>
          </label>
          <label>Frame Rate (FPS)
            <select id="videoFps">
              <option value="24" selected>24 fps</option>
              <option value="30">30 fps</option>
            </select>
          </label>
          <label>macOS Neural TTS Voice
            <select id="videoVoice">
              <option value="Samantha" selected>Samantha (macOS Neural)</option>
              <option value="Daniel">Daniel (UK)</option>
              <option value="Alex">Alex</option>
              <option value="none">Silent Track (No Voiceover)</option>
            </select>
          </label>
        </div>
        <div id="scenesList" style="display:flex;flex-direction:column;gap:10px;"></div>
        <div style="display:flex;gap:10px;justify-content:flex-end;">
          <button id="addSceneBtn">+ Add Scene</button>
          <button id="draftScenesBtn">AI Auto-Draft Storyboard</button>
          <button class="primary" id="renderVideoBtn">Render .mp4 Video</button>
        </div>
      </div>
    </section>

    <!-- TAB 4: OFFICE & GENUI LAB -->
    <section class="pane" id="pane-office">
      <div class="form-grid" style="grid-template-columns: repeat(auto-fit, minmax(310px, 1fr));">
        <div class="form-card">
          <h2>OpenXML Excel (.xlsx) &amp; Word (.docx)</h2>
          <label>Document / Sheet Title<input type="text" id="officeTitle" value="Q4 Engineering Benchmarks" /></label>
          <label>Summary / Notes<textarea id="officePrompt" placeholder="Key metrics, formulas, or design notes..."></textarea></label>
          <div style="display:flex;gap:8px;flex-wrap:wrap;">
            <button class="primary" id="createXlsxBtn">Create .xlsx (with =SUM)</button>
            <button class="primary" id="createDocxBtn">Create .docx Spec</button>
          </div>
        </div>

        <div class="form-card">
          <h2>Imagen / Local SVG Diagram &rarr; PNG</h2>
          <label>Asset Prompt or Title<input type="text" id="imgPrompt" value="Rursor Multimodal Pipeline" /></label>
          <label>Optional SVG Markup (leave blank for Diagram Card / Cloud Imagen)<textarea id="imgSvg" placeholder="<svg width='640' height='360'>...</svg>"></textarea></label>
          <button class="emerald" id="createImgBtn">Generate .png Asset</button>
        </div>
      </div>

      <div class="form-card">
        <h2>Sandboxed Inline Generative UI Tester (<code>buildInlineGenUiSrcdoc</code>)</h2>
        <div class="form-grid">
          <label>Widget Title<input type="text" id="genUiTitle" value="Sidecar Latency & Artifact Matrix" /></label>
        </div>
        <label>Interactive HTML/CSS/JS Payload<textarea id="genUiHtml">${escapeHtml(sampleGenUiHtml)}</textarea></label>
        <div><button class="primary" id="previewGenUiBtn">Render Sandboxed GenUI Card</button></div>
        <div id="genUiMountArea" style="margin-top:8px;"></div>
      </div>
    </section>
  </main>
</div>

<script nonce="${nonce}">
${INLINE_GENUI_HOST_RESIZE_SCRIPT}
(function() {
  const vscode = acquireVsCodeApi();
  let currentTab = ${JSON.stringify(initialTab)};
  let currentCat = 'all';
  let allArtifacts = [];
  let scenes = [
    {
      heading: 'Sub-2ms Tree-sitter Syntax Gate',
      subheading: 'Rust Sidecar Pre-Write Verification',
      bullets: ['Validates AST before touching disk', 'Zero broken edits across Rust, TS, Python & Go'],
      codeSnippet: 'pub fn validate_edit_syntax(path: &str, after: &str) -> bool',
      narration: 'Rursor validates every proposed edit in under two milliseconds.',
      durationSecs: 3.5,
      accentHex: '#7C5CFF'
    },
    {
      heading: 'OpenXML & Apple Silicon Video Studio',
      subheading: 'Synchronized .pptx, .xlsx, .docx & H.264 .mp4',
      bullets: ['16:9 widescreen decks + interactive HTML5 viewer', 'Strict workspace path confinement'],
      codeSnippet: '',
      narration: 'Generate native slide decks, spreadsheets, and explainer videos directly inside your workspace.',
      durationSecs: 3.5,
      accentHex: '#2AD8A7'
    }
  ];

  function setTab(tab) {
    currentTab = tab;
    document.querySelectorAll('.tab-btn').forEach((b) => {
      b.classList.toggle('active', b.getAttribute('data-tab') === tab);
    });
    document.querySelectorAll('.pane').forEach((p) => {
      p.classList.toggle('active', p.id === 'pane-' + tab);
    });
  }

  document.querySelectorAll('.tab-btn').forEach((btn) => {
    btn.addEventListener('click', () => setTab(btn.getAttribute('data-tab') || 'gallery'));
  });

  document.querySelectorAll('#filterChips .chip').forEach((chip) => {
    chip.addEventListener('click', () => {
      currentCat = chip.getAttribute('data-cat') || 'all';
      document.querySelectorAll('#filterChips .chip').forEach((c) => c.classList.toggle('active', c === chip));
      renderGallery();
    });
  });

  document.getElementById('searchInput').addEventListener('input', renderGallery);
  document.getElementById('refreshBtn').addEventListener('click', () => {
    vscode.postMessage({ type: 'refreshGallery' });
  });

  function fmtSize(bytes) {
    if (!bytes || bytes <= 0) return '0 B';
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1048576) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / 1048576).toFixed(2) + ' MB';
  }

  function esc(str) {
    const d = document.createElement('div');
    d.textContent = str || '';
    return d.innerHTML;
  }

  function renderGallery() {
    const q = (document.getElementById('searchInput').value || '').toLowerCase().trim();
    const container = document.getElementById('galleryContainer');
    const filtered = allArtifacts.filter((item) => {
      if (currentCat !== 'all' && item.category !== currentCat) return false;
      if (q && !item.name.toLowerCase().includes(q) && !item.relativePath.toLowerCase().includes(q)) return false;
      return true;
    });

    if (filtered.length === 0) {
      container.innerHTML = '<div class="empty-state" style="grid-column:1/-1;">No matching workspace artifacts yet. Use <strong>AI Deck Builder</strong>, <strong>AI Video Storyboard</strong>, or <strong>Office &amp; GenUI Lab</strong> to create one!</div>';
      return;
    }

    container.innerHTML = '';
    filtered.forEach((item) => {
      const card = document.createElement('div');
      card.className = 'art-card';
      const badgeCls = item.category === 'video' ? 'ext-badge video' : 'ext-badge';
      const extLabel = (item.ext || '').replace(/^\\./, '').toUpperCase() || item.category.toUpperCase();

      let primaryBtnHtml = '';
      if (item.category === 'deck') {
        const previewTarget = item.companionDeckFsPath || item.fsPath;
        primaryBtnHtml = '<button class="primary" data-act="preview" data-path="' + esc(previewTarget) + '">Preview Slide Deck</button>';
      } else if (item.category === 'video') {
        primaryBtnHtml = '<button class="emerald" data-act="preview" data-path="' + esc(item.fsPath) + '">Play Video</button>';
      } else if (item.category === 'image') {
        primaryBtnHtml = '<button class="primary" data-act="preview" data-path="' + esc(item.fsPath) + '">Preview Image</button>';
      }

      const extOpenPath = item.companionPptxFsPath || item.fsPath;
      const extOpenLabel = item.category === 'deck'
        ? 'Open in Keynote'
        : item.category === 'spreadsheet'
        ? 'Open in Excel'
        : item.category === 'document'
        ? 'Open in Word'
        : 'Open Externally';

      card.innerHTML =
        '<div class="art-top"><span class="' + badgeCls + '">' + esc(extLabel) + '</span><span style="color:var(--muted);font-size:11px;">' + esc(fmtSize(item.sizeBytes)) + '</span></div>' +
        '<div class="art-name">' + esc(item.name) + '</div>' +
        '<div class="art-path">' + esc(item.relativePath) + '</div>' +
        '<div class="art-actions">' +
          primaryBtnHtml +
          '<button data-act="external" data-path="' + esc(extOpenPath) + '">' + esc(extOpenLabel) + '</button>' +
          '<button data-act="reveal" data-path="' + esc(item.fsPath) + '">Reveal in Finder</button>' +
        '</div>';

      card.querySelectorAll('button[data-act]').forEach((b) => {
        b.addEventListener('click', () => {
          const act = b.getAttribute('data-act');
          const p = b.getAttribute('data-path');
          if (act === 'preview') vscode.postMessage({ type: 'previewArtifact', fsPath: p });
          else if (act === 'external') vscode.postMessage({ type: 'openExternal', fsPath: p });
          else if (act === 'reveal') vscode.postMessage({ type: 'revealInFinder', fsPath: p });
        });
      });
      container.appendChild(card);
    });
  }

  function renderScenesEditor() {
    const host = document.getElementById('scenesList');
    host.innerHTML = '';
    scenes.forEach((sc, idx) => {
      const box = document.createElement('div');
      box.className = 'scene-item';
      box.innerHTML =
        '<div style="display:flex;justify-content:space-between;align-items:center;">' +
          '<strong style="color:var(--accent-alt);">Scene ' + (idx + 1) + '</strong>' +
          '<button data-rm="' + idx + '" style="padding:2px 8px;font-size:11px;">Remove</button>' +
        '</div>' +
        '<div class="form-grid">' +
          '<label>Heading<input type="text" data-f="heading" value="' + esc(sc.heading) + '" /></label>' +
          '<label>Subheading<input type="text" data-f="subheading" value="' + esc(sc.subheading) + '" /></label>' +
          '<label>Duration (s)<input type="number" step="0.5" min="1" max="30" data-f="durationSecs" value="' + Number(sc.durationSecs || 3.5) + '" /></label>' +
        '</div>' +
        '<label>Bullets (one per line)<textarea data-f="bullets" style="min-height:52px;">' + esc((sc.bullets || []).join('\\n')) + '</textarea></label>' +
        '<label>Spoken Narration (macOS TTS)<input type="text" data-f="narration" value="' + esc(sc.narration || '') + '" /></label>';

      box.querySelector('button[data-rm]').addEventListener('click', () => {
        if (scenes.length > 1) {
          scenes.splice(idx, 1);
          renderScenesEditor();
        }
      });
      box.querySelectorAll('[data-f]').forEach((inp) => {
        inp.addEventListener('input', () => {
          const field = inp.getAttribute('data-f');
          if (field === 'bullets') sc.bullets = inp.value.split('\\n').map((s) => s.trim()).filter(Boolean);
          else if (field === 'durationSecs') sc.durationSecs = Number(inp.value) || 3.5;
          else sc[field] = inp.value;
        });
      });
      host.appendChild(box);
    });
  }

  document.getElementById('addSceneBtn').addEventListener('click', () => {
    scenes.push({
      heading: 'New Storyboard Scene',
      subheading: 'Key takeaway',
      bullets: ['Highlight metric or architectural component'],
      narration: 'Next, let us examine how this component operates.',
      durationSecs: 3.5,
      accentHex: '#7C5CFF'
    });
    renderScenesEditor();
  });

  document.getElementById('buildDeckBtn').addEventListener('click', () => {
    vscode.postMessage({
      type: 'generateDeck',
      prompt: document.getElementById('deckPrompt').value,
      title: document.getElementById('deckTitle').value,
      subtitle: document.getElementById('deckSubtitle').value,
      author: document.getElementById('deckAuthor').value,
      theme: document.getElementById('deckTheme').value,
      slideCount: Number(document.getElementById('deckSlideCount').value) || 6,
      outputPath: document.getElementById('deckOutputPath').value,
      useWorkspaceSearch: document.getElementById('deckUseSearch').checked
    });
  });

  function readVideoParams() {
    const resParts = (document.getElementById('videoRes').value || '1280x720').split('x');
    return {
      engine: document.getElementById('videoEngine').value,
      title: document.getElementById('videoTitle').value,
      outputPath: document.getElementById('videoOutputPath').value,
      prompt: document.getElementById('videoPrompt').value,
      width: Number(resParts[0]) || 1280,
      height: Number(resParts[1]) || 720,
      fps: Number(document.getElementById('videoFps').value) || 24,
      voice: document.getElementById('videoVoice').value,
      scenes: scenes
    };
  }

  document.getElementById('draftScenesBtn').addEventListener('click', () => {
    vscode.postMessage(Object.assign({ type: 'draftVideoScenes' }, readVideoParams()));
  });

  document.getElementById('renderVideoBtn').addEventListener('click', () => {
    vscode.postMessage(Object.assign({ type: 'generateVideo' }, readVideoParams()));
  });

  document.getElementById('createXlsxBtn').addEventListener('click', () => {
    vscode.postMessage({
      type: 'generateOfficeArtifact',
      officeKind: 'xlsx',
      title: document.getElementById('officeTitle').value,
      prompt: document.getElementById('officePrompt').value,
      outputPath: 'reports/metrics.xlsx'
    });
  });

  document.getElementById('createDocxBtn').addEventListener('click', () => {
    vscode.postMessage({
      type: 'generateOfficeArtifact',
      officeKind: 'docx',
      title: document.getElementById('officeTitle').value,
      prompt: document.getElementById('officePrompt').value,
      outputPath: 'docs/specification.docx'
    });
  });

  document.getElementById('createImgBtn').addEventListener('click', () => {
    vscode.postMessage({
      type: 'generateOfficeArtifact',
      officeKind: 'image',
      title: document.getElementById('imgPrompt').value,
      prompt: document.getElementById('imgPrompt').value,
      svgMarkup: document.getElementById('imgSvg').value,
      outputPath: 'assets/studio_asset.png'
    });
  });

  document.getElementById('previewGenUiBtn').addEventListener('click', () => {
    vscode.postMessage({
      type: 'renderGenUiPreview',
      genUiTitle: document.getElementById('genUiTitle').value,
      genUiHtml: document.getElementById('genUiHtml').value
    });
  });

  window.addEventListener('message', (event) => {
    const msg = event.data;
    if (!msg) return;
    if (msg.type === 'switchTab' && msg.tab) {
      setTab(msg.tab);
    } else if (msg.type === 'artifactsUpdated') {
      allArtifacts = Array.isArray(msg.artifacts) ? msg.artifacts : [];
      renderGallery();
    } else if (msg.type === 'studioStatus') {
      const banner = document.getElementById('statusBanner');
      banner.textContent = msg.message || '';
      banner.classList.toggle('visible', Boolean(msg.message));
      banner.classList.toggle('error', Boolean(msg.isError));
    } else if (msg.type === 'storyboardDrafted' && msg.storyboard && Array.isArray(msg.storyboard.scenes)) {
      scenes = msg.storyboard.scenes;
      if (msg.storyboard.title) document.getElementById('videoTitle').value = msg.storyboard.title;
      renderScenesEditor();
    } else if (msg.type === 'genUiRendered' && msg.iframeHtml) {
      document.getElementById('genUiMountArea').innerHTML = msg.iframeHtml;
    }
  });

  setTab(currentTab);
  renderScenesEditor();
})();
</script>
</body>
</html>`;
  }

  public dispose(): void {
    this.studioPanel?.dispose();
    this.studioPanel = undefined;
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables.length = 0;
  }
}

/**
 * Registers `ArtifactStudioManager` and Studio commands (`rursor.openArtifactStudio`,
 * `rursor.createSlideDeck`, `rursor.createExplainerVideo`, `rursor.previewArtifact`) on `context`.
 */
export function registerArtifactStudio(
  context: vscode.ExtensionContext,
  client: SidecarClient
): ArtifactStudioManager {
  const studio = new ArtifactStudioManager(client, context.extensionUri);

  context.subscriptions.push(
    studio,
    vscode.commands.registerCommand('rursor.openArtifactStudio', (tab?: StudioTab) =>
      studio.openArtifactStudio(tab)
    ),
    vscode.commands.registerCommand('rursor.createSlideDeck', (prompt?: string) =>
      studio.createSlideDeck(typeof prompt === 'string' ? prompt : undefined)
    ),
    vscode.commands.registerCommand('rursor.createExplainerVideo', (prompt?: string) =>
      studio.createExplainerVideo(typeof prompt === 'string' ? prompt : undefined)
    ),
    vscode.commands.registerCommand('rursor.previewArtifact', async (arg?: vscode.Uri | string) => {
      if (arg instanceof vscode.Uri) {
        await studio.openArtifactPreview(arg);
      } else if (typeof arg === 'string' && arg.trim()) {
        await studio.openArtifactPreview(vscode.Uri.file(arg.trim()));
      } else {
        await studio.openArtifactStudio('gallery');
      }
    })
  );

  return studio;
}
