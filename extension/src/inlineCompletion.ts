import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const PREFIX_LINES = 100;
const SUFFIX_LINES = 35;
const NES_BEFORE_LINES = 40;
const NES_FOCUS_BEFORE = 4;
const NES_FOCUS_AFTER = 8;
const NES_AFTER_LINES = 35;
const DEBOUNCE_MS = 220;
const PREFETCH_DEBOUNCE_MS = 350;
const MAX_CACHE_ENTRIES = 60;
const MAX_RECENT_EDITS = 6;

export type NesPredictionKind = 'edit' | 'insert' | 'jump' | 'none';

export interface NesPrediction {
  kind: NesPredictionKind;
  targetPath: string;
  startLine: number;
  endLine: number;
  replacement: string;
  reason: string;
  jumpToLine?: number | null;
}

export interface NextEditPredictResponse {
  prediction?: NesPrediction | null;
}

interface PendingNesState {
  prediction: NesPrediction;
  targetUri: vscode.Uri;
  anchorLineZeroBased: number;
}

interface PendingJumpState {
  prediction: NesPrediction;
  targetUri: vscode.Uri;
  targetLineZeroBased: number;
  anchorUri: vscode.Uri;
  anchorLineZeroBased: number;
}

interface ActiveGhostState {
  uri: string;
  position: vscode.Position;
  fullText: string;
}

/**
 * Extracts the next logical word/token (including leading horizontal whitespace or
 * newline+indentation) for word-by-word ghost text acceptance (`Cmd+Right`).
 */
export function extractNextWordToken(text: string): string {
  if (!text) {
    return '';
  }
  if (text.startsWith('\n')) {
    const match = /^\n[ \t]*(?:[A-Za-z0-9_$]+|[^\sA-Za-z0-9_$])?/.exec(text);
    return match ? match[0] : '\n';
  }
  const wordMatch = /^[ \t]*[A-Za-z0-9_$]+/.exec(text);
  if (wordMatch) {
    return wordMatch[0];
  }
  const opMatch = /^[ \t]*(?:=>|===|!==|==|!=|<=|>=|&&|\|\||::|\.\.\.?|[^\sA-Za-z0-9_$])/.exec(
    text
  );
  if (opMatch) {
    return opMatch[0];
  }
  return text.slice(0, 1);
}

/**
 * Registers VS Code commands for Next-Edit Prediction, Tab-to-Jump, and
 * word-by-word inline completion acceptance.
 */
export function registerInlineCompletionCommands(
  context: vscode.ExtensionContext,
  provider: RursorInlineCompletionProvider
): vscode.Disposable[] {
  const registered = [
    vscode.commands.registerCommand('rursor.acceptNextEdit', () => provider.acceptNextEdit()),
    vscode.commands.registerCommand('rursor.dismissNextEdit', () => provider.dismissNextEdit()),
    vscode.commands.registerCommand('rursor.jumpToNextEdit', () => provider.jumpToNextEdit()),
    vscode.commands.registerCommand('rursor.acceptNextWord', () => provider.acceptNextWord()),
  ];
  context.subscriptions.push(...registered);
  return registered;
}

export class RursorInlineCompletionProvider
  implements vscode.InlineCompletionItemProvider, vscode.Disposable
{
  private activeRequestId: number | undefined;
  private prefetchRequestId: number | undefined;
  private prefetchTimer: ReturnType<typeof setTimeout> | undefined;
  private nesMethodUnsupported = false;
  private applyingEdit = false;

  private readonly cache = new Map<string, string>();
  private readonly recentSnippets = new Map<string, string>();
  private readonly recentEdits: string[] = [];
  private recentEditHint = '';

  private pendingNes: PendingNesState | undefined;
  private pendingJump: PendingJumpState | undefined;
  private activeGhost: ActiveGhostState | undefined;
  private hasNextEditContext = false;
  private hasJumpTargetContext = false;

  private readonly nesRemovedDecoration: vscode.TextEditorDecorationType;
  private readonly nesPreviewDecoration: vscode.TextEditorDecorationType;
  private readonly jumpBannerDecoration: vscode.TextEditorDecorationType;
  private readonly statusBarItem: vscode.StatusBarItem;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(private readonly client: SidecarClient) {
    this.nesRemovedDecoration = vscode.window.createTextEditorDecorationType({
      backgroundColor: 'rgba(248, 81, 73, 0.22)',
      borderWidth: '0 0 0 3px',
      borderStyle: 'solid',
      borderColor: 'rgba(248, 81, 73, 0.90)',
      textDecoration: 'line-through',
      opacity: '0.82',
      isWholeLine: true,
    });

    this.nesPreviewDecoration = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
    });

    this.jumpBannerDecoration = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
    });

    this.statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 98);

    this.disposables.push(
      this.nesRemovedDecoration,
      this.nesPreviewDecoration,
      this.jumpBannerDecoration,
      this.statusBarItem,
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor) {
          this.recordDocument(editor.document);
        }
        if (!this.applyingEdit) {
          this.clearActiveGhost();
          if (
            this.pendingNes &&
            (!editor || editor.document.uri.toString() !== this.pendingNes.targetUri.toString())
          ) {
            void this.dismissNextEdit();
          }
          if (
            this.pendingJump &&
            (!editor || editor.document.uri.toString() !== this.pendingJump.anchorUri.toString())
          ) {
            void this.clearJumpTarget();
          }
        }
      }),
      vscode.workspace.onDidChangeTextDocument((event) => {
        this.recordDocument(event.document);
        if (event.document.uri.scheme === 'file' && event.contentChanges.length > 0) {
          const change = event.contentChanges[0];
          const rel = vscode.workspace.asRelativePath(event.document.uri, false);
          const hint = `${rel}:${change.range.start.line + 1} -> ${change.text.slice(0, 240)}`;
          this.recentEditHint = hint;
          this.recentEdits.push(hint);
          while (this.recentEdits.length > MAX_RECENT_EDITS) {
            this.recentEdits.shift();
          }
        }
        if (!this.applyingEdit && event.contentChanges.length > 0) {
          this.clearActiveGhost();
          if (
            this.pendingNes &&
            event.document.uri.toString() === this.pendingNes.targetUri.toString()
          ) {
            void this.dismissNextEdit();
          }
          if (
            this.pendingJump &&
            event.document.uri.toString() === this.pendingJump.anchorUri.toString()
          ) {
            void this.clearJumpTarget();
          }
        }
      }),
      vscode.window.onDidChangeTextEditorSelection((event) => {
        this.handleSelectionChange(event);
      })
    );
  }

  private recordDocument(doc: vscode.TextDocument): void {
    if (doc.uri.scheme !== 'file' || doc.isUntitled) {
      return;
    }
    const relPath = vscode.workspace.asRelativePath(doc.uri, false);
    const snippet = doc.getText().slice(0, 2400);
    if (this.recentSnippets.has(relPath)) {
      this.recentSnippets.delete(relPath);
    }
    this.recentSnippets.set(relPath, snippet);
    while (this.recentSnippets.size > 4) {
      const oldest = this.recentSnippets.keys().next().value;
      if (oldest !== undefined) {
        this.recentSnippets.delete(oldest);
      } else {
        break;
      }
    }
  }

  private handleSelectionChange(event: vscode.TextEditorSelectionChangeEvent): void {
    if (this.applyingEdit) {
      return;
    }

    const editor = event.textEditor;
    const doc = editor.document;
    if (doc.uri.scheme !== 'file' && doc.uri.scheme !== 'untitled') {
      return;
    }

    const pos = event.selections[0]?.active ?? editor.selection.active;

    if (
      this.pendingNes &&
      doc.uri.toString() === this.pendingNes.targetUri.toString() &&
      (pos.line < this.pendingNes.prediction.startLine - 5 ||
        pos.line > this.pendingNes.prediction.endLine + 4)
    ) {
      void this.dismissNextEdit();
    }

    if (
      this.pendingJump &&
      doc.uri.toString() === this.pendingJump.anchorUri.toString() &&
      Math.abs(pos.line - this.pendingJump.anchorLineZeroBased) > 3
    ) {
      void this.clearJumpTarget();
    }

    if (this.activeGhost && !this.activeGhost.position.isEqual(pos)) {
      this.clearActiveGhost();
    }

    this.scheduleSpeculativePrefetch(editor, pos);
  }

  private scheduleSpeculativePrefetch(editor: vscode.TextEditor, position: vscode.Position): void {
    if (this.prefetchTimer !== undefined) {
      clearTimeout(this.prefetchTimer);
      this.prefetchTimer = undefined;
    }

    const config = vscode.workspace.getConfiguration('rursor');
    if (!config.get<boolean>('autocomplete.enabled', true)) {
      return;
    }

    const doc = editor.document;
    const { prefix, suffix, cacheKey } = this.extractWindowAndCacheKey(doc, position);
    if (prefix.trim().length < 3) {
      return;
    }

    if (this.cache.has(cacheKey) || this.pendingNes || this.pendingJump) {
      return;
    }

    this.prefetchTimer = setTimeout(() => {
      this.prefetchTimer = undefined;
      const active = vscode.window.activeTextEditor;
      if (
        !active ||
        active.document.uri.toString() !== doc.uri.toString() ||
        !active.selection.active.isEqual(position)
      ) {
        return;
      }
      void this.runSpeculativePrefetch(active, position, prefix, suffix, cacheKey);
    }, PREFETCH_DEBOUNCE_MS);
  }

  private async runSpeculativePrefetch(
    editor: vscode.TextEditor,
    position: vscode.Position,
    prefix: string,
    suffix: string,
    cacheKey: string
  ): Promise<void> {
    if (this.prefetchRequestId !== undefined) {
      void this.client.cancel(this.prefetchRequestId);
      this.prefetchRequestId = undefined;
    }

    const doc = editor.document;
    const relPath = vscode.workspace.asRelativePath(doc.uri, false);

    try {
      const nes = await this.requestNesPrediction(doc, position, (id) => {
        this.prefetchRequestId = id;
      });

      if (
        vscode.window.activeTextEditor?.document.uri.toString() !== doc.uri.toString() ||
        !vscode.window.activeTextEditor.selection.active.isEqual(position)
      ) {
        return;
      }

      if (nes) {
        const handled = await this.handleNesPrediction(editor, doc, position, relPath, nes, cacheKey);
        if (handled !== 'none') {
          return;
        }
      }

      const completion = await this.requestInlineComplete(doc, prefix, suffix, (id) => {
        this.prefetchRequestId = id;
      });
      if (completion !== undefined) {
        this.rememberCache(cacheKey, completion);
        if (completion.trim()) {
          this.activeGhost = {
            uri: doc.uri.toString(),
            position,
            fullText: completion,
          };
        }
      }
    } catch {
      // Speculative prefetch errors are intentionally silent.
    } finally {
      this.prefetchRequestId = undefined;
    }
  }

  async provideInlineCompletionItems(
    document: vscode.TextDocument,
    position: vscode.Position,
    context: vscode.InlineCompletionContext,
    token: vscode.CancellationToken
  ): Promise<vscode.InlineCompletionItem[]> {
    const config = vscode.workspace.getConfiguration('rursor');
    if (!config.get<boolean>('autocomplete.enabled', true)) {
      return [];
    }

    if (document.uri.scheme !== 'file' && document.uri.scheme !== 'untitled') {
      return [];
    }

    if (
      context.selectedCompletionInfo &&
      context.triggerKind === vscode.InlineCompletionTriggerKind.Automatic
    ) {
      return [];
    }

    const { prefix, suffix, cacheKey } = this.extractWindowAndCacheKey(document, position);
    if (prefix.trim().length < 3) {
      return [];
    }

    const cached = this.cache.get(cacheKey);
    if (cached !== undefined) {
      if (cached) {
        this.activeGhost = {
          uri: document.uri.toString(),
          position,
          fullText: cached,
        };
        return [new vscode.InlineCompletionItem(cached, new vscode.Range(position, position))];
      }
      return [];
    }

    if (context.triggerKind === vscode.InlineCompletionTriggerKind.Automatic) {
      const stillValid = await new Promise<boolean>((resolve) => {
        const timer = setTimeout(() => resolve(!token.isCancellationRequested), DEBOUNCE_MS);
        const sub = token.onCancellationRequested(() => {
          clearTimeout(timer);
          sub.dispose();
          resolve(false);
        });
      });
      if (!stillValid) {
        return [];
      }
    }

    if (this.activeRequestId !== undefined) {
      void this.client.cancel(this.activeRequestId);
      this.activeRequestId = undefined;
    }
    if (this.prefetchRequestId !== undefined) {
      void this.client.cancel(this.prefetchRequestId);
      this.prefetchRequestId = undefined;
    }

    const relPath = vscode.workspace.asRelativePath(document.uri, false);
    const activeEditor =
      vscode.window.activeTextEditor?.document.uri.toString() === document.uri.toString()
        ? vscode.window.activeTextEditor
        : undefined;

    const cancelSub = token.onCancellationRequested(() => {
      if (this.activeRequestId !== undefined) {
        void this.client.cancel(this.activeRequestId);
      }
    });

    try {
      const nes = await this.requestNesPrediction(document, position, (id) => {
        this.activeRequestId = id;
      });
      if (token.isCancellationRequested) {
        return [];
      }

      if (nes && activeEditor) {
        const outcome = await this.handleNesPrediction(
          activeEditor,
          document,
          position,
          relPath,
          nes,
          cacheKey
        );
        if (outcome === 'ghost') {
          const ghostText = this.cache.get(cacheKey) ?? '';
          if (ghostText) {
            return [
              new vscode.InlineCompletionItem(ghostText, new vscode.Range(position, position)),
            ];
          }
        } else if (outcome === 'nes' || outcome === 'jump') {
          return [];
        }
      }

      const completion = await this.requestInlineComplete(document, prefix, suffix, (id) => {
        this.activeRequestId = id;
      });
      if (token.isCancellationRequested || completion === undefined) {
        return [];
      }
      if (!completion.trim()) {
        this.rememberCache(cacheKey, '');
        return [];
      }
      this.rememberCache(cacheKey, completion);
      this.activeGhost = {
        uri: document.uri.toString(),
        position,
        fullText: completion,
      };
      return [new vscode.InlineCompletionItem(completion, new vscode.Range(position, position))];
    } catch {
      return [];
    } finally {
      cancelSub.dispose();
      this.activeRequestId = undefined;
    }
  }

  /**
   * Accepts the active multi-line/block Next-Edit Prediction and applies it via WorkspaceEdit.
   */
  async acceptNextEdit(): Promise<boolean> {
    const pending = this.pendingNes;
    if (!pending) {
      return false;
    }

    const pred = pending.prediction;
    const doc = await vscode.workspace.openTextDocument(pending.targetUri);
    const editor =
      vscode.window.activeTextEditor?.document.uri.toString() === doc.uri.toString()
        ? vscode.window.activeTextEditor
        : await vscode.window.showTextDocument(doc, { preview: false });

    const edit = new vscode.WorkspaceEdit();
    const startIdx = Math.max(0, Math.min(doc.lineCount - 1, pred.startLine - 1));
    const endIdx = Math.max(startIdx, Math.min(doc.lineCount - 1, pred.endLine - 1));
    const replacement = pred.replacement.replace(/\r\n/g, '\n');

    if (pred.kind === 'insert') {
      const targetLine = doc.lineAt(startIdx);
      if (targetLine.text.trim().length === 0) {
        edit.replace(doc.uri, targetLine.range, replacement);
      } else {
        edit.insert(doc.uri, new vscode.Position(startIdx, 0), `${replacement}\n`);
      }
    } else {
      const replaceRange = new vscode.Range(
        new vscode.Position(startIdx, 0),
        doc.lineAt(endIdx).range.end
      );
      edit.replace(doc.uri, replaceRange, replacement);
    }

    this.applyingEdit = true;
    try {
      const applied = await vscode.workspace.applyEdit(edit);
      if (!applied) {
        return false;
      }
      await this.dismissNextEdit();

      const replacementLines = replacement.split('\n');
      const endLineAfter = Math.max(0, Math.min(doc.lineCount - 1, startIdx + replacementLines.length - 1));
      const endChar = doc.lineAt(endLineAfter).text.length;
      const newPos = new vscode.Position(endLineAfter, endChar);
      editor.selection = new vscode.Selection(newPos, newPos);
      editor.revealRange(new vscode.Range(newPos, newPos), vscode.TextEditorRevealType.InCenterIfOutsideViewport);

      if (pred.jumpToLine && pred.jumpToLine > 0) {
        const jumpUri = this.resolveTargetUri(pred.targetPath, doc.uri);
        const targetLineZeroBased = Math.max(0, pred.jumpToLine - 1);
        if (
          jumpUri.toString() !== doc.uri.toString() ||
          Math.abs(targetLineZeroBased - endLineAfter) > 2
        ) {
          await this.showJumpBanner(editor, pred, jumpUri, targetLineZeroBased, endLineAfter);
        }
      }
      return true;
    } finally {
      this.applyingEdit = false;
    }
  }

  /**
   * Dismisses any active Next-Edit Prediction (and any active Tab-to-Jump banner).
   */
  async dismissNextEdit(): Promise<void> {
    if (this.pendingNes) {
      const targetUriStr = this.pendingNes.targetUri.toString();
      this.pendingNes = undefined;
      for (const editor of vscode.window.visibleTextEditors) {
        if (editor.document.uri.toString() === targetUriStr) {
          editor.setDecorations(this.nesRemovedDecoration, []);
          editor.setDecorations(this.nesPreviewDecoration, []);
        }
      }
    }
    await this.setContext('rursor.hasNextEdit', false);
    if (!this.pendingJump) {
      this.statusBarItem.hide();
    } else {
      await this.clearJumpTarget();
    }
  }

  /**
   * Executes Tab-to-Jump: navigates to the predicted target file/line and, if the
   * jump prediction carries a replacement edit, previews it at the destination.
   */
  async jumpToNextEdit(): Promise<boolean> {
    const jump = this.pendingJump;
    if (!jump) {
      return false;
    }

    this.applyingEdit = true;
    try {
      const doc = await vscode.workspace.openTextDocument(jump.targetUri);
      const editor = await vscode.window.showTextDocument(doc, { preview: false });
      const lineIdx = Math.max(0, Math.min(doc.lineCount - 1, jump.targetLineZeroBased));
      const pos = new vscode.Position(lineIdx, 0);
      editor.selection = new vscode.Selection(pos, pos);
      editor.revealRange(new vscode.Range(pos, pos), vscode.TextEditorRevealType.InCenter);

      await this.clearJumpTarget();

      if (
        (jump.prediction.kind === 'edit' || jump.prediction.kind === 'insert') &&
        jump.prediction.replacement.trim().length > 0
      ) {
        await this.showNesDecorations(editor, jump.prediction);
      }
      return true;
    } finally {
      this.applyingEdit = false;
    }
  }

  /**
   * Accepts the next word/token (`Cmd+Right`) from the active or cached inline ghost completion.
   */
  async acceptNextWord(): Promise<boolean> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      return false;
    }

    const doc = editor.document;
    const pos = editor.selection.active;
    const { prefix, suffix, cacheKey } = this.extractWindowAndCacheKey(doc, pos);

    let fullText: string | undefined;
    if (
      this.activeGhost &&
      this.activeGhost.uri === doc.uri.toString() &&
      this.activeGhost.position.isEqual(pos)
    ) {
      fullText = this.activeGhost.fullText;
    } else {
      fullText = this.cache.get(cacheKey);
    }

    if (!fullText) {
      return false;
    }

    const token = extractNextWordToken(fullText);
    if (!token) {
      return false;
    }

    this.applyingEdit = true;
    try {
      const applied = await editor.edit(
        (editBuilder) => {
          editBuilder.insert(pos, token);
        },
        { undoStopBefore: true, undoStopAfter: false }
      );
      if (!applied) {
        return false;
      }

      const tokenLines = token.split('\n');
      const newLine = pos.line + tokenLines.length - 1;
      const newChar =
        tokenLines.length === 1
          ? pos.character + token.length
          : tokenLines[tokenLines.length - 1].length;
      const newPos = new vscode.Position(newLine, newChar);
      editor.selection = new vscode.Selection(newPos, newPos);

      const remaining = fullText.slice(token.length);
      if (remaining.length > 0) {
        const nextPrefix = `${prefix}${token}`.slice(-180);
        const nextCacheKey = `${doc.uri.toString()}::${nextPrefix}::${suffix.slice(0, 80)}`;
        this.rememberCache(nextCacheKey, remaining);
        this.activeGhost = {
          uri: doc.uri.toString(),
          position: newPos,
          fullText: remaining,
        };
        void vscode.commands.executeCommand('editor.action.inlineSuggest.trigger');
      } else {
        this.clearActiveGhost();
      }
      return true;
    } finally {
      this.applyingEdit = false;
    }
  }

  private async requestNesPrediction(
    document: vscode.TextDocument,
    position: vscode.Position,
    onRequestId: (id: number) => void
  ): Promise<NesPrediction | null> {
    if (this.nesMethodUnsupported) {
      return null;
    }

    const config = vscode.workspace.getConfiguration('rursor');
    const model = config.get<string>('autocomplete.model', 'gemini-2.5-flash-lite');
    const relPath = vscode.workspace.asRelativePath(document.uri, false);

    const focusStartLine = Math.max(0, position.line - NES_FOCUS_BEFORE);
    const focusEndLine = Math.min(document.lineCount - 1, position.line + NES_FOCUS_AFTER);
    const windowBeforeStart = Math.max(0, focusStartLine - NES_BEFORE_LINES);
    const windowAfterEnd = Math.min(document.lineCount - 1, focusEndLine + NES_AFTER_LINES);

    const windowBefore =
      focusStartLine > windowBeforeStart
        ? document.getText(
            new vscode.Range(
              new vscode.Position(windowBeforeStart, 0),
              document.lineAt(focusStartLine - 1).range.end
            )
          )
        : '';

    const focusBlock = document.getText(
      new vscode.Range(
        new vscode.Position(focusStartLine, 0),
        document.lineAt(focusEndLine).range.end
      )
    );

    const windowAfter =
      windowAfterEnd > focusEndLine
        ? document.getText(
            new vscode.Range(
              new vscode.Position(focusEndLine + 1, 0),
              document.lineAt(windowAfterEnd).range.end
            )
          )
        : '';

    const { id, promise } = this.client.request<NextEditPredictResponse | NesPrediction>(
      'next_edit_predict',
      {
        filePath: relPath,
        languageId: document.languageId,
        windowBefore,
        focusBlock,
        windowAfter,
        focusStartLine: focusStartLine + 1,
        cursorLine: position.line + 1,
        cursorColumn: position.character + 1,
        recentFiles: this.collectRecentFiles(relPath),
        recentEdits: [...this.recentEdits],
        recentEditHint: this.recentEditHint || undefined,
        model,
      }
    );
    onRequestId(id);

    try {
      const res = await promise;
      const raw =
        res && typeof res === 'object' && 'prediction' in res
          ? (res as NextEditPredictResponse).prediction
          : (res as NesPrediction);
      if (!raw || !raw.kind || raw.kind === 'none') {
        return null;
      }
      return {
        kind: raw.kind,
        targetPath: raw.targetPath || relPath,
        startLine: Math.max(1, raw.startLine || position.line + 1),
        endLine: Math.max(1, raw.endLine || raw.startLine || position.line + 1),
        replacement: (raw.replacement ?? '').replace(/\r\n/g, '\n'),
        reason: raw.reason ?? '',
        jumpToLine: raw.jumpToLine ?? null,
      };
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      if (/unknown method/i.test(msg)) {
        this.nesMethodUnsupported = true;
      }
      return null;
    }
  }

  private async requestInlineComplete(
    document: vscode.TextDocument,
    prefix: string,
    suffix: string,
    onRequestId: (id: number) => void
  ): Promise<string | undefined> {
    const config = vscode.workspace.getConfiguration('rursor');
    const model = config.get<string>('autocomplete.model', 'gemini-2.5-flash-lite');
    const relPath = vscode.workspace.asRelativePath(document.uri, false);

    const { id, promise } = this.client.request<{ completion: string }>('inline_complete', {
      filePath: relPath,
      languageId: document.languageId,
      prefix,
      suffix,
      recentFiles: this.collectRecentFiles(relPath),
      recentEditHint: this.recentEditHint || undefined,
      model,
    });
    onRequestId(id);

    const res = await promise;
    return (res.completion ?? '').replace(/\r\n/g, '\n');
  }

  private async handleNesPrediction(
    editor: vscode.TextEditor,
    document: vscode.TextDocument,
    position: vscode.Position,
    relPath: string,
    pred: NesPrediction,
    cacheKey: string
  ): Promise<'ghost' | 'nes' | 'jump' | 'none'> {
    if (pred.kind === 'none') {
      return 'none';
    }

    const targetUri = this.resolveTargetUri(pred.targetPath, document.uri);
    const sameFile = targetUri.toString() === document.uri.toString();
    const cursorLineOneBased = position.line + 1;

    if (
      pred.kind === 'jump' ||
      !sameFile ||
      (pred.jumpToLine != null &&
        pred.jumpToLine > 0 &&
        pred.kind !== 'edit' &&
        Math.abs(pred.jumpToLine - cursorLineOneBased) > 2)
    ) {
      const jumpLine = pred.jumpToLine && pred.jumpToLine > 0 ? pred.jumpToLine : pred.startLine;
      await this.showJumpBanner(
        editor,
        pred,
        targetUri,
        Math.max(0, jumpLine - 1),
        position.line
      );
      return 'jump';
    }

    if (
      pred.kind === 'insert' &&
      pred.startLine === cursorLineOneBased &&
      pred.endLine === pred.startLine &&
      pred.replacement.trim().length > 0
    ) {
      this.rememberCache(cacheKey, pred.replacement);
      this.activeGhost = {
        uri: document.uri.toString(),
        position,
        fullText: pred.replacement,
      };
      return 'ghost';
    }

    if (pred.kind === 'edit' || pred.kind === 'insert') {
      await this.showNesDecorations(editor, pred);
      return 'nes';
    }

    return 'none';
  }

  private async showNesDecorations(
    editor: vscode.TextEditor,
    pred: NesPrediction
  ): Promise<void> {
    await this.clearJumpTarget();

    const doc = editor.document;
    const startIdx = Math.max(0, Math.min(doc.lineCount - 1, pred.startLine - 1));
    const endIdx = Math.max(startIdx, Math.min(doc.lineCount - 1, pred.endLine - 1));

    const removedRanges: vscode.Range[] = [];
    const previewOptions: vscode.DecorationOptions[] = [];
    const replacementLines = pred.replacement.split('\n');
    const replacedLineCount = endIdx - startIdx + 1;

    if (pred.kind === 'edit') {
      for (let line = startIdx; line <= endIdx; line++) {
        removedRanges.push(doc.lineAt(line).range);
      }
    }

    for (let offset = 0; offset < replacedLineCount; offset++) {
      const lineIdx = startIdx + offset;
      const lineRange = doc.lineAt(lineIdx).range;
      const previewLine = replacementLines[offset];
      const isLastShown = offset === replacedLineCount - 1;
      const extraCount = replacementLines.length - replacedLineCount;

      let contentText = '';
      if (previewLine !== undefined) {
        const trimmedPreview = previewLine.trimEnd();
        contentText =
          pred.kind === 'insert' ? `  + ${trimmedPreview}` : `  → ${trimmedPreview}`;
      } else if (offset === 0 && pred.replacement === '') {
        contentText = '  → (delete)';
      }

      if (isLastShown && extraCount > 0) {
        contentText += ` (+${extraCount} line${extraCount === 1 ? '' : 's'})`;
      }

      if (offset === 0) {
        const reasonTag = pred.reason ? ` — ${pred.reason}` : '';
        contentText += `   [NES${reasonTag}: Accept / Dismiss]`;
      }

      if (contentText) {
        previewOptions.push({
          range: lineRange,
          renderOptions: {
            after: {
              contentText,
              color: 'rgba(63, 185, 80, 0.95)',
              backgroundColor: 'rgba(46, 160, 67, 0.16)',
              border: '1px solid rgba(46, 160, 67, 0.45)',
              margin: '0 0 0 12px',
              fontStyle: 'italic',
            },
          },
        });
      }
    }

    editor.setDecorations(this.nesRemovedDecoration, removedRanges);
    editor.setDecorations(this.nesPreviewDecoration, previewOptions);

    this.pendingNes = {
      prediction: pred,
      targetUri: doc.uri,
      anchorLineZeroBased: startIdx,
    };
    await this.setContext('rursor.hasNextEdit', true);

    const shortReason = pred.reason ? `: ${pred.reason.slice(0, 48)}` : '';
    this.statusBarItem.text = `$(sparkle) Next Edit${shortReason}`;
    this.statusBarItem.tooltip = 'Click to accept Rursor Next-Edit Prediction (or Esc to dismiss)';
    this.statusBarItem.command = 'rursor.acceptNextEdit';
    this.statusBarItem.show();
  }

  private async showJumpBanner(
    editor: vscode.TextEditor,
    pred: NesPrediction,
    targetUri: vscode.Uri,
    targetLineZeroBased: number,
    anchorLineZeroBased: number
  ): Promise<void> {
    if (this.pendingNes) {
      await this.dismissNextEdit();
    }

    const doc = editor.document;
    const clampedAnchor = Math.max(0, Math.min(doc.lineCount - 1, anchorLineZeroBased));
    const displayFile =
      targetUri.toString() === doc.uri.toString()
        ? path.basename(doc.fileName)
        : pred.targetPath || vscode.workspace.asRelativePath(targetUri, false);
    const displayLine = targetLineZeroBased + 1;
    const bannerText = `  Tab → Jump to ${displayFile}:${displayLine}  `;

    editor.setDecorations(this.jumpBannerDecoration, [
      {
        range: doc.lineAt(clampedAnchor).range,
        renderOptions: {
          after: {
            contentText: bannerText,
            color: '#79c0ff',
            backgroundColor: 'rgba(56, 139, 253, 0.18)',
            border: '1px solid rgba(56, 139, 253, 0.55)',
            margin: '0 0 0 12px',
            fontStyle: 'italic',
          },
        },
      },
    ]);

    this.pendingJump = {
      prediction: pred,
      targetUri,
      targetLineZeroBased,
      anchorUri: doc.uri,
      anchorLineZeroBased: clampedAnchor,
    };
    await this.setContext('rursor.hasJumpTarget', true);

    this.statusBarItem.text = `$(arrow-right) Tab → Jump to ${displayFile}:${displayLine}`;
    this.statusBarItem.tooltip = pred.reason || `Jump to ${displayFile}:${displayLine}`;
    this.statusBarItem.command = 'rursor.jumpToNextEdit';
    this.statusBarItem.show();
  }

  private async clearJumpTarget(): Promise<void> {
    if (this.pendingJump) {
      const anchorUriStr = this.pendingJump.anchorUri.toString();
      this.pendingJump = undefined;
      for (const editor of vscode.window.visibleTextEditors) {
        if (editor.document.uri.toString() === anchorUriStr) {
          editor.setDecorations(this.jumpBannerDecoration, []);
        }
      }
    }
    await this.setContext('rursor.hasJumpTarget', false);
    if (!this.pendingNes) {
      this.statusBarItem.hide();
    }
  }

  private clearActiveGhost(): void {
    this.activeGhost = undefined;
  }

  private async setContext(
    key: 'rursor.hasNextEdit' | 'rursor.hasJumpTarget',
    value: boolean
  ): Promise<void> {
    if (key === 'rursor.hasNextEdit') {
      if (this.hasNextEditContext === value) {
        return;
      }
      this.hasNextEditContext = value;
    } else {
      if (this.hasJumpTargetContext === value) {
        return;
      }
      this.hasJumpTargetContext = value;
    }
    await vscode.commands.executeCommand('setContext', key, value);
  }

  private resolveTargetUri(targetPath: string, fallbackUri: vscode.Uri): vscode.Uri {
    const trimmed = targetPath.trim();
    if (!trimmed) {
      return fallbackUri;
    }
    if (path.isAbsolute(trimmed)) {
      return vscode.Uri.file(trimmed);
    }
    const workspaceFolder =
      vscode.workspace.getWorkspaceFolder(fallbackUri) ?? vscode.workspace.workspaceFolders?.[0];
    if (workspaceFolder) {
      return vscode.Uri.joinPath(workspaceFolder.uri, ...trimmed.split(/[\\/]+/).filter(Boolean));
    }
    return vscode.Uri.file(path.resolve(path.dirname(fallbackUri.fsPath), trimmed));
  }

  private extractWindowAndCacheKey(
    document: vscode.TextDocument,
    position: vscode.Position
  ): { prefix: string; suffix: string; cacheKey: string } {
    const prefixStart = Math.max(0, position.line - PREFIX_LINES);
    const suffixEnd = Math.min(document.lineCount - 1, position.line + SUFFIX_LINES);

    const prefix = document.getText(
      new vscode.Range(new vscode.Position(prefixStart, 0), position)
    );
    const suffix = document.getText(
      new vscode.Range(position, document.lineAt(suffixEnd).range.end)
    );

    const cacheKey = `${document.uri.toString()}::${prefix.slice(-180)}::${suffix.slice(0, 80)}`;
    return { prefix, suffix, cacheKey };
  }

  private collectRecentFiles(currentRelPath: string): Array<{ path: string; content: string }> {
    const recentFiles: Array<{ path: string; content: string }> = [];
    for (const [p, content] of this.recentSnippets.entries()) {
      if (p !== currentRelPath) {
        recentFiles.push({ path: p, content });
      }
    }
    return recentFiles.slice(-2);
  }

  private rememberCache(key: string, value: string): void {
    if (this.cache.size >= MAX_CACHE_ENTRIES) {
      const firstKey = this.cache.keys().next().value;
      if (firstKey !== undefined) {
        this.cache.delete(firstKey);
      }
    }
    this.cache.set(key, value);
  }

  dispose(): void {
    if (this.prefetchTimer !== undefined) {
      clearTimeout(this.prefetchTimer);
      this.prefetchTimer = undefined;
    }
    if (this.activeRequestId !== undefined) {
      void this.client.cancel(this.activeRequestId);
      this.activeRequestId = undefined;
    }
    if (this.prefetchRequestId !== undefined) {
      void this.client.cancel(this.prefetchRequestId);
      this.prefetchRequestId = undefined;
    }
    void this.setContext('rursor.hasNextEdit', false);
    void this.setContext('rursor.hasJumpTarget', false);
    for (const d of this.disposables) {
      d.dispose();
    }
  }
}
