import * as crypto from 'node:crypto';
import * as fs from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

export const CANVAS_EDITOR_VIEW_TYPE = 'rursor.canvasEditor';

export interface CanvasNode {
  id: string;
  kind: 'frame' | 'card' | 'note' | 'service' | 'image';
  title: string;
  body: string;
  x: number;
  y: number;
  width: number;
  height: number;
  color?: string;
  imageDataUrl?: string;
}

export interface CanvasEdge {
  id: string;
  fromId: string;
  toId: string;
  label?: string;
}

export interface CanvasPreviewTile {
  id: string;
  title: string;
  x: number;
  y: number;
  width: number;
  height: number;
  html: string;
}

export interface CanvasDocumentModel {
  version: 1;
  viewport: { panX: number; panY: number; zoom: number; snapToGrid: boolean };
  nodes: CanvasNode[];
  edges: CanvasEdge[];
  previews: CanvasPreviewTile[];
}

function defaultCanvasModel(): CanvasDocumentModel {
  return {
    version: 1,
    viewport: { panX: 60, panY: 60, zoom: 1, snapToGrid: true },
    nodes: [
      {
        id: 'n_hero_frame',
        kind: 'frame',
        title: 'SaaS Analytics Dashboard',
        body: 'Header nav + 3 KPI metric cards (MRR, Active Users, Latency) + interactive sparkline chart + recent activity table',
        x: 80,
        y: 80,
        width: 360,
        height: 240,
        color: '#7C5CFF',
      },
      {
        id: 'n_api_srv',
        kind: 'service',
        title: 'Rust Sidecar API',
        body: 'GET /metrics · WebSocket live telemetry stream',
        x: 520,
        y: 120,
        width: 240,
        height: 140,
        color: '#22C55E',
      },
    ],
    edges: [
      {
        id: 'e_1',
        fromId: 'n_hero_frame',
        toId: 'n_api_srv',
        label: 'JSON-RPC / SSE',
      },
    ],
    previews: [],
  };
}

function extractGeneratedHtmlBlock(rawText: string): string {
  const fencedHtml = rawText.match(/```(?:html|tsx|jsx)?\s*([\s\S]*?)```/i);
  if (fencedHtml && fencedHtml[1]?.trim()) {
    return fencedHtml[1].trim();
  }
  return rawText.trim();
}

export class RursorCanvasProvider implements vscode.CustomTextEditorProvider {
  public static readonly viewType = CANVAS_EDITOR_VIEW_TYPE;

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly client: SidecarClient
  ) {}

  public async resolveCustomTextEditor(
    document: vscode.TextDocument,
    webviewPanel: vscode.WebviewPanel,
    _token: vscode.CancellationToken
  ): Promise<void> {
    void this.extensionUri;
    webviewPanel.webview.options = {
      enableScripts: true,
    };

    let applyingFromWebview = false;

    const parseDoc = (): CanvasDocumentModel => {
      const raw = document.getText().trim();
      if (!raw) {
        return defaultCanvasModel();
      }
      try {
        const parsed = JSON.parse(raw) as Partial<CanvasDocumentModel>;
        return {
          version: 1,
          viewport: parsed.viewport ?? { panX: 60, panY: 60, zoom: 1, snapToGrid: true },
          nodes: Array.isArray(parsed.nodes) ? parsed.nodes : [],
          edges: Array.isArray(parsed.edges) ? parsed.edges : [],
          previews: Array.isArray(parsed.previews) ? parsed.previews : [],
        };
      } catch {
        return defaultCanvasModel();
      }
    };

    const persistModelToDoc = async (model: CanvasDocumentModel): Promise<void> => {
      const nextText = `${JSON.stringify(model, null, 2)}\n`;
      if (nextText === document.getText()) {
        return;
      }
      applyingFromWebview = true;
      try {
        const edit = new vscode.WorkspaceEdit();
        const fullRange = new vscode.Range(
          document.positionAt(0),
          document.positionAt(document.getText().length)
        );
        edit.replace(document.uri, fullRange, nextText);
        await vscode.workspace.applyEdit(edit);
      } finally {
        applyingFromWebview = false;
      }
    };

    webviewPanel.webview.html = this.buildWebviewHtml(webviewPanel.webview, parseDoc());

    const docChangeSub = vscode.workspace.onDidChangeTextDocument((ev) => {
      if (ev.document.uri.toString() === document.uri.toString() && !applyingFromWebview) {
        void webviewPanel.webview.postMessage({
          type: 'loadModel',
          model: parseDoc(),
        });
      }
    });

    const msgSub = webviewPanel.webview.onDidReceiveMessage(async (msg: Record<string, unknown>) => {
      const type = String(msg.type ?? '');
      if (type === 'syncModel' && msg.model && typeof msg.model === 'object') {
        await persistModelToDoc(msg.model as CanvasDocumentModel);
        return;
      }

      if (type === 'generatePreview') {
        const selectedNodes = (Array.isArray(msg.nodes) ? msg.nodes : []) as CanvasNode[];
        const selectedEdges = (Array.isArray(msg.edges) ? msg.edges : []) as CanvasEdge[];
        const extraPrompt = String(msg.prompt ?? '').trim();
        if (selectedNodes.length === 0) {
          void vscode.window.showWarningMessage('Select at least one wireframe or architecture node on the canvas.');
          return;
        }

        const images: Array<{ mimeType: string; dataBase64: string }> = [];
        for (const node of selectedNodes) {
          if (node.imageDataUrl && node.imageDataUrl.startsWith('data:image/')) {
            const commaIdx = node.imageDataUrl.indexOf(',');
            const meta = node.imageDataUrl.slice(5, commaIdx);
            const mimeType = meta.split(';')[0] || 'image/png';
            const dataBase64 = node.imageDataUrl.slice(commaIdx + 1);
            if (dataBase64) {
              images.push({ mimeType, dataBase64 });
            }
          }
        }

        const layoutSpec = JSON.stringify(
          {
            nodes: selectedNodes.map((n) => ({
              id: n.id,
              kind: n.kind,
              title: n.title,
              body: n.body,
              x: n.x,
              y: n.y,
              width: n.width,
              height: n.height,
            })),
            edges: selectedEdges,
          },
          null,
          2
        );

        const fullPrompt = [
          'Convert the following Rursor Infinite Canvas wireframe / architecture graph into a sleek, self-contained, interactive HTML5 + CSS + vanilla JS UI preview.',
          'Requirements:',
          '- Return ONLY a single complete HTML document (`<!DOCTYPE html>...`) inside an ```html code fence.',
          '- Use modern dark UI styling (`#0B0D12` surface, `#131822` cards, `#7C5CFF` electric violet accents, crisp Inter/system sans-serif typography, subtle borders, interactive hover/tabs/buttons).',
          extraPrompt ? `- Additional user direction: ${extraPrompt}` : '',
          '',
          'Canvas Spatial Specification:',
          '```json',
          layoutSpec,
          '```',
        ]
          .filter(Boolean)
          .join('\n');

        void webviewPanel.webview.postMessage({
          type: 'generationStatus',
          running: true,
          status: 'Synthesizing live UI preview from spatial wireframe...',
        });

        try {
          const res = await this.client.request<{ text: string }>('agent_chat', {
            mode: 'ask',
            useWorkspaceSearch: false,
            includeGitDiff: false,
            messages: [
              {
                role: 'user',
                content: fullPrompt,
                images,
              },
            ],
          });

          const html = extractGeneratedHtmlBlock(res.text ?? '');
          const anchorRight = Math.max(...selectedNodes.map((n) => n.x + n.width));
          const anchorTop = Math.min(...selectedNodes.map((n) => n.y));
          const preview: CanvasPreviewTile = {
            id: `prev_${Date.now()}`,
            title: `Live Preview: ${selectedNodes[0]?.title || 'Component'}`,
            x: anchorRight + 60,
            y: anchorTop,
            width: 560,
            height: 400,
            html,
          };

          void webviewPanel.webview.postMessage({
            type: 'previewGenerated',
            preview,
          });
        } catch (err) {
          const message = err instanceof Error ? err.message : String(err);
          void vscode.window.showErrorMessage(`Canvas UI generation failed: ${message}`);
          void webviewPanel.webview.postMessage({
            type: 'generationStatus',
            running: false,
            status: `Error: ${message}`,
          });
        }
        return;
      }

      if (type === 'exportPreviewToFile') {
        const html = String(msg.html ?? '');
        const suggested = String(msg.suggestedName ?? 'canvas-preview.html');
        const wsFolder = vscode.workspace.workspaceFolders?.[0]?.uri;
        const defaultUri = wsFolder
          ? vscode.Uri.joinPath(wsFolder, suggested)
          : vscode.Uri.file(suggested);
        const saveUri = await vscode.window.showSaveDialog({
          defaultUri,
          saveLabel: 'Save Canvas Component',
          filters: {
            'Web / UI Files': ['html', 'tsx', 'jsx', 'vue'],
          },
        });
        if (saveUri) {
          await fs.mkdir(path.dirname(saveUri.fsPath), { recursive: true });
          await fs.writeFile(saveUri.fsPath, html, 'utf8');
          const opened = await vscode.workspace.openTextDocument(saveUri);
          await vscode.window.showTextDocument(opened, { viewColumn: vscode.ViewColumn.Beside });
        }
      }
    });

    webviewPanel.onDidDispose(() => {
      docChangeSub.dispose();
      msgSub.dispose();
    });
  }

  private buildWebviewHtml(_webview: vscode.Webview, initialModel: CanvasDocumentModel): string {
    const nonce = crypto.randomBytes(16).toString('hex');
    const serialized = JSON.stringify(initialModel).replace(/</g, '\\u003c');

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data: https: http:; style-src 'unsafe-inline'; script-src 'nonce-${nonce}'; frame-src 'self' data: blob:;" />
  <title>Rursor Infinite Canvas</title>
  <style>
    :root {
      --bg: #0B0D12;
      --panel: #131822;
      --border: #232B3B;
      --accent: #7C5CFF;
      --fg: #E6EDF3;
      --muted: #8B96A9;
    }
    * { box-sizing: border-box; user-select: none; }
    html, body {
      margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden;
      background: var(--bg); color: var(--fg);
      font-family: -apple-system, BlinkMacSystemFont, "SF Pro Text", sans-serif;
    }
    #toolbar {
      position: fixed; top: 14px; left: 50%; transform: translateX(-50%);
      z-index: 50; display: flex; align-items: center; gap: 6px;
      background: rgba(19, 24, 34, 0.94); border: 1px solid var(--border);
      border-radius: 10px; padding: 6px 10px; box-shadow: 0 10px 28px rgba(0,0,0,0.45);
    }
    .tool-btn {
      background: transparent; color: var(--fg); border: 1px solid transparent;
      border-radius: 6px; padding: 5px 9px; font-size: 12px; cursor: pointer;
    }
    .tool-btn:hover { background: rgba(124, 92, 255, 0.14); }
    .tool-btn.active { background: rgba(124, 92, 255, 0.25); border-color: var(--accent); color: #fff; }
    .gen-btn {
      background: var(--accent); color: #fff; border: none; border-radius: 6px;
      padding: 6px 12px; font-size: 12px; font-weight: 600; cursor: pointer;
    }
    .gen-btn:hover { filter: brightness(1.1); }
    .sep { width: 1px; height: 18px; background: var(--border); margin: 0 2px; }
    #statusBar {
      position: fixed; bottom: 12px; left: 14px; z-index: 50;
      background: rgba(19, 24, 34, 0.9); border: 1px solid var(--border);
      border-radius: 8px; padding: 5px 10px; font-size: 11px; color: var(--muted);
    }
    #viewport {
      width: 100%; height: 100%; position: relative; cursor: grab;
      background-image: radial-gradient(#252E42 1px, transparent 1px);
      background-size: 20px 20px;
    }
    #world { position: absolute; top: 0; left: 0; transform-origin: 0 0; }
    #edgesSvg { position: absolute; top: 0; left: 0; width: 6000px; height: 6000px; pointer-events: none; overflow: visible; }
    .canvas-node {
      position: absolute; background: var(--panel); border: 1.5px solid var(--border);
      border-radius: 10px; padding: 10px 12px; display: flex; flex-direction: column; gap: 6px;
      box-shadow: 0 6px 20px rgba(0,0,0,0.35); cursor: move;
    }
    .canvas-node.selected { border-color: var(--accent); box-shadow: 0 0 0 2px rgba(124,92,255,0.35); }
    .canvas-node.kind-note { background: #262114; border-color: #6B561B; }
    .canvas-node.kind-service { border-left: 4px solid #22C55E; }
    .canvas-node.kind-frame { border-top: 22px solid #1C2333; }
    .node-title {
      background: transparent; border: none; color: #fff; font-weight: 600; font-size: 13px;
      outline: none; width: 100%; user-select: text;
    }
    .node-body {
      background: transparent; border: none; color: var(--muted); font-size: 12px;
      resize: none; flex: 1; outline: none; width: 100%; font-family: inherit; user-select: text;
    }
    .node-img { max-width: 100%; max-height: 150px; object-fit: contain; border-radius: 6px; }
    .preview-tile {
      position: absolute; background: #0E121B; border: 1.5px solid var(--accent);
      border-radius: 10px; overflow: hidden; display: flex; flex-direction: column;
      box-shadow: 0 12px 32px rgba(0,0,0,0.55);
    }
    .preview-hdr {
      display: flex; align-items: center; justify-content: space-between;
      padding: 6px 10px; background: #181E2C; font-size: 12px; font-weight: 600; cursor: move;
    }
    .preview-hdr button {
      background: #252E42; color: var(--fg); border: none; border-radius: 5px;
      padding: 3px 8px; font-size: 11px; cursor: pointer; margin-left: 4px;
    }
    .preview-frame { flex: 1; width: 100%; border: none; background: #0B0D12; }
  </style>
</head>
<body>
  <div id="toolbar">
    <button class="tool-btn active" data-tool="select">Select (V)</button>
    <button class="tool-btn" data-tool="frame">+ Wireframe (F)</button>
    <button class="tool-btn" data-tool="card">+ Card (R)</button>
    <button class="tool-btn" data-tool="service">+ Service (N)</button>
    <button class="tool-btn" data-tool="note">+ Sticky (S)</button>
    <button class="tool-btn" data-tool="connect">Connect (A)</button>
    <div class="sep"></div>
    <button class="tool-btn" id="deleteBtn">Delete</button>
    <button class="gen-btn" id="generateBtn">Generate Live UI (Cmd+Enter)</button>
  </div>
  <div id="viewport">
    <div id="world">
      <svg id="edgesSvg"></svg>
      <div id="nodesLayer"></div>
      <div id="previewsLayer"></div>
    </div>
  </div>
  <div id="statusBar">Drag background to pan · Scroll to zoom · Paste screenshot image anytime · Select nodes &amp; click Generate Live UI</div>

  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    let model = ${serialized};
    let activeTool = 'select';
    let selectedIds = new Set(model.nodes.slice(0, 1).map(n => n.id));
    let connectSourceId = null;
    let dragState = null;

    const worldEl = document.getElementById('world');
    const nodesLayer = document.getElementById('nodesLayer');
    const previewsLayer = document.getElementById('previewsLayer');
    const edgesSvg = document.getElementById('edgesSvg');
    const statusBar = document.getElementById('statusBar');

    function snap(val) {
      return model.viewport.snapToGrid ? Math.round(val / 20) * 20 : Math.round(val);
    }

    let syncTimer = null;
    function scheduleSync() {
      clearTimeout(syncTimer);
      syncTimer = setTimeout(() => {
        vscode.postMessage({ type: 'syncModel', model });
      }, 180);
    }

    function applyTransform() {
      const { panX, panY, zoom } = model.viewport;
      worldEl.style.transform = 'translate(' + panX + 'px, ' + panY + 'px) scale(' + zoom + ')';
    }

    function renderEdges() {
      edgesSvg.innerHTML = '';
      const defs = document.createElementNS('http://www.w3.org/2000/svg', 'defs');
      defs.innerHTML = '<marker id="arrow" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="#7C5CFF"/></marker>';
      edgesSvg.appendChild(defs);

      for (const edge of model.edges) {
        const a = model.nodes.find(n => n.id === edge.fromId);
        const b = model.nodes.find(n => n.id === edge.toId);
        if (!a || !b) continue;
        const x1 = a.x + a.width;
        const y1 = a.y + a.height / 2;
        const x2 = b.x;
        const y2 = b.y + b.height / 2;
        const cx = (x1 + x2) / 2;
        const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
        path.setAttribute('d', 'M ' + x1 + ' ' + y1 + ' C ' + cx + ' ' + y1 + ', ' + cx + ' ' + y2 + ', ' + x2 + ' ' + y2);
        path.setAttribute('stroke', '#7C5CFF');
        path.setAttribute('stroke-width', '2');
        path.setAttribute('fill', 'none');
        path.setAttribute('marker-end', 'url(#arrow)');
        edgesSvg.appendChild(path);
      }
    }

    function renderAll() {
      applyTransform();
      renderEdges();
      nodesLayer.innerHTML = '';
      for (const node of model.nodes) {
        const el = document.createElement('div');
        el.className = 'canvas-node kind-' + node.kind + (selectedIds.has(node.id) ? ' selected' : '');
        el.style.left = node.x + 'px';
        el.style.top = node.y + 'px';
        el.style.width = node.width + 'px';
        el.style.height = node.height + 'px';

        const titleIn = document.createElement('input');
        titleIn.className = 'node-title';
        titleIn.value = node.title;
        titleIn.addEventListener('input', () => { node.title = titleIn.value; scheduleSync(); });
        el.appendChild(titleIn);

        if (node.imageDataUrl) {
          const img = document.createElement('img');
          img.className = 'node-img';
          img.src = node.imageDataUrl;
          el.appendChild(img);
        }

        const bodyIn = document.createElement('textarea');
        bodyIn.className = 'node-body';
        bodyIn.value = node.body;
        bodyIn.addEventListener('input', () => { node.body = bodyIn.value; scheduleSync(); });
        el.appendChild(bodyIn);

        el.addEventListener('mousedown', (ev) => {
          ev.stopPropagation();
          if (activeTool === 'connect') {
            if (!connectSourceId) {
              connectSourceId = node.id;
              statusBar.textContent = 'Connect mode: click target node...';
            } else if (connectSourceId !== node.id) {
              model.edges.push({ id: 'e_' + Date.now(), fromId: connectSourceId, toId: node.id });
              connectSourceId = null;
              setTool('select');
              renderAll();
              scheduleSync();
            }
            return;
          }
          if (!ev.shiftKey && !selectedIds.has(node.id)) {
            selectedIds = new Set([node.id]);
          } else if (ev.shiftKey) {
            selectedIds.add(node.id);
          }
          renderAll();
          dragState = {
            kind: 'node',
            node,
            startClientX: ev.clientX,
            startClientY: ev.clientY,
            origX: node.x,
            origY: node.y,
          };
        });

        nodesLayer.appendChild(el);
      }

      previewsLayer.innerHTML = '';
      for (const prev of model.previews) {
        const card = document.createElement('div');
        card.className = 'preview-tile';
        card.style.left = prev.x + 'px';
        card.style.top = prev.y + 'px';
        card.style.width = prev.width + 'px';
        card.style.height = prev.height + 'px';

        const hdr = document.createElement('div');
        hdr.className = 'preview-hdr';
        const titleSpan = document.createElement('span');
        titleSpan.textContent = prev.title;
        const actions = document.createElement('div');

        const saveBtn = document.createElement('button');
        saveBtn.textContent = 'Save File...';
        saveBtn.addEventListener('click', (e) => {
          e.stopPropagation();
          vscode.postMessage({ type: 'exportPreviewToFile', html: prev.html, suggestedName: 'component-preview.html' });
        });

        const closeBtn = document.createElement('button');
        closeBtn.textContent = '×';
        closeBtn.addEventListener('click', (e) => {
          e.stopPropagation();
          model.previews = model.previews.filter(p => p.id !== prev.id);
          renderAll();
          scheduleSync();
        });

        actions.appendChild(saveBtn);
        actions.appendChild(closeBtn);
        hdr.appendChild(titleSpan);
        hdr.appendChild(actions);

        hdr.addEventListener('mousedown', (ev) => {
          ev.stopPropagation();
          dragState = {
            kind: 'preview',
            prev,
            startClientX: ev.clientX,
            startClientY: ev.clientY,
            origX: prev.x,
            origY: prev.y,
          };
        });

        const iframe = document.createElement('iframe');
        iframe.className = 'preview-frame';
        iframe.setAttribute('sandbox', 'allow-scripts');
        iframe.srcdoc = prev.html;

        card.appendChild(hdr);
        card.appendChild(iframe);
        previewsLayer.appendChild(card);
      }
    }

    function setTool(tool) {
      activeTool = tool;
      document.querySelectorAll('.tool-btn[data-tool]').forEach(btn => {
        btn.classList.toggle('active', btn.getAttribute('data-tool') === tool);
      });
    }

    document.querySelectorAll('.tool-btn[data-tool]').forEach(btn => {
      btn.addEventListener('click', () => {
        const t = btn.getAttribute('data-tool');
        if (t === 'frame' || t === 'card' || t === 'service' || t === 'note') {
          const id = 'n_' + Date.now();
          const newNode = {
            id,
            kind: t,
            title: t === 'frame' ? 'New App Screen' : t === 'service' ? 'Backend Service' : t === 'note' ? 'Design Note' : 'UI Component',
            body: 'Describe UI elements, state, or API behavior...',
            x: snap((220 - model.viewport.panX) / model.viewport.zoom),
            y: snap((160 - model.viewport.panY) / model.viewport.zoom),
            width: t === 'frame' ? 320 : 230,
            height: t === 'frame' ? 220 : 130,
          };
          model.nodes.push(newNode);
          selectedIds = new Set([id]);
          setTool('select');
          renderAll();
          scheduleSync();
        } else {
          setTool(t);
        }
      });
    });

    document.getElementById('deleteBtn').addEventListener('click', () => {
      if (selectedIds.size === 0) return;
      model.nodes = model.nodes.filter(n => !selectedIds.has(n.id));
      model.edges = model.edges.filter(e => !selectedIds.has(e.fromId) && !selectedIds.has(e.toId));
      selectedIds.clear();
      renderAll();
      scheduleSync();
    });

    function triggerGenerate() {
      const chosen = model.nodes.filter(n => selectedIds.has(n.id));
      const nodes = chosen.length > 0 ? chosen : model.nodes;
      const nodeSet = new Set(nodes.map(n => n.id));
      const edges = model.edges.filter(e => nodeSet.has(e.fromId) || nodeSet.has(e.toId));
      vscode.postMessage({ type: 'generatePreview', nodes, edges });
    }

    document.getElementById('generateBtn').addEventListener('click', triggerGenerate);

    document.getElementById('viewport').addEventListener('mousedown', (ev) => {
      if (ev.target.id === 'viewport' || ev.target.id === 'world') {
        dragState = {
          kind: 'pan',
          startClientX: ev.clientX,
          startClientY: ev.clientY,
          origPanX: model.viewport.panX,
          origPanY: model.viewport.panY,
        };
      }
    });

    window.addEventListener('mousemove', (ev) => {
      if (!dragState) return;
      const dx = ev.clientX - dragState.startClientX;
      const dy = ev.clientY - dragState.startClientY;
      if (dragState.kind === 'pan') {
        model.viewport.panX = dragState.origPanX + dx;
        model.viewport.panY = dragState.origPanY + dy;
        applyTransform();
      } else if (dragState.kind === 'node') {
        dragState.node.x = snap(dragState.origX + dx / model.viewport.zoom);
        dragState.node.y = snap(dragState.origY + dy / model.viewport.zoom);
        renderAll();
      } else if (dragState.kind === 'preview') {
        dragState.prev.x = snap(dragState.origX + dx / model.viewport.zoom);
        dragState.prev.y = snap(dragState.origY + dy / model.viewport.zoom);
        renderAll();
      }
    });

    window.addEventListener('mouseup', () => {
      if (dragState) {
        dragState = null;
        scheduleSync();
      }
    });

    window.addEventListener('wheel', (ev) => {
      ev.preventDefault();
      const factor = ev.deltaY < 0 ? 1.08 : 0.92;
      model.viewport.zoom = Math.min(3, Math.max(0.25, model.viewport.zoom * factor));
      applyTransform();
      scheduleSync();
    }, { passive: false });

    window.addEventListener('paste', (ev) => {
      const items = ev.clipboardData && ev.clipboardData.items;
      if (!items) return;
      for (const item of items) {
        if (item.type.startsWith('image/')) {
          const file = item.getAsFile();
          if (!file) continue;
          const reader = new FileReader();
          reader.onload = () => {
            model.nodes.push({
              id: 'img_' + Date.now(),
              kind: 'image',
              title: 'Reference Screenshot',
              body: 'Match this visual layout & styling',
              x: snap((240 - model.viewport.panX) / model.viewport.zoom),
              y: snap((180 - model.viewport.panY) / model.viewport.zoom),
              width: 280,
              height: 240,
              imageDataUrl: String(reader.result || ''),
            });
            renderAll();
            scheduleSync();
          };
          reader.readAsDataURL(file);
        }
      }
    });

    window.addEventListener('keydown', (ev) => {
      if ((ev.metaKey || ev.ctrlKey) && ev.key === 'Enter') {
        ev.preventDefault();
        triggerGenerate();
      }
    });

    window.addEventListener('message', (ev) => {
      const msg = ev.data || {};
      if (msg.type === 'loadModel' && msg.model) {
        model = msg.model;
        renderAll();
      } else if (msg.type === 'generationStatus') {
        statusBar.textContent = msg.status || '';
      } else if (msg.type === 'previewGenerated' && msg.preview) {
        model.previews.push(msg.preview);
        statusBar.textContent = 'Generated live interactive UI preview tile!';
        renderAll();
        scheduleSync();
      }
    });

    renderAll();
  </script>
</body>
</html>`;
  }
}

export function registerCanvasStudio(
  context: vscode.ExtensionContext,
  client: SidecarClient
): void {
  const provider = new RursorCanvasProvider(context.extensionUri, client);
  context.subscriptions.push(
    vscode.window.registerCustomEditorProvider(CANVAS_EDITOR_VIEW_TYPE, provider, {
      webviewOptions: { retainContextWhenHidden: true },
      supportsMultipleEditorsPerDocument: false,
    }),
    vscode.commands.registerCommand('rursor.openCanvas', async () => {
      const wsFolder = vscode.workspace.workspaceFolders?.[0]?.uri;
      if (wsFolder) {
        const canvasDir = vscode.Uri.joinPath(wsFolder, '.rursor');
        const canvasFile = vscode.Uri.joinPath(canvasDir, 'studio.rursor.canvas');
        try {
          await fs.access(canvasFile.fsPath);
        } catch {
          await fs.mkdir(canvasDir.fsPath, { recursive: true });
          await fs.writeFile(
            canvasFile.fsPath,
            `${JSON.stringify(defaultCanvasModel(), null, 2)}\n`,
            'utf8'
          );
        }
        await vscode.commands.executeCommand('vscode.openWith', canvasFile, CANVAS_EDITOR_VIEW_TYPE);
      } else {
        const doc = await vscode.workspace.openTextDocument({
          language: 'json',
          content: `${JSON.stringify(defaultCanvasModel(), null, 2)}\n`,
        });
        await vscode.window.showTextDocument(doc);
      }
    })
  );
}
