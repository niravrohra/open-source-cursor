import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const CONTEXT_LINES_AROUND = 80;

const addedHighlight = vscode.window.createTextEditorDecorationType({
  backgroundColor: 'rgba(46, 160, 67, 0.20)',
  isWholeLine: true,
});

export async function runInlineEdit(client: SidecarClient): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showWarningMessage('Open a file to run Rursor Inline Edit.');
    return;
  }

  const instruction = await vscode.window.showInputBox({
    title: 'Rursor Inline Edit (Cmd+K)',
    prompt: 'Describe how to edit the selected code (or insert at cursor)',
    placeHolder: 'e.g., Add error handling and return early on invalid input',
  });
  if (!instruction) {
    return;
  }

  const doc = editor.document;
  const selection = editor.selection.isEmpty
    ? new vscode.Selection(
        new vscode.Position(editor.selection.active.line, 0),
        doc.lineAt(editor.selection.active.line).range.end
      )
    : editor.selection;

  const prefixStartLine = Math.max(0, selection.start.line - CONTEXT_LINES_AROUND);
  const suffixEndLine = Math.min(doc.lineCount - 1, selection.end.line + CONTEXT_LINES_AROUND);

  const prefix = doc.getText(new vscode.Range(new vscode.Position(prefixStartLine, 0), selection.start));
  const selectedText = doc.getText(selection);
  const suffix = doc.getText(
    new vscode.Range(selection.end, doc.lineAt(suffixEndLine).range.end)
  );

  const relPath = vscode.workspace.asRelativePath(doc.uri, false);

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: 'Rursor: Generating inline edit with Gemini…',
      cancellable: true,
    },
    async (_progress, token) => {
      const { id, promise } = client.request<{ replacement: string }>('inline_edit', {
        filePath: relPath,
        languageId: doc.languageId,
        prefix,
        selection: selectedText,
        suffix,
        instruction,
      });

      token.onCancellationRequested(() => {
        void client.cancel(id);
      });

      const result = await promise;
      if (token.isCancellationRequested) {
        return;
      }

      const replacement = result.replacement;
      const applied = await editor.edit((editBuilder) => {
        editBuilder.replace(selection, replacement);
      });

      if (!applied) {
        vscode.window.showErrorMessage('Rursor could not apply the inline edit.');
        return;
      }

      const newEndLine = Math.min(
        doc.lineCount - 1,
        selection.start.line + replacement.split('\n').length - 1
      );
      const highlightRange = new vscode.Range(
        new vscode.Position(selection.start.line, 0),
        doc.lineAt(newEndLine).range.end
      );
      editor.setDecorations(addedHighlight, [highlightRange]);

      const choice = await vscode.window.showInformationMessage(
        'Rursor applied inline edit.',
        'Keep',
        'Revert'
      );
      editor.setDecorations(addedHighlight, []);

      if (choice === 'Revert') {
        await vscode.commands.executeCommand('undo');
      }
    }
  );
}
