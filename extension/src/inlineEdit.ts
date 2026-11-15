import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const CONTEXT_LINES_AROUND = 80;
const MAX_PROMPT_HISTORY = 8;

const addedHighlight = vscode.window.createTextEditorDecorationType({
  backgroundColor: 'rgba(46, 160, 67, 0.22)',
  borderWidth: '0 0 0 3px',
  borderStyle: 'solid',
  borderColor: 'rgba(46, 160, 67, 0.90)',
  isWholeLine: true,
});

const removedHighlight = vscode.window.createTextEditorDecorationType({
  backgroundColor: 'rgba(248, 81, 73, 0.22)',
  borderWidth: '0 0 0 3px',
  borderStyle: 'solid',
  borderColor: 'rgba(248, 81, 73, 0.90)',
  textDecoration: 'line-through',
  opacity: '0.82',
  isWholeLine: true,
});

export interface DiffLineEntry {
  kind: 'same' | 'removed' | 'added';
  text: string;
}

interface PendingInlineEdit {
  uri: vscode.Uri;
  diffRange: vscode.Range;
  originalText: string;
  replacementText: string;
  lastInstruction: string;
}

export class InlineEditSessionManager implements vscode.CodeLensProvider, vscode.Disposable {
  private readonly onDidChangeCodeLensesEmitter = new vscode.EventEmitter<void>();
  public readonly onDidChangeCodeLenses = this.onDidChangeCodeLensesEmitter.event;
  private session: PendingInlineEdit | undefined;
  private promptHistory: string[] = [];

  constructor(private readonly client: SidecarClient) {}

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    if (!this.session || this.session.uri.toString() !== document.uri.toString()) {
      return [];
    }
    const line = Math.max(0, Math.min(this.session.diffRange.start.line, document.lineCount - 1));
    const lensRange = new vscode.Range(line, 0, line, 0);
    return [
      new vscode.CodeLens(lensRange, {
        title: '$(check) Accept Edit (Cmd+Y)',
        command: 'rursor.acceptInlineEdit',
      }),
      new vscode.CodeLens(lensRange, {
        title: '$(discard) Revert Edit (Cmd+N)',
        command: 'rursor.revertInlineEdit',
      }),
      new vscode.CodeLens(lensRange, {
        title: '$(sync) Refine',
        command: 'rursor.refineInlineEdit',
      }),
    ];
  }

  async runInlineEdit(
    presetOrBridge?:
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
  ): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showWarningMessage('Open a file to run Rursor Inline Edit.');
      return;
    }

    if (this.session) {
      await this.acceptSession();
    }

    const doc = editor.document;
    let selection: vscode.Selection;
    let presetInstruction: string | undefined;

    if (typeof presetOrBridge === 'string') {
      presetInstruction = presetOrBridge;
      selection = editor.selection.isEmpty
        ? new vscode.Selection(
            new vscode.Position(editor.selection.active.line, 0),
            doc.lineAt(editor.selection.active.line).range.end
          )
        : editor.selection;
    } else if (presetOrBridge && typeof presetOrBridge === 'object') {
      presetInstruction = presetOrBridge.prompt;
      if (presetOrBridge.range) {
        const r = presetOrBridge.range;
        selection = new vscode.Selection(
          new vscode.Position(Math.max(0, r.startLineNumber - 1), Math.max(0, r.startColumn - 1)),
          new vscode.Position(Math.max(0, r.endLineNumber - 1), Math.max(0, r.endColumn - 1))
        );
      } else {
        selection = editor.selection.isEmpty
          ? new vscode.Selection(
              new vscode.Position(editor.selection.active.line, 0),
              doc.lineAt(editor.selection.active.line).range.end
            )
          : editor.selection;
      }
    } else {
      selection = editor.selection.isEmpty
        ? new vscode.Selection(
            new vscode.Position(editor.selection.active.line, 0),
            doc.lineAt(editor.selection.active.line).range.end
          )
        : editor.selection;
    }

    const matchingDiagnostics = collectDiagnosticsNearRange(doc.uri, selection);

    const instruction =
      presetInstruction ?? (await this.promptInstructionWithQuickPick(matchingDiagnostics.length));

    if (!instruction?.trim()) {
      return;
    }

    this.recordPrompt(instruction.trim());
    await this.executeEdit(editor, selection, instruction.trim(), matchingDiagnostics);
  }

  private async promptInstructionWithQuickPick(diagnosticCount: number): Promise<string | undefined> {
    const qp = vscode.window.createQuickPick<vscode.QuickPickItem>();
    qp.title = 'Rursor Inline Edit (Cmd+K)';
    qp.placeholder =
      diagnosticCount > 0
        ? `Type an instruction or pick an action (${diagnosticCount} diagnostic(s) attached)`
        : 'Type an instruction (or pick a preset / recent prompt)…';
    qp.matchOnDescription = true;

    const baseItems: vscode.QuickPickItem[] = [
      ...(diagnosticCount > 0
        ? [
            {
              label: '$(wrench) Fix the diagnostics in this code',
              description: `${diagnosticCount} active diagnostic(s)`,
            },
          ]
        : []),
      { label: '$(sparkle) Refactor and improve readability', description: 'Preset' },
      { label: '$(shield) Add robust error handling and edge-case checks', description: 'Preset' },
      { label: '$(beaker) Write unit tests for this code', description: 'Preset' },
      { label: '$(comment) Add concise docstrings / comments', description: 'Preset' },
      ...this.promptHistory.map((h) => ({ label: `$(history) ${h}`, description: 'Recent' })),
    ];

    qp.items = baseItems;
    qp.onDidChangeValue((val) => {
      const trimmed = val.trim();
      if (!trimmed) {
        qp.items = baseItems;
      } else {
        qp.items = [
          { label: `$(play) ${trimmed}`, description: 'Run custom instruction' },
          ...baseItems,
        ];
      }
    });

    return new Promise<string | undefined>((resolve) => {
      let resolved = false;
      qp.onDidAccept(() => {
        resolved = true;
        const selected = qp.selectedItems[0];
        const raw = selected
          ? selected.label.replace(/^\$\([a-z-]+\)\s*/, '')
          : qp.value.trim();
        qp.hide();
        resolve(raw || undefined);
      });
      qp.onDidHide(() => {
        qp.dispose();
        if (!resolved) {
          resolve(undefined);
        }
      });
      qp.show();
    });
  }

  private recordPrompt(prompt: string): void {
    this.promptHistory = [prompt, ...this.promptHistory.filter((p) => p !== prompt)].slice(
      0,
      MAX_PROMPT_HISTORY
    );
  }

  async refineSession(): Promise<void> {
    if (!this.session) {
      return;
    }
    const editor = vscode.window.visibleTextEditors.find(
      (e) => e.document.uri.toString() === this.session?.uri.toString()
    );
    if (!editor) {
      return;
    }
    const followUp = await vscode.window.showInputBox({
      title: 'Refine Rursor Inline Edit',
      prompt: 'Describe how to adjust the generated code',
      placeHolder: `Previous: ${this.session.lastInstruction}`,
    });
    if (!followUp?.trim()) {
      return;
    }

    const session = this.session;
    // Strip temporary removed-line diff preview back to clean replacement before refining.
    await editor.edit((builder) => {
      builder.replace(session.diffRange, session.replacementText);
    });

    const lines = session.replacementText.split('\n');
    const cleanEndLine = Math.min(
      editor.document.lineCount - 1,
      session.diffRange.start.line + lines.length - 1
    );
    const cleanEndChar =
      lines.length === 1
        ? session.diffRange.start.character + lines[0].length
        : lines[lines.length - 1].length;
    const cleanRange = new vscode.Range(
      session.diffRange.start,
      new vscode.Position(cleanEndLine, cleanEndChar)
    );

    const diagnostics = collectDiagnosticsNearRange(editor.document.uri, cleanRange);
    await this.executeEdit(
      editor,
      cleanRange,
      `${session.lastInstruction}\nFollow-up refinement: ${followUp.trim()}`,
      diagnostics,
      session.originalText
    );
  }

  async acceptSession(): Promise<void> {
    if (!this.session) {
      return;
    }
    const session = this.session;
    const editor = vscode.window.visibleTextEditors.find(
      (e) => e.document.uri.toString() === session.uri.toString()
    );

    if (editor) {
      editor.setDecorations(addedHighlight, []);
      editor.setDecorations(removedHighlight, []);
      await editor.edit((builder) => {
        builder.replace(session.diffRange, session.replacementText);
      });
    } else {
      const wsEdit = new vscode.WorkspaceEdit();
      wsEdit.replace(session.uri, session.diffRange, session.replacementText);
      await vscode.workspace.applyEdit(wsEdit);
    }

    this.session = undefined;
    await vscode.commands.executeCommand('setContext', 'rursor.hasActiveInlineEdit', false);
    this.onDidChangeCodeLensesEmitter.fire();
    void Promise.resolve(vscode.commands.executeCommand('rursor.inlineEdit.hideWidget')).catch(() => undefined);
  }

  async revertSession(): Promise<void> {
    if (!this.session) {
      return;
    }
    const session = this.session;
    const editor = vscode.window.visibleTextEditors.find(
      (e) => e.document.uri.toString() === session.uri.toString()
    );

    if (editor) {
      editor.setDecorations(addedHighlight, []);
      editor.setDecorations(removedHighlight, []);
      await editor.edit((builder) => {
        builder.replace(session.diffRange, session.originalText);
      });
    } else {
      const wsEdit = new vscode.WorkspaceEdit();
      wsEdit.replace(session.uri, session.diffRange, session.originalText);
      await vscode.workspace.applyEdit(wsEdit);
    }

    this.session = undefined;
    await vscode.commands.executeCommand('setContext', 'rursor.hasActiveInlineEdit', false);
    this.onDidChangeCodeLensesEmitter.fire();
    void Promise.resolve(vscode.commands.executeCommand('rursor.inlineEdit.hideWidget')).catch(() => undefined);
  }

  private async executeEdit(
    editor: vscode.TextEditor,
    targetRange: vscode.Range,
    instruction: string,
    diagnostics: Array<{ path: string; line: number; severity: string; message: string }>,
    preservedOriginalText?: string
  ): Promise<void> {
    const doc = editor.document;
    const prefixStartLine = Math.max(0, targetRange.start.line - CONTEXT_LINES_AROUND);
    const suffixEndLine = Math.min(doc.lineCount - 1, targetRange.end.line + CONTEXT_LINES_AROUND);

    const prefix = doc.getText(new vscode.Range(new vscode.Position(prefixStartLine, 0), targetRange.start));
    const selectedText = doc.getText(targetRange);
    const suffix = doc.getText(
      new vscode.Range(targetRange.end, doc.lineAt(suffixEndLine).range.end)
    );
    const relPath = vscode.workspace.asRelativePath(doc.uri, false);

    await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: 'Rursor: Generating inline edit with Gemini…',
        cancellable: true,
      },
      async (_progress, token) => {
        let streamedSoFar = '';
        const { id, promise } = this.client.request<{ replacement: string }>(
          'inline_edit',
          {
            filePath: relPath,
            languageId: doc.languageId,
            prefix,
            selection: selectedText,
            suffix,
            instruction,
            diagnostics,
          },
          (delta) => {
            streamedSoFar += delta;
            void Promise.resolve(
              vscode.commands.executeCommand('rursor.inlineEdit.updateDiffPreview', {
                startLineNumber: targetRange.start.line + 1,
                streaming: true,
                statusText: 'Streaming Rursor edit…',
                hunks: [
                  {
                    kind: 'added',
                    startLineNumber: targetRange.start.line + 1,
                    endLineNumber: targetRange.end.line + 1,
                    text: streamedSoFar,
                  },
                ],
              })
            ).catch(() => undefined);
          }
        );

        token.onCancellationRequested(() => {
          void this.client.cancel(id);
        });

        try {
          const result = await promise;
          if (token.isCancellationRequested) {
            return;
          }

          const originalForDiff = preservedOriginalText ?? selectedText;
          const replacement = result.replacement;

          // Build interleaved red/green diff preview when inline edit spans whole lines.
          const isWholeLineEdit =
            targetRange.start.character === 0 &&
            (targetRange.end.character === doc.lineAt(targetRange.end.line).text.length ||
              selectedText.includes('\n'));

          const diffEntries = isWholeLineEdit
            ? computeLineDiff(originalForDiff.split('\n'), replacement.split('\n'))
            : [{ kind: 'added' as const, text: replacement }];

          const previewText = isWholeLineEdit
            ? diffEntries.map((e) => e.text).join('\n')
            : replacement;

          const applied = await editor.edit((editBuilder) => {
            editBuilder.replace(targetRange, previewText);
          });

          if (!applied) {
            vscode.window.showErrorMessage('Rursor could not apply the inline edit.');
            return;
          }

          const previewLines = previewText.split('\n');
          const endLine = Math.min(
            doc.lineCount - 1,
            targetRange.start.line + previewLines.length - 1
          );
          const endChar =
            previewLines.length === 1
              ? targetRange.start.character + previewLines[0].length
              : previewLines[previewLines.length - 1].length;
          const diffRange = new vscode.Range(
            targetRange.start,
            new vscode.Position(endLine, endChar)
          );

          const addedRanges: vscode.Range[] = [];
          const removedRanges: vscode.Range[] = [];

          if (isWholeLineEdit) {
            diffEntries.forEach((entry, offset) => {
              const lineIdx = Math.min(doc.lineCount - 1, targetRange.start.line + offset);
              const lineRange = doc.lineAt(lineIdx).range;
              if (entry.kind === 'added') {
                addedRanges.push(lineRange);
              } else if (entry.kind === 'removed') {
                removedRanges.push(lineRange);
              }
            });
          } else {
            addedRanges.push(diffRange);
          }

          editor.setDecorations(addedHighlight, addedRanges);
          editor.setDecorations(removedHighlight, removedRanges);

          this.session = {
            uri: doc.uri,
            diffRange,
            originalText: originalForDiff,
            replacementText: replacement,
            lastInstruction: instruction,
          };

          await vscode.commands.executeCommand('setContext', 'rursor.hasActiveInlineEdit', true);
          this.onDidChangeCodeLensesEmitter.fire();
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          vscode.window.showErrorMessage(`Rursor Inline Edit failed: ${msg}`);
        }
      }
    );
  }

  dispose(): void {
    this.onDidChangeCodeLensesEmitter.dispose();
  }
}

export class RursorCodeActionProvider implements vscode.CodeActionProvider {
  provideCodeActions(
    _document: vscode.TextDocument,
    _range: vscode.Range | vscode.Selection,
    context: vscode.CodeActionContext
  ): vscode.CodeAction[] {
    if (context.diagnostics.length === 0) {
      return [];
    }
    const action = new vscode.CodeAction(
      'Fix with Rursor AI (Cmd+K)',
      vscode.CodeActionKind.QuickFix
    );
    action.command = {
      command: 'rursor.fixDiagnostics',
      title: 'Fix with Rursor AI',
    };
    action.diagnostics = [...context.diagnostics];
    return [action];
  }
}

/** LCS line diff producing interleaved removed/added hunks for inline editor preview. */
export function computeLineDiff(before: string[], after: string[]): DiffLineEntry[] {
  const n = before.length;
  const m = after.length;
  if (n * m > 40_000) {
    return [
      ...before.map((text) => ({ kind: 'removed' as const, text })),
      ...after.map((text) => ({ kind: 'added' as const, text })),
    ];
  }

  const dp: number[][] = Array.from({ length: n + 1 }, () => Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      if (before[i] === after[j]) {
        dp[i][j] = 1 + dp[i + 1][j + 1];
      } else {
        dp[i][j] = Math.max(dp[i + 1][j], dp[i][j + 1]);
      }
    }
  }

  const out: DiffLineEntry[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (before[i] === after[j]) {
      out.push({ kind: 'same', text: before[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      out.push({ kind: 'removed', text: before[i] });
      i++;
    } else {
      out.push({ kind: 'added', text: after[j] });
      j++;
    }
  }
  while (i < n) {
    out.push({ kind: 'removed', text: before[i++] });
  }
  while (j < m) {
    out.push({ kind: 'added', text: after[j++] });
  }
  return out;
}

function collectDiagnosticsNearRange(
  uri: vscode.Uri,
  range: vscode.Range
): Array<{ path: string; line: number; severity: string; message: string }> {
  const relPath = vscode.workspace.asRelativePath(uri, false);
  const all = vscode.languages.getDiagnostics(uri);
  const near = all.filter(
    (d) =>
      d.range.end.line >= Math.max(0, range.start.line - 3) &&
      d.range.start.line <= range.end.line + 3
  );
  const chosen = near.length > 0 ? near : all.slice(0, 10);
  return chosen.map((d) => ({
    path: relPath,
    line: d.range.start.line + 1,
    severity:
      d.severity === vscode.DiagnosticSeverity.Error
        ? 'error'
        : d.severity === vscode.DiagnosticSeverity.Warning
          ? 'warning'
          : 'info',
    message: d.message,
  }));
}
