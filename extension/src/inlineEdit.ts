import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const CONTEXT_LINES_AROUND = 80;

const addedHighlight = vscode.window.createTextEditorDecorationType({
  backgroundColor: 'rgba(46, 160, 67, 0.20)',
  borderWidth: '0 0 0 3px',
  borderStyle: 'solid',
  borderColor: 'rgba(46, 160, 67, 0.85)',
  isWholeLine: true,
});

interface PendingInlineEdit {
  uri: vscode.Uri;
  range: vscode.Range;
  originalText: string;
  lastInstruction: string;
}

export class InlineEditSessionManager implements vscode.CodeLensProvider, vscode.Disposable {
  private readonly onDidChangeCodeLensesEmitter = new vscode.EventEmitter<void>();
  public readonly onDidChangeCodeLenses = this.onDidChangeCodeLensesEmitter.event;
  private session: PendingInlineEdit | undefined;

  constructor(private readonly client: SidecarClient) {}

  provideCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    if (!this.session || this.session.uri.toString() !== document.uri.toString()) {
      return [];
    }
    const line = Math.max(0, Math.min(this.session.range.start.line, document.lineCount - 1));
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

  async runInlineEdit(presetInstruction?: string): Promise<void> {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showWarningMessage('Open a file to run Rursor Inline Edit.');
      return;
    }

    // Clear any prior unaccepted session decoration before starting a new edit.
    if (this.session) {
      await this.acceptSession();
    }

    const doc = editor.document;
    const selection = editor.selection.isEmpty
      ? new vscode.Selection(
          new vscode.Position(editor.selection.active.line, 0),
          doc.lineAt(editor.selection.active.line).range.end
        )
      : editor.selection;

    const matchingDiagnostics = collectDiagnosticsNearRange(doc.uri, selection);

    const instruction =
      presetInstruction ??
      (await vscode.window.showInputBox({
        title: 'Rursor Inline Edit (Cmd+K)',
        prompt:
          matchingDiagnostics.length > 0
            ? `Describe edit (${matchingDiagnostics.length} diagnostic(s) attached automatically)`
            : 'Describe how to edit the selected code (or generate at cursor)',
        value: matchingDiagnostics.length > 0 ? 'Fix the diagnostics in this code' : '',
        placeHolder: 'e.g., Add error handling and return early on invalid input',
      }));

    if (!instruction?.trim()) {
      return;
    }

    await this.executeEdit(editor, selection, instruction.trim(), matchingDiagnostics);
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

    const currentRange = this.session.range;
    const originalBeforeAll = this.session.originalText;
    const diagnostics = collectDiagnosticsNearRange(editor.document.uri, currentRange);

    await this.executeEdit(
      editor,
      currentRange,
      `${this.session.lastInstruction}\nFollow-up refinement: ${followUp.trim()}`,
      diagnostics,
      originalBeforeAll
    );
  }

  async acceptSession(): Promise<void> {
    if (!this.session) {
      return;
    }
    const targetUri = this.session.uri.toString();
    for (const editor of vscode.window.visibleTextEditors) {
      if (editor.document.uri.toString() === targetUri) {
        editor.setDecorations(addedHighlight, []);
      }
    }
    this.session = undefined;
    await vscode.commands.executeCommand('setContext', 'rursor.hasActiveInlineEdit', false);
    this.onDidChangeCodeLensesEmitter.fire();
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
      await editor.edit((editBuilder) => {
        editBuilder.replace(session.range, session.originalText);
      });
    } else {
      const wsEdit = new vscode.WorkspaceEdit();
      wsEdit.replace(session.uri, session.range, session.originalText);
      await vscode.workspace.applyEdit(wsEdit);
    }

    this.session = undefined;
    await vscode.commands.executeCommand('setContext', 'rursor.hasActiveInlineEdit', false);
    this.onDidChangeCodeLensesEmitter.fire();
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
        const { id, promise } = this.client.request<{ replacement: string }>('inline_edit', {
          filePath: relPath,
          languageId: doc.languageId,
          prefix,
          selection: selectedText,
          suffix,
          instruction,
          diagnostics,
        });

        token.onCancellationRequested(() => {
          void this.client.cancel(id);
        });

        try {
          const result = await promise;
          if (token.isCancellationRequested) {
            return;
          }

          const replacement = result.replacement;
          const applied = await editor.edit((editBuilder) => {
            editBuilder.replace(targetRange, replacement);
          });

          if (!applied) {
            vscode.window.showErrorMessage('Rursor could not apply the inline edit.');
            return;
          }

          const lines = replacement.split('\n');
          const endLine = Math.min(doc.lineCount - 1, targetRange.start.line + lines.length - 1);
          const endChar =
            lines.length === 1
              ? targetRange.start.character + lines[0].length
              : lines[lines.length - 1].length;
          const newRange = new vscode.Range(targetRange.start, new vscode.Position(endLine, endChar));
          const highlightRange = new vscode.Range(
            new vscode.Position(targetRange.start.line, 0),
            doc.lineAt(endLine).range.end
          );

          editor.setDecorations(addedHighlight, [highlightRange]);
          this.session = {
            uri: doc.uri,
            range: newRange,
            originalText: preservedOriginalText ?? selectedText,
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
