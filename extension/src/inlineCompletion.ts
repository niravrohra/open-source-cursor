import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const PREFIX_LINES = 100;
const SUFFIX_LINES = 35;
const DEBOUNCE_MS = 260;
const MAX_CACHE_ENTRIES = 60;

export class RursorInlineCompletionProvider
  implements vscode.InlineCompletionItemProvider, vscode.Disposable
{
  private activeRequestId: number | undefined;
  private readonly cache = new Map<string, string>();
  private readonly recentSnippets = new Map<string, string>();
  private readonly disposables: vscode.Disposable[] = [];

  constructor(private readonly client: SidecarClient) {
    this.disposables.push(
      vscode.window.onDidChangeActiveTextEditor((editor) => {
        if (editor) {
          this.recordDocument(editor.document);
        }
      }),
      vscode.workspace.onDidChangeTextDocument((event) => {
        this.recordDocument(event.document);
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

    // Avoid firing when a completion widget is explicitly selected unless invoked manually.
    if (
      context.selectedCompletionInfo &&
      context.triggerKind === vscode.InlineCompletionTriggerKind.Automatic
    ) {
      return [];
    }

    const prefixStart = Math.max(0, position.line - PREFIX_LINES);
    const suffixEnd = Math.min(document.lineCount - 1, position.line + SUFFIX_LINES);

    const prefix = document.getText(
      new vscode.Range(new vscode.Position(prefixStart, 0), position)
    );
    const suffix = document.getText(
      new vscode.Range(position, document.lineAt(suffixEnd).range.end)
    );

    if (prefix.trim().length < 3) {
      return [];
    }

    const cacheKey = `${document.uri.toString()}::${prefix.slice(-180)}::${suffix.slice(0, 80)}`;
    const cached = this.cache.get(cacheKey);
    if (cached !== undefined) {
      return cached
        ? [new vscode.InlineCompletionItem(cached, new vscode.Range(position, position))]
        : [];
    }

    // Debounce rapid keystrokes before calling Gemini.
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

    const relPath = vscode.workspace.asRelativePath(document.uri, false);
    const recentFiles: Array<{ path: string; content: string }> = [];
    for (const [p, content] of this.recentSnippets.entries()) {
      if (p !== relPath) {
        recentFiles.push({ path: p, content });
      }
    }

    const model = config.get<string>('autocomplete.model', 'gemini-2.5-flash-lite');
    const { id, promise } = this.client.request<{ completion: string }>('inline_complete', {
      filePath: relPath,
      languageId: document.languageId,
      prefix,
      suffix,
      recentFiles: recentFiles.slice(-2),
      model,
    });
    this.activeRequestId = id;

    const cancelSub = token.onCancellationRequested(() => {
      void this.client.cancel(id);
    });

    try {
      const res = await promise;
      if (token.isCancellationRequested) {
        return [];
      }
      const completion = (res.completion ?? '').replace(/\r\n/g, '\n');
      if (!completion.trim()) {
        this.rememberCache(cacheKey, '');
        return [];
      }
      this.rememberCache(cacheKey, completion);
      return [new vscode.InlineCompletionItem(completion, new vscode.Range(position, position))];
    } catch {
      return [];
    } finally {
      cancelSub.dispose();
      if (this.activeRequestId === id) {
        this.activeRequestId = undefined;
      }
    }
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
    for (const d of this.disposables) {
      d.dispose();
    }
  }
}
