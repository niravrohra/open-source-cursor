import * as crypto from 'node:crypto';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import * as vscode from 'vscode';
import { SidecarClient } from './sidecarClient';

const execFileAsync = promisify(execFile);

export const RURSOR_BASELINE_SCHEME = 'rursor-baseline';

const MAX_RAW_TERMINAL_BUFFER = 16_000;
const MAX_TERMINAL_OUTPUT_CHARS = 4_500;
const MAX_TERMINAL_OUTPUT_LINES = 80;
const STREAM_DRAIN_TIMEOUT_MS = 450;
const RENAME_CONTEXT_LINES = 35;

/**
 * Hooks connecting native fork UI providers to the Rursor Agent panel and Inline Edit session manager.
 */
export interface NativeForkHooks {
  onTriggerAgentPrompt: (prompt: string) => Promise<void>;
  onRunInlineEditWithInstruction: (instruction: string) => Promise<void>;
}

/**
 * Typed interfaces for VS Code Proposed APIs unlocked in `fork/product.json` (`Rursor.app`),
 * keeping compilation 100% compatible with standard `@types/vscode` (`^1.95.0`) and `strict: true`
 * with zero `any` types.
 */
export interface ProposedWebviewEditorInset extends vscode.Disposable {
  readonly editor: vscode.TextEditor;
  readonly line: number;
  readonly height: number;
  readonly webview: vscode.Webview;
  readonly onDidDispose: vscode.Event<void>;
}

export interface ProposedQuickDiffProvider extends vscode.QuickDiffProvider {
  label?: string;
  readonly visible?: boolean;
}

export interface ProposedTerminalCommandMatchResult {
  commandLine: string;
  commandLineMatch: RegExpMatchArray;
  outputMatch?: {
    regexMatch: RegExpMatchArray;
    outputLines: string[];
  };
}

export interface ProposedTerminalQuickFixTerminalCommand {
  terminalCommand: string;
  shouldExecute?: boolean;
}

export interface ProposedTerminalQuickFixOpener {
  uri: vscode.Uri;
}

export type ProposedTerminalQuickFixItem =
  | ProposedTerminalQuickFixTerminalCommand
  | ProposedTerminalQuickFixOpener
  | vscode.Command;

export interface ProposedTerminalQuickFixProvider {
  provideTerminalQuickFixes(
    commandMatchResult: ProposedTerminalCommandMatchResult,
    token: vscode.CancellationToken
  ): vscode.ProviderResult<ProposedTerminalQuickFixItem[] | ProposedTerminalQuickFixItem>;
}

export const enum ProposedNewSymbolNameTag {
  AIGenerated = 1,
}

export const enum ProposedNewSymbolNameTriggerKind {
  Invoke = 0,
  Automatic = 1,
}

export interface ProposedNewSymbolName {
  readonly newSymbolName: string;
  readonly tags?: readonly number[];
}

export interface ProposedNewSymbolNameConstructor {
  new (newSymbolName: string, tags?: readonly number[]): ProposedNewSymbolName;
}

export interface ProposedNewSymbolNamesProvider {
  readonly supportsAutomaticTriggerKind?: Thenable<boolean>;
  provideNewSymbolNames(
    document: vscode.TextDocument,
    range: vscode.Range,
    triggerKind: number,
    token: vscode.CancellationToken
  ): vscode.ProviderResult<ProposedNewSymbolName[]>;
}

export const enum ProposedInlineEditTriggerKind {
  Invoke = 0,
  Automatic = 1,
}

export interface ProposedInlineEdit {
  readonly text: string;
  readonly range: vscode.Range;
  accepted?: vscode.Command;
  rejected?: vscode.Command;
  shown?: vscode.Command;
  commands?: vscode.Command[];
}

export interface ProposedInlineEditConstructor {
  new (text: string, range: vscode.Range): ProposedInlineEdit;
}

export interface ProposedInlineEditContext {
  triggerKind: number;
}

export interface ProposedInlineEditProvider {
  provideInlineEdit(
    document: vscode.TextDocument,
    context: ProposedInlineEditContext,
    token: vscode.CancellationToken
  ): vscode.ProviderResult<ProposedInlineEdit>;
}

export interface ProposedLineChange {
  readonly originalStartLineNumber: number;
  readonly originalEndLineNumber: number;
  readonly modifiedStartLineNumber: number;
  readonly modifiedEndLineNumber: number;
}

export interface ProposedVsCodeApi {
  InlineEdit?: ProposedInlineEditConstructor;
  NewSymbolName?: ProposedNewSymbolNameConstructor;
  window?: {
    createWebviewTextEditorInset?: (
      editor: vscode.TextEditor,
      line: number,
      height: number,
      options?: vscode.WebviewOptions
    ) => ProposedWebviewEditorInset;
    registerQuickDiffProvider?: (
      selector: vscode.DocumentSelector,
      quickDiffProvider: ProposedQuickDiffProvider,
      label: string,
      rootUri?: vscode.Uri
    ) => vscode.Disposable;
    registerTerminalQuickFixProvider?: (
      id: string,
      provider: ProposedTerminalQuickFixProvider
    ) => vscode.Disposable;
    onDidStartTerminalShellExecution?: vscode.Event<vscode.TerminalShellExecutionStartEvent>;
    onDidEndTerminalShellExecution?: vscode.Event<vscode.TerminalShellExecutionEndEvent>;
  };
  languages?: {
    registerNewSymbolNamesProvider?: (
      selector: vscode.DocumentSelector,
      provider: ProposedNewSymbolNamesProvider
    ) => vscode.Disposable;
    registerInlineEditProvider?: (
      selector: vscode.DocumentSelector,
      provider: ProposedInlineEditProvider
    ) => vscode.Disposable;
  };
  commands?: {
    registerDiffInformationCommand?: (
      command: string,
      callback: (diff: ProposedLineChange[], ...args: unknown[]) => unknown
    ) => vscode.Disposable;
  };
}

export interface BaselineFileRecord {
  key: string;
  uri: vscode.Uri;
  baselineUri: vscode.Uri;
  baselineContent: string;
  latestContent: string;
  modified: boolean;
  updatedAt: number;
}

export interface TerminalFailureRecord {
  commandLine: string;
  exitCode: number;
  outputTail: string;
  cwd?: string;
  terminalName?: string;
  timestamp: number;
}

export interface ReviewFinding {
  file: string;
  line: number;
  severity: 'critical' | 'warning' | 'info';
  comment: string;
  suggestedFix?: string;
}

interface InsetWebviewMessage {
  type: 'submit' | 'cancel';
  instruction?: string;
}

interface DapSource {
  name?: string;
  path?: string;
}

interface DapStackFrame {
  id: number;
  name: string;
  source?: DapSource;
  line: number;
  column: number;
  presentationHint?: string;
}

interface DapStackTraceResponse {
  stackFrames?: DapStackFrame[];
  totalFrames?: number;
}

interface DapThreadsResponse {
  threads?: Array<{ id: number; name?: string }>;
}

interface DapScope {
  name: string;
  presentationHint?: string;
  variablesReference: number;
  expensive?: boolean;
}

interface DapScopesResponse {
  scopes?: DapScope[];
}

interface DapVariable {
  name: string;
  value: string;
  type?: string;
  variablesReference?: number;
}

interface DapVariablesResponse {
  variables?: DapVariable[];
}

interface DapExceptionInfoResponse {
  exceptionId?: string;
  description?: string;
  breakMode?: string;
  details?: {
    message?: string;
    typeName?: string;
    stackTrace?: string;
  };
}

interface NesPredictInlineResponse {
  prediction?: {
    kind?: string;
    targetPath?: string;
    startLine?: number;
    endLine?: number;
    replacement?: string;
  } | null;
}

/**
 * Tracks pre-AI baselines across the editing session to power:
 * 1. `rursor-baseline:` virtual document snapshots
 * 2. Native `"Rursor AI Edits"` Quick-Diff gutter bars (`window.registerQuickDiffProvider`)
 * 3. Native unified multi-file diff review (`vscode.changes` via `rursor.openMultiDiff`)
 */
export class RursorSessionBaselineTracker
  implements vscode.TextDocumentContentProvider, ProposedQuickDiffProvider, vscode.Disposable
{
  public readonly label = 'Rursor AI Edits';
  public readonly visible = true;

  private readonly entries = new Map<string, BaselineFileRecord>();
  private readonly baselineUriToKey = new Map<string, string>();
  private readonly onDidChangeEmitter = new vscode.EventEmitter<vscode.Uri>();
  public readonly onDidChange = this.onDidChangeEmitter.event;
  private readonly statusBarItem: vscode.StatusBarItem;
  private readonly docSubs: vscode.Disposable[] = [];

  constructor() {
    this.statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 97);
    this.statusBarItem.command = 'rursor.openMultiDiff';

    this.docSubs.push(
      vscode.workspace.onDidChangeTextDocument((event) => {
        if (event.document.uri.scheme !== 'file') {
          return;
        }
        const key = this.normalizeKey(event.document.uri);
        const existing = this.entries.get(key);
        if (!existing) {
          return;
        }
        const liveText = event.document.getText();
        const isNowModified = liveText !== existing.baselineContent;
        existing.latestContent = liveText;
        if (existing.modified !== isNowModified) {
          existing.modified = isNowModified;
          this.onDidChangeEmitter.fire(existing.baselineUri);
          this.refreshStatusBar();
        }
      })
    );
  }

  private toFileUri(uriOrPath: vscode.Uri | string): vscode.Uri {
    return typeof uriOrPath === 'string' ? vscode.Uri.file(uriOrPath) : uriOrPath;
  }

  private normalizeKey(uri: vscode.Uri): string {
    return uri.scheme === 'file' ? path.resolve(uri.fsPath) : uri.toString();
  }

  public toBaselineUri(uriOrPath: vscode.Uri | string): vscode.Uri {
    const fileUri = this.toFileUri(uriOrPath);
    const key = this.normalizeKey(fileUri);
    const baselineUri = fileUri.with({
      scheme: RURSOR_BASELINE_SCHEME,
      query: `key=${encodeURIComponent(key)}`,
    });
    this.baselineUriToKey.set(baselineUri.toString(), key);
    return baselineUri;
  }

  /**
   * Captures the original baseline content of `uri` prior to the first AI edit in this session.
   * Subsequent AI edits on the same file preserve the initial session baseline until cleared/reverted.
   */
  public recordBaselineBeforeEdit(uriOrPath: vscode.Uri | string, originalContent: string): void {
    const fileUri = this.toFileUri(uriOrPath);
    const key = this.normalizeKey(fileUri);
    const existing = this.entries.get(key);
    if (existing && existing.modified) {
      return;
    }

    const baselineUri = this.toBaselineUri(fileUri);
    this.entries.set(key, {
      key,
      uri: fileUri,
      baselineUri,
      baselineContent: originalContent,
      latestContent: originalContent,
      modified: false,
      updatedAt: Date.now(),
    });
    this.onDidChangeEmitter.fire(baselineUri);
    this.refreshStatusBar();
  }

  /**
   * Marks `uri` as modified by Rursor AI (or clears its modified flag if `afterContent` matches baseline).
   */
  public markFileModified(uriOrPath: vscode.Uri | string, afterContent?: string): void {
    const fileUri = this.toFileUri(uriOrPath);
    const key = this.normalizeKey(fileUri);
    let record = this.entries.get(key);

    if (!record) {
      const baselineUri = this.toBaselineUri(fileUri);
      record = {
        key,
        uri: fileUri,
        baselineUri,
        baselineContent: '',
        latestContent: afterContent ?? '',
        modified: true,
        updatedAt: Date.now(),
      };
      this.entries.set(key, record);
    } else {
      const resolvedAfter =
        afterContent ??
        vscode.workspace.textDocuments
          .find((d) => this.normalizeKey(d.uri) === key)
          ?.getText();

      if (resolvedAfter !== undefined) {
        record.latestContent = resolvedAfter;
        record.modified = resolvedAfter !== record.baselineContent;
      } else {
        record.modified = true;
      }
      record.updatedAt = Date.now();
    }

    this.onDidChangeEmitter.fire(record.baselineUri);
    this.refreshStatusBar();
  }

  public provideTextDocumentContent(uri: vscode.Uri): string {
    const mappedKey = this.baselineUriToKey.get(uri.toString());
    if (mappedKey !== undefined) {
      return this.entries.get(mappedKey)?.baselineContent ?? '';
    }

    const params = new URLSearchParams(uri.query);
    const queryKey = params.get('key');
    if (queryKey) {
      return this.entries.get(queryKey)?.baselineContent ?? '';
    }

    const fallbackKey = path.resolve(uri.fsPath);
    return this.entries.get(fallbackKey)?.baselineContent ?? '';
  }

  public provideOriginalResource(
    uri: vscode.Uri,
    _token: vscode.CancellationToken
  ): vscode.ProviderResult<vscode.Uri> {
    if (uri.scheme !== 'file') {
      return undefined;
    }
    const key = this.normalizeKey(uri);
    const record = this.entries.get(key);
    if (!record || !record.modified) {
      return undefined;
    }
    return record.baselineUri;
  }

  public getChangedFiles(): BaselineFileRecord[] {
    return Array.from(this.entries.values())
      .filter((entry) => entry.modified)
      .sort((a, b) => b.updatedAt - a.updatedAt);
  }

  public getChangedCount(): number {
    return this.getChangedFiles().length;
  }

  public hasChanges(): boolean {
    return this.getChangedCount() > 0;
  }

  /**
   * Builds `[labelUri, originalBaselineUri, modifiedFileUri]` triples for `vscode.changes`.
   */
  public getResourceTriples(): Array<[vscode.Uri, vscode.Uri, vscode.Uri]> {
    return this.getChangedFiles().map((entry) => [entry.uri, entry.baselineUri, entry.uri]);
  }

  public clearBaselines(): void {
    const previousUris = Array.from(this.entries.values()).map((e) => e.baselineUri);
    this.entries.clear();
    this.baselineUriToKey.clear();
    for (const uri of previousUris) {
      this.onDidChangeEmitter.fire(uri);
    }
    this.refreshStatusBar();
  }

  public async openMultiDiff(): Promise<void> {
    const changed = this.getChangedFiles();
    if (changed.length === 0) {
      vscode.window.showInformationMessage(
        'Rursor: No AI-modified files recorded in this session yet.'
      );
      return;
    }

    const resourceTriples = this.getResourceTriples();
    try {
      await vscode.commands.executeCommand('vscode.changes', 'Rursor AI Changes', resourceTriples);
      return;
    } catch {
      // Fall back cleanly to QuickPick per-file diff selector if `vscode.changes` is unavailable or fails.
    }

    const items = changed.map((entry) => {
      const relPath = vscode.workspace.asRelativePath(entry.uri, false);
      const beforeLines = countLines(entry.baselineContent);
      const afterLines = countLines(entry.latestContent);
      const delta = afterLines - beforeLines;
      const deltaLabel = delta >= 0 ? `+${delta}` : `${delta}`;
      return {
        label: `$(diff) ${relPath}`,
        description: `${deltaLabel} line(s) vs session baseline`,
        entry,
        relPath,
      };
    });

    const picked = await vscode.window.showQuickPick(items, {
      title: 'Rursor AI Changes — Select a File to Diff',
      placeHolder: `${changed.length} AI-modified file(s) in current session`,
      matchOnDescription: true,
    });

    if (picked) {
      await vscode.commands.executeCommand(
        'vscode.diff',
        picked.entry.baselineUri,
        picked.entry.uri,
        `${picked.relPath} (Rursor Baseline ↔ Current)`
      );
    }
  }

  private refreshStatusBar(): void {
    const count = this.getChangedCount();
    void vscode.commands.executeCommand('setContext', 'rursor.hasSessionChanges', count > 0);
    if (count === 0) {
      this.statusBarItem.hide();
      return;
    }
    this.statusBarItem.text = `$(diff-multiple) ${count} AI ${count === 1 ? 'File' : 'Files'}`;
    this.statusBarItem.tooltip =
      'Click to review all Rursor AI modified files in the Unified Multi-Diff Editor';
    this.statusBarItem.show();
  }

  public dispose(): void {
    for (const sub of this.docSubs) {
      sub.dispose();
    }
    this.docSubs.length = 0;
    this.statusBarItem.dispose();
    this.onDidChangeEmitter.dispose();
    this.entries.clear();
    this.baselineUriToKey.clear();
  }
}

/**
 * Registers Rursor's unlocked fork native UI & workflow providers, degrading gracefully
 * when running inside vanilla VS Code (`./scripts/dev-vscode.sh`).
 */
export function registerNativeForkFeatures(
  context: vscode.ExtensionContext,
  client: SidecarClient,
  hooks: NativeForkHooks
): RursorSessionBaselineTracker {
  const vsAny = vscode as unknown as ProposedVsCodeApi;

  // ---------------------------------------------------------------------------
  // 1. AI Quick-Diff Gutter & Unified Multi-File Diff (`vscode.changes`)
  // ---------------------------------------------------------------------------
  const baselineTracker = new RursorSessionBaselineTracker();
  context.subscriptions.push(
    baselineTracker,
    vscode.workspace.registerTextDocumentContentProvider(RURSOR_BASELINE_SCHEME, baselineTracker),
    vscode.commands.registerCommand('rursor.openMultiDiff', async () => {
      await baselineTracker.openMultiDiff();
    })
  );

  if (typeof vsAny.window?.registerQuickDiffProvider === 'function') {
    try {
      const workspaceRootUri = vscode.workspace.workspaceFolders?.[0]?.uri;
      const quickDiffSub = vsAny.window.registerQuickDiffProvider(
        { scheme: 'file' },
        baselineTracker,
        'Rursor AI Edits',
        workspaceRootUri
      );
      context.subscriptions.push(quickDiffSub);
    } catch {
      // `quickDiffProvider` proposal not unlocked in vanilla VS Code; skip silently.
    }
  }

  // ---------------------------------------------------------------------------
  // 2. Floating Editor Inset `Cmd+K` Composer (`rursor.inlineEditInset`)
  // ---------------------------------------------------------------------------
  let activeInset: ProposedWebviewEditorInset | undefined;

  const disposeActiveInset = (): void => {
    if (activeInset) {
      const prev = activeInset;
      activeInset = undefined;
      try {
        prev.dispose();
      } catch {
        // Ignore already-disposed inset.
      }
    }
  };

  context.subscriptions.push(
    new vscode.Disposable(() => {
      disposeActiveInset();
    }),
    vscode.commands.registerCommand(
      'rursor.inlineEditInset',
      async (presetInstruction?: string) => {
        const editor = vscode.window.activeTextEditor;
        if (!editor) {
          vscode.window.showWarningMessage('Open a file to run Rursor Inline Edit.');
          return;
        }

        const runTrackedInlineEdit = async (
          targetEditor: vscode.TextEditor,
          instruction: string
        ): Promise<void> => {
          const doc = targetEditor.document;
          if (doc.uri.scheme === 'file') {
            baselineTracker.recordBaselineBeforeEdit(doc.uri, doc.getText());
          }
          await hooks.onRunInlineEditWithInstruction(instruction);
          if (doc.uri.scheme === 'file') {
            baselineTracker.markFileModified(doc.uri, doc.getText());
          }
        };

        if (typeof presetInstruction === 'string' && presetInstruction.trim().length > 0) {
          disposeActiveInset();
          await runTrackedInlineEdit(editor, presetInstruction.trim());
          return;
        }

        disposeActiveInset();

        const selection = editor.selection;
        const targetRange = selection.isEmpty
          ? editor.document.lineAt(selection.active.line).range
          : selection;
        const diagCount = countDiagnosticsNearRange(editor.document.uri, targetRange);
        const lineLabel =
          targetRange.start.line === targetRange.end.line
            ? `L${targetRange.start.line + 1}`
            : `L${targetRange.start.line + 1}–${targetRange.end.line + 1}`;
        const relPath = vscode.workspace.asRelativePath(editor.document.uri, false);

        if (typeof vsAny.window?.createWebviewTextEditorInset === 'function') {
          try {
            const anchorLine = Math.max(0, targetRange.start.line - 1);
            editor.revealRange(targetRange, vscode.TextEditorRevealType.InCenterIfOutsideViewport);

            const inset = vsAny.window.createWebviewTextEditorInset(editor, anchorLine, 4, {
              enableScripts: true,
            });
            activeInset = inset;
            inset.webview.options = { enableScripts: true };
            inset.webview.html = renderInlineEditInsetHtml(
              inset.webview.cspSource,
              relPath,
              lineLabel,
              diagCount
            );

            const msgSub = inset.webview.onDidReceiveMessage(
              async (rawMsg: unknown): Promise<void> => {
                if (!rawMsg || typeof rawMsg !== 'object') {
                  return;
                }
                const msg = rawMsg as InsetWebviewMessage;
                if (msg.type === 'cancel') {
                  disposeActiveInset();
                  await vscode.window.showTextDocument(editor.document, {
                    viewColumn: editor.viewColumn,
                    preserveFocus: false,
                    selection,
                  });
                  return;
                }
                if (msg.type === 'submit') {
                  const instruction = (msg.instruction ?? '').trim();
                  if (!instruction) {
                    return;
                  }
                  disposeActiveInset();
                  const restoredEditor = await vscode.window.showTextDocument(editor.document, {
                    viewColumn: editor.viewColumn,
                    preserveFocus: false,
                    selection,
                  });
                  await runTrackedInlineEdit(restoredEditor, instruction);
                }
              }
            );

            inset.onDidDispose(() => {
              msgSub.dispose();
              if (activeInset === inset) {
                activeInset = undefined;
              }
            });

            return;
          } catch {
            // `editorInsets` proposal not unlocked or editor cannot host an inset; fall back cleanly.
            disposeActiveInset();
          }
        }

        const fallbackInstruction = await promptFallbackInlineEditInstruction(diagCount);
        if (fallbackInstruction) {
          await runTrackedInlineEdit(editor, fallbackInstruction);
        }
      }
    )
  );

  // ---------------------------------------------------------------------------
  // 3. Proactive Terminal Failure Watcher & Quick Fix (`rursor.fixLastTerminalFailure`)
  // ---------------------------------------------------------------------------
  const executionStreams = new WeakMap<vscode.TerminalShellExecution, Promise<string>>();
  let lastTerminalFailure: TerminalFailureRecord | undefined;

  const terminalFailureStatus = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Right,
    101
  );
  terminalFailureStatus.text = '$(error) Fix Cmd Failure';
  terminalFailureStatus.command = 'rursor.fixLastTerminalFailure';
  terminalFailureStatus.backgroundColor = new vscode.ThemeColor('statusBarItem.errorBackground');
  context.subscriptions.push(terminalFailureStatus);

  const startExecEvent = vsAny.window?.onDidStartTerminalShellExecution;
  if (typeof startExecEvent === 'function') {
    try {
      context.subscriptions.push(
        startExecEvent((event) => {
          executionStreams.set(event.execution, collectTerminalStreamTail(event.execution));
        })
      );
    } catch {
      // Ignore if shell execution start event is unavailable in host version.
    }
  }

  const endExecEvent = vsAny.window?.onDidEndTerminalShellExecution;
  if (typeof endExecEvent === 'function') {
    try {
      context.subscriptions.push(
        endExecEvent(async (event) => {
          const exitCode = event.exitCode;
          if (exitCode === undefined) {
            return;
          }
          if (exitCode === 0) {
            terminalFailureStatus.hide();
            return;
          }

          const commandLine = event.execution.commandLine.value.trim() || '(unknown command)';
          const prestarted = executionStreams.get(event.execution);
          const rawOutput = await withTimeout(
            prestarted ?? collectTerminalStreamTail(event.execution),
            STREAM_DRAIN_TIMEOUT_MS,
            ''
          );
          const outputTail = cleanTerminalOutput(rawOutput);

          lastTerminalFailure = {
            commandLine,
            exitCode,
            outputTail,
            cwd: event.execution.cwd?.fsPath,
            terminalName: event.terminal.name,
            timestamp: Date.now(),
          };

          terminalFailureStatus.text = '$(error) Fix Cmd Failure';
          terminalFailureStatus.tooltip = new vscode.MarkdownString(
            `**Rursor AI — Terminal Command Failed (exit ${exitCode})**\n\n\`${escapeMarkdownInlineCode(
              truncateInline(commandLine, 90)
            )}\`\n\nClick to send command + output to Rursor Agent.`
          );
          terminalFailureStatus.show();
        })
      );
    } catch {
      // Ignore if shell execution end event is unavailable in host version.
    }
  }

  context.subscriptions.push(
    vscode.commands.registerCommand(
      'rursor.fixLastTerminalFailure',
      async (matchOverride?: ProposedTerminalCommandMatchResult) => {
        let record = lastTerminalFailure;

        if (matchOverride && typeof matchOverride.commandLine === 'string') {
          const matchedLines = matchOverride.outputMatch?.outputLines?.join('\n') ?? '';
          const fallbackOutput =
            record && record.commandLine === matchOverride.commandLine.trim()
              ? record.outputTail
              : cleanTerminalOutput(matchedLines);
          record = {
            commandLine: matchOverride.commandLine.trim() || record?.commandLine || '(command)',
            exitCode: record?.exitCode ?? 1,
            outputTail: fallbackOutput || cleanTerminalOutput(matchedLines),
            cwd: record?.cwd ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
            terminalName: record?.terminalName,
            timestamp: Date.now(),
          };
        }

        if (!record) {
          vscode.window.showInformationMessage(
            'Rursor: No failed terminal command recorded in this session yet.'
          );
          return;
        }

        terminalFailureStatus.hide();
        await vscode.commands.executeCommand('rursor.openAgent').then(undefined, () => undefined);

        const cwdLine = record.cwd ? `\n- **Working Directory:** \`${record.cwd}\`` : '';
        const termLine = record.terminalName
          ? `\n- **Terminal:** \`${record.terminalName}\``
          : '';
        const outputSection = record.outputTail
          ? `\n\n### Terminal Output (Tail)\n\`\`\`text\n${record.outputTail}\n\`\`\``
          : '\n\n*(No stdout/stderr output was captured from shell integration.)*';

        const prompt = [
          'Explain why this terminal command failed and fix the underlying issue in the workspace:',
          '',
          `- **Command:** \`${record.commandLine}\``,
          `- **Exit Code:** \`${record.exitCode}\`${cwdLine}${termLine}`,
          outputSection,
          '',
          'Identify the root cause from the error output, inspect any referenced files/lines, and apply or propose the minimal fix.',
        ].join('\n');

        await hooks.onTriggerAgentPrompt(prompt);
      }
    )
  );

  if (typeof vsAny.window?.registerTerminalQuickFixProvider === 'function') {
    try {
      const quickFixDisposable = vsAny.window.registerTerminalQuickFixProvider(
        'rursor.terminalQuickFix',
        {
          provideTerminalQuickFixes(
            commandMatchResult: ProposedTerminalCommandMatchResult,
            _token: vscode.CancellationToken
          ): ProposedTerminalQuickFixItem[] {
            const cmdPreview = truncateInline(
              commandMatchResult.commandLine.trim() || 'last command',
              48
            );
            return [
              {
                command: 'rursor.fixLastTerminalFailure',
                title: `Rursor AI: Explain & Fix Failed Command (${cmdPreview})`,
                arguments: [commandMatchResult],
              },
            ];
          },
        }
      );
      context.subscriptions.push(quickFixDisposable);
    } catch {
      // `terminalQuickFixProvider` proposal not unlocked in vanilla VS Code; skip silently.
    }
  }

  // ---------------------------------------------------------------------------
  // 4. DAP Live Debugger Agent (`rursor.debugExplainStop`)
  // ---------------------------------------------------------------------------
  let latestStackItem: vscode.DebugThread | vscode.DebugStackFrame | undefined =
    vscode.debug.activeStackItem;

  const debugStatus = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 102);
  debugStatus.text = '$(bug) Explain Stop';
  debugStatus.tooltip =
    'Rursor AI: Explain & Fix Current Breakpoint / Exception (inspects live DAP stack & locals)';
  debugStatus.command = 'rursor.debugExplainStop';
  debugStatus.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground');
  context.subscriptions.push(debugStatus);

  const syncDebugStatus = (
    item: vscode.DebugThread | vscode.DebugStackFrame | undefined
  ): void => {
    latestStackItem = item;
    if (item && 'frameId' in item) {
      debugStatus.show();
    } else if (!vscode.debug.activeDebugSession) {
      debugStatus.hide();
    } else {
      debugStatus.hide();
    }
  };

  syncDebugStatus(vscode.debug.activeStackItem);

  if (typeof vscode.debug.onDidChangeActiveStackItem === 'function') {
    context.subscriptions.push(
      vscode.debug.onDidChangeActiveStackItem((item) => {
        syncDebugStatus(item);
      })
    );
  }

  context.subscriptions.push(
    vscode.debug.onDidTerminateDebugSession(() => {
      latestStackItem = undefined;
      debugStatus.hide();
    }),
    vscode.commands.registerCommand('rursor.debugExplainStop', async () => {
      const stackItem = vscode.debug.activeStackItem ?? latestStackItem;
      const session =
        (stackItem && 'session' in stackItem ? stackItem.session : undefined) ??
        vscode.debug.activeDebugSession;

      if (!session) {
        vscode.window.showWarningMessage(
          'Rursor: No active debug session is paused at a breakpoint or exception.'
        );
        return;
      }

      const prompt = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Rursor: Inspecting live debugger stack frame & variables…',
        },
        async () => collectDapStopDiagnosticPrompt(session, stackItem)
      );

      if (!prompt) {
        vscode.window.showWarningMessage(
          'Rursor: Could not read a paused stack frame from the active debug session.'
        );
        return;
      }

      await vscode.commands.executeCommand('rursor.openAgent').then(undefined, () => undefined);
      await hooks.onTriggerAgentPrompt(prompt);
    })
  );

  // ---------------------------------------------------------------------------
  // 5. Inline Code Review Controller (`rursor.reviewGitDiff` & `rursor.clearReviewComments`)
  // ---------------------------------------------------------------------------
  const commentController = vscode.comments.createCommentController(
    'rursor-review',
    'Rursor AI Code Review'
  );
  commentController.options = {
    prompt: 'Rursor AI Code Review',
    placeHolder: 'Review finding generated by Rursor AI',
  };

  const activeReviewThreads: vscode.CommentThread[] = [];
  const clearReviewThreads = (): number => {
    const count = activeReviewThreads.length;
    for (const thread of activeReviewThreads) {
      thread.dispose();
    }
    activeReviewThreads.length = 0;
    return count;
  };

  context.subscriptions.push(
    commentController,
    new vscode.Disposable(() => {
      clearReviewThreads();
    }),
    vscode.commands.registerCommand('rursor.clearReviewComments', () => {
      const cleared = clearReviewThreads();
      vscode.window.showInformationMessage(
        cleared > 0
          ? `Rursor: Cleared ${cleared} AI review comment${cleared === 1 ? '' : 's'}.`
          : 'Rursor: No active AI review comments to clear.'
      );
    }),
    vscode.commands.registerCommand('rursor.reviewGitDiff', async () => {
      const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (!workspaceRoot) {
        vscode.window.showWarningMessage('Open a workspace folder to review git changes.');
        return;
      }

      await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: 'Rursor: Reviewing git diff with Gemini…',
          cancellable: true,
        },
        async (_progress, token) => {
          const gitDiffPayload = await fetchGitDiffContext(client, workspaceRoot);
          if (token.isCancellationRequested) {
            return;
          }

          if (!gitDiffPayload) {
            vscode.window.showInformationMessage(
              'Rursor: Working tree has no uncommitted git diff changes to review.'
            );
            return;
          }

          const reviewPrompt = [
            'Perform a high-signal code review of the following uncommitted `git diff`.',
            'Focus ONLY on real bugs, logic errors, security issues, race conditions, unhandled edge cases, or type/API misuse introduced in the changed lines.',
            'Return ONLY a valid JSON array (no markdown prose outside JSON) matching this exact schema:',
            '[',
            '  {',
            '    "file": "relative/path/from/workspace/root.ts",',
            '    "line": 42,',
            '    "severity": "critical" | "warning" | "info",',
            '    "comment": "Clear explanation of the bug or risk",',
            '    "suggestedFix": "Optional concise code snippet or fix recommendation"',
            '  }',
            ']',
            'Note: `line` MUST be the 1-based line number in the NEW (post-change) file. If there are no meaningful issues, return `[]`.',
            '',
            gitDiffPayload,
          ].join('\n');

          const { id, promise } = client.request<{ text: string }>('agent_chat', {
            messages: [{ role: 'user', content: reviewPrompt }],
            contextFiles: [],
            diagnostics: [],
            useWorkspaceSearch: false,
            includeGitDiff: false,
            mode: 'ask',
          });

          const cancelSub = token.onCancellationRequested(() => {
            void client.cancel(id);
          });

          try {
            const response = await promise;
            if (token.isCancellationRequested) {
              return;
            }

            const findings = parseReviewFindings(response.text ?? '');
            clearReviewThreads();

            if (findings.length === 0) {
              vscode.window.showInformationMessage(
                'Rursor AI Review: No issues found in the current git diff!'
              );
              return;
            }

            let firstThreadUri: vscode.Uri | undefined;
            let firstThreadRange: vscode.Range | undefined;

            for (const finding of findings) {
              const targetUri = resolveWorkspaceFileUri(workspaceRoot, finding.file);
              if (!targetUri) {
                continue;
              }

              const openDoc = vscode.workspace.textDocuments.find(
                (d) => d.uri.fsPath === targetUri.fsPath
              );
              const maxLine = openDoc ? Math.max(0, openDoc.lineCount - 1) : undefined;
              const zeroBasedLine = Math.max(0, finding.line - 1);
              const clampedLine =
                maxLine !== undefined ? Math.min(maxLine, zeroBasedLine) : zeroBasedLine;
              const range = new vscode.Range(clampedLine, 0, clampedLine, 0);

              const badge =
                finding.severity === 'critical'
                  ? '🔴 **CRITICAL**'
                  : finding.severity === 'warning'
                    ? '🟡 **WARNING**'
                    : '🔵 **INFO**';

              const bodyParts = [`${badge} — ${finding.comment}`];
              if (finding.suggestedFix && finding.suggestedFix.trim().length > 0) {
                bodyParts.push(
                  `\n\n**Suggested Fix:**\n\`\`\`\n${finding.suggestedFix.trim()}\n\`\`\``
                );
              }

              const md = new vscode.MarkdownString(bodyParts.join(''));
              md.isTrusted = false;

              const comment: vscode.Comment = {
                body: md,
                mode: vscode.CommentMode.Preview,
                author: { name: 'Rursor AI Review' },
                label: finding.severity.toUpperCase(),
                timestamp: new Date(),
              };

              const thread = commentController.createCommentThread(targetUri, range, [comment]);
              thread.label = `Rursor AI (${finding.severity}): ${truncateInline(
                finding.comment,
                64
              )}`;
              thread.canReply = false;
              thread.state = vscode.CommentThreadState.Unresolved;
              thread.collapsibleState =
                finding.severity === 'critical' || finding.severity === 'warning'
                  ? vscode.CommentThreadCollapsibleState.Expanded
                  : vscode.CommentThreadCollapsibleState.Collapsed;

              activeReviewThreads.push(thread);
              if (!firstThreadUri) {
                firstThreadUri = targetUri;
                firstThreadRange = range;
              }
            }

            if (activeReviewThreads.length === 0) {
              vscode.window.showInformationMessage(
                'Rursor AI Review completed with no mappable workspace file comments.'
              );
              return;
            }

            const choice = await vscode.window.showInformationMessage(
              `Rursor AI Review placed ${activeReviewThreads.length} inline review comment${
                activeReviewThreads.length === 1 ? '' : 's'
              } across your diff.`,
              'Jump to First Finding',
              'Open Comments Panel',
              'Clear Comments'
            );

            if (choice === 'Jump to First Finding' && firstThreadUri && firstThreadRange) {
              const doc = await vscode.workspace.openTextDocument(firstThreadUri);
              const ed = await vscode.window.showTextDocument(doc, { preview: false });
              ed.selection = new vscode.Selection(firstThreadRange.start, firstThreadRange.start);
              ed.revealRange(firstThreadRange, vscode.TextEditorRevealType.InCenter);
            } else if (choice === 'Open Comments Panel') {
              await vscode.commands
                .executeCommand('workbench.action.focusCommentsPanel')
                .then(undefined, () => undefined);
            } else if (choice === 'Clear Comments') {
              clearReviewThreads();
            }
          } catch (err) {
            if (!token.isCancellationRequested) {
              vscode.window.showErrorMessage(
                `Rursor AI Code Review failed: ${err instanceof Error ? err.message : String(err)}`
              );
            }
          } finally {
            cancelSub.dispose();
          }
        }
      );
    })
  );

  // ---------------------------------------------------------------------------
  // 6. AI Rename Symbol Suggestions (`languages.registerNewSymbolNamesProvider`)
  // ---------------------------------------------------------------------------
  if (typeof vsAny.languages?.registerNewSymbolNamesProvider === 'function') {
    try {
      const renameSub = vsAny.languages.registerNewSymbolNamesProvider(
        [{ scheme: 'file' }, { scheme: 'untitled' }],
        {
          supportsAutomaticTriggerKind: Promise.resolve(true),
          async provideNewSymbolNames(
            document: vscode.TextDocument,
            range: vscode.Range,
            _triggerKind: number,
            token: vscode.CancellationToken
          ): Promise<ProposedNewSymbolName[]> {
            const effectiveRange = range.isEmpty
              ? document.getWordRangeAtPosition(range.start) ?? range
              : range;
            const currentName = document.getText(effectiveRange).trim();
            if (!currentName || currentName.length > 120 || /\s/.test(currentName)) {
              return [];
            }

            const startLine = Math.max(0, effectiveRange.start.line - RENAME_CONTEXT_LINES);
            const endLine = Math.min(
              document.lineCount - 1,
              effectiveRange.end.line + RENAME_CONTEXT_LINES
            );
            const snippet = document.getText(
              new vscode.Range(
                new vscode.Position(startLine, 0),
                document.lineAt(endLine).range.end
              )
            );
            const relPath = vscode.workspace.asRelativePath(document.uri, false);
            const fastModel = vscode.workspace
              .getConfiguration('rursor')
              .get<string>('autocomplete.model', 'gemini-2.5-flash-lite');

            const renamePrompt = [
              `Suggest exactly 4 idiomatic, descriptive replacement identifier names for the symbol \`${currentName}\` at line ${
                effectiveRange.start.line + 1
              } in \`${relPath}\` (${document.languageId}).`,
              'Match the naming convention (camelCase, PascalCase, snake_case, SCREAMING_SNAKE_CASE) of the symbol and surrounding code.',
              'Return ONLY a JSON array of 4 strings, e.g. ["nameOne", "nameTwo", "nameThree", "nameFour"]. Do not include markdown or explanations.',
              '',
              `\`\`\`${document.languageId}`,
              snippet,
              '```',
            ].join('\n');

            const { id, promise } = client.request<{ text: string }>('agent_chat', {
              messages: [{ role: 'user', content: renamePrompt }],
              contextFiles: [],
              diagnostics: [],
              useWorkspaceSearch: false,
              includeGitDiff: false,
              mode: 'ask',
              model: fastModel,
              thinkingBudget: 0,
            });

            const cancelSub = token.onCancellationRequested(() => {
              void client.cancel(id);
            });

            try {
              const res = await promise;
              if (token.isCancellationRequested) {
                return [];
              }
              const names = parseRenameCandidates(res.text ?? '', currentName).slice(0, 4);
              const Ctor = vsAny.NewSymbolName;
              return names.map((name) =>
                typeof Ctor === 'function'
                  ? new Ctor(name, [ProposedNewSymbolNameTag.AIGenerated])
                  : { newSymbolName: name, tags: [ProposedNewSymbolNameTag.AIGenerated] }
              );
            } catch {
              return [];
            } finally {
              cancelSub.dispose();
            }
          },
        }
      );
      context.subscriptions.push(renameSub);
    } catch {
      // `newSymbolNamesProvider` proposal not unlocked in vanilla VS Code; skip silently.
    }
  }

  // ---------------------------------------------------------------------------
  // Bonus: Native `InlineEditProvider` on explicit Invoke (`inlineEdit` proposal)
  // ---------------------------------------------------------------------------
  if (
    typeof vsAny.languages?.registerInlineEditProvider === 'function' &&
    typeof vsAny.InlineEdit === 'function'
  ) {
    const InlineEditCtor = vsAny.InlineEdit;
    try {
      const inlineEditProviderSub = vsAny.languages.registerInlineEditProvider(
        { scheme: 'file' },
        {
          async provideInlineEdit(
            document: vscode.TextDocument,
            inlineContext: ProposedInlineEditContext,
            token: vscode.CancellationToken
          ): Promise<ProposedInlineEdit | undefined> {
            // Only respond to explicit user invocations so automatic ghost-text/NES stays handled by RursorInlineCompletionProvider.
            if (inlineContext.triggerKind !== ProposedInlineEditTriggerKind.Invoke) {
              return undefined;
            }
            const activeEditor = vscode.window.activeTextEditor;
            const cursorLine =
              activeEditor && activeEditor.document.uri.toString() === document.uri.toString()
                ? activeEditor.selection.active.line
                : 0;

            const focusStart = Math.max(0, cursorLine - 4);
            const focusEnd = Math.min(document.lineCount - 1, cursorLine + 8);
            const beforeStart = Math.max(0, focusStart - 30);
            const afterEnd = Math.min(document.lineCount - 1, focusEnd + 30);

            const windowBefore =
              focusStart > beforeStart
                ? document.getText(
                    new vscode.Range(
                      new vscode.Position(beforeStart, 0),
                      document.lineAt(focusStart - 1).range.end
                    )
                  )
                : '';
            const focusBlock = document.getText(
              new vscode.Range(
                new vscode.Position(focusStart, 0),
                document.lineAt(focusEnd).range.end
              )
            );
            const windowAfter =
              afterEnd > focusEnd
                ? document.getText(
                    new vscode.Range(
                      new vscode.Position(focusEnd + 1, 0),
                      document.lineAt(afterEnd).range.end
                    )
                  )
                : '';

            const relPath = vscode.workspace.asRelativePath(document.uri, false);
            const { id, promise } = client.request<NesPredictInlineResponse>('next_edit_predict', {
              filePath: relPath,
              languageId: document.languageId,
              windowBefore,
              focusBlock,
              windowAfter,
              focusStartLine: focusStart + 1,
              recentEdits: [],
            });

            const cancelSub = token.onCancellationRequested(() => {
              void client.cancel(id);
            });

            try {
              const res = await promise;
              if (token.isCancellationRequested) {
                return undefined;
              }
              const pred = res.prediction;
              if (
                !pred ||
                (pred.kind !== 'edit' && pred.kind !== 'insert') ||
                !pred.replacement
              ) {
                return undefined;
              }
              if (pred.targetPath && pred.targetPath !== relPath) {
                return undefined;
              }
              const startIdx = Math.max(
                0,
                Math.min(document.lineCount - 1, (pred.startLine ?? cursorLine + 1) - 1)
              );
              const endIdx = Math.max(
                startIdx,
                Math.min(document.lineCount - 1, (pred.endLine ?? startIdx + 1) - 1)
              );
              const replaceRange = new vscode.Range(
                new vscode.Position(startIdx, 0),
                document.lineAt(endIdx).range.end
              );
              return new InlineEditCtor(pred.replacement.replace(/\r\n/g, '\n'), replaceRange);
            } catch {
              return undefined;
            } finally {
              cancelSub.dispose();
            }
          },
        }
      );
      context.subscriptions.push(inlineEditProviderSub);
    } catch {
      // `inlineEdit` proposal not unlocked in vanilla VS Code; skip silently.
    }
  }

  return baselineTracker;
}

// =============================================================================
// Internal Helpers
// =============================================================================

function countLines(text: string): number {
  if (!text) {
    return 0;
  }
  let count = 1;
  for (let i = 0; i < text.length; i++) {
    if (text.charCodeAt(i) === 10) {
      count++;
    }
  }
  return count;
}

function truncateInline(value: string, maxLen: number): string {
  const singleLine = value.replace(/\s+/g, ' ').trim();
  if (singleLine.length <= maxLen) {
    return singleLine;
  }
  return `${singleLine.slice(0, Math.max(1, maxLen - 1))}…`;
}

function escapeHtml(text: string): string {
  return text
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function escapeMarkdownInlineCode(text: string): string {
  return text.replace(/`/g, "'");
}

function countDiagnosticsNearRange(uri: vscode.Uri, range: vscode.Range): number {
  const diagnostics = vscode.languages.getDiagnostics(uri);
  return diagnostics.filter(
    (d) =>
      d.range.end.line >= Math.max(0, range.start.line - 3) &&
      d.range.start.line <= range.end.line + 3
  ).length;
}

function renderInlineEditInsetHtml(
  cspSource: string,
  relPath: string,
  lineLabel: string,
  diagCount: number
): string {
  const nonce = crypto.randomBytes(16).toString('base64');
  const presets: Array<{ label: string; instruction: string }> = [
    ...(diagCount > 0
      ? [
          {
            label: `Fix ${diagCount} Issue${diagCount === 1 ? '' : 's'}`,
            instruction: 'Fix the diagnostics in this code',
          },
        ]
      : []),
    { label: 'Refactor', instruction: 'Refactor and improve readability' },
    {
      label: 'Error Handling',
      instruction: 'Add robust error handling and edge-case checks',
    },
    { label: 'Types & Docs', instruction: 'Add concise types and docstrings' },
    { label: 'Unit Tests', instruction: 'Write unit tests for this code' },
    { label: 'Optimize', instruction: 'Optimize performance and simplify logic' },
  ];

  const chipsHtml = presets
    .map(
      (p) =>
        `<button type="button" class="chip" data-instruction="${escapeHtml(
          p.instruction
        )}">${escapeHtml(p.label)}</button>`
    )
    .join('');

  const fileShort = escapeHtml(path.basename(relPath) || relPath);

  return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src ${cspSource} 'nonce-${nonce}'; script-src 'nonce-${nonce}';" />
  <style nonce="${nonce}">
    * {
      box-sizing: border-box;
      margin: 0;
      padding: 0;
    }
    html, body {
      width: 100%;
      height: 100vh;
      overflow: hidden;
      background: transparent;
      font-family: var(--vscode-font-family, -apple-system, BlinkMacSystemFont, sans-serif);
      color: var(--vscode-editorWidget-foreground, var(--vscode-foreground));
    }
    .composer {
      display: flex;
      flex-direction: column;
      justify-content: center;
      gap: 5px;
      height: calc(100vh - 4px);
      margin: 2px 12px 2px 4px;
      padding: 5px 10px;
      background: var(--vscode-editorWidget-background);
      border: 1px solid var(--vscode-focusBorder, var(--vscode-editorWidget-border));
      border-radius: 7px;
      box-shadow: 0 4px 14px rgba(0, 0, 0, 0.26);
    }
    .top-row {
      display: flex;
      align-items: center;
      gap: 6px;
      height: 26px;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 4px;
      padding: 2px 7px;
      border-radius: 4px;
      font-size: 11px;
      font-weight: 600;
      white-space: nowrap;
      background: var(--vscode-badge-background);
      color: var(--vscode-badge-foreground);
    }
    .prompt-input {
      flex: 1;
      min-width: 120px;
      height: 25px;
      padding: 0 8px;
      border-radius: 4px;
      border: 1px solid var(--vscode-input-border, transparent);
      background: var(--vscode-input-background);
      color: var(--vscode-input-foreground);
      font-size: 12px;
      outline: none;
    }
    .prompt-input:focus {
      border-color: var(--vscode-focusBorder);
    }
    .prompt-input::placeholder {
      color: var(--vscode-input-placeholderForeground);
    }
    .btn {
      height: 24px;
      padding: 0 9px;
      border-radius: 4px;
      border: none;
      font-size: 11px;
      font-weight: 600;
      cursor: pointer;
      white-space: nowrap;
    }
    .btn-primary {
      background: var(--vscode-button-background);
      color: var(--vscode-button-foreground);
    }
    .btn-primary:hover {
      background: var(--vscode-button-hoverBackground);
    }
    .btn-ghost {
      background: transparent;
      color: var(--vscode-descriptionForeground);
      border: 1px solid var(--vscode-editorWidget-border, transparent);
      padding: 0 7px;
    }
    .btn-ghost:hover {
      color: var(--vscode-foreground);
      background: var(--vscode-toolbar-hoverBackground);
    }
    .chips-row {
      display: flex;
      align-items: center;
      gap: 5px;
      overflow-x: auto;
      scrollbar-width: none;
      height: 22px;
    }
    .chips-row::-webkit-scrollbar {
      display: none;
    }
    .hint {
      font-size: 10.5px;
      color: var(--vscode-descriptionForeground);
      white-space: nowrap;
      margin-right: 2px;
    }
    .chip {
      height: 20px;
      padding: 0 8px;
      border-radius: 999px;
      border: 1px solid var(--vscode-editorWidget-border, rgba(128, 128, 128, 0.35));
      background: var(--vscode-editor-background);
      color: var(--vscode-foreground);
      font-size: 11px;
      cursor: pointer;
      white-space: nowrap;
    }
    .chip:hover {
      border-color: var(--vscode-focusBorder);
      background: var(--vscode-list-hoverBackground);
    }
  </style>
</head>
<body>
  <div class="composer">
    <div class="top-row">
      <span class="badge" title="${escapeHtml(relPath)} (${escapeHtml(lineLabel)})">✦ ${fileShort} · ${escapeHtml(lineLabel)}</span>
      <input
        id="promptInput"
        class="prompt-input"
        type="text"
        placeholder="Ask Rursor to edit selection… (Enter to run, Esc to close)"
        autocomplete="off"
        spellcheck="false"
      />
      <button id="runBtn" type="button" class="btn btn-primary">Run ↵</button>
      <button id="closeBtn" type="button" class="btn btn-ghost" title="Close (Esc)">Esc</button>
    </div>
    <div class="chips-row">
      <span class="hint">Presets:</span>
      ${chipsHtml}
    </div>
  </div>
  <script nonce="${nonce}">
    (function () {
      const vscode = acquireVsCodeApi();
      const input = document.getElementById('promptInput');
      const runBtn = document.getElementById('runBtn');
      const closeBtn = document.getElementById('closeBtn');

      function focusInput() {
        if (input) {
          input.focus();
        }
      }

      function submit(text) {
        const trimmed = (text || '').trim();
        if (!trimmed) {
          focusInput();
          return;
        }
        vscode.postMessage({ type: 'submit', instruction: trimmed });
      }

      runBtn.addEventListener('click', function () {
        submit(input.value);
      });

      closeBtn.addEventListener('click', function () {
        vscode.postMessage({ type: 'cancel' });
      });

      input.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' && !ev.isComposing) {
          ev.preventDefault();
          submit(input.value);
        } else if (ev.key === 'Escape') {
          ev.preventDefault();
          vscode.postMessage({ type: 'cancel' });
        }
      });

      window.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') {
          ev.preventDefault();
          vscode.postMessage({ type: 'cancel' });
        }
      });

      const chips = document.querySelectorAll('.chip');
      chips.forEach(function (chip) {
        chip.addEventListener('click', function () {
          const preset = chip.getAttribute('data-instruction') || '';
          const typed = (input.value || '').trim();
          submit(typed ? typed + ' — ' + preset : preset);
        });
      });

      focusInput();
      window.addEventListener('load', focusInput);
      setTimeout(focusInput, 35);
    })();
  </script>
</body>
</html>`;
}

async function promptFallbackInlineEditInstruction(diagCount: number): Promise<string | undefined> {
  const qp = vscode.window.createQuickPick<vscode.QuickPickItem>();
  qp.title = 'Rursor Inline Edit Composer (Cmd+K)';
  qp.placeholder =
    diagCount > 0
      ? `Type an instruction or choose a preset (${diagCount} nearby diagnostic(s))`
      : 'Type an instruction or choose a preset…';
  qp.matchOnDescription = true;

  const baseItems: vscode.QuickPickItem[] = [
    ...(diagCount > 0
      ? [
          {
            label: '$(wrench) Fix the diagnostics in this code',
            description: `${diagCount} diagnostic(s)`,
          },
        ]
      : []),
    { label: '$(sparkle) Refactor and improve readability', description: 'Preset' },
    {
      label: '$(shield) Add robust error handling and edge-case checks',
      description: 'Preset',
    },
    { label: '$(symbol-Namespace) Add concise types and docstrings', description: 'Preset' },
    { label: '$(beaker) Write unit tests for this code', description: 'Preset' },
  ];

  qp.items = baseItems;
  qp.onDidChangeValue((val) => {
    const trimmed = val.trim();
    qp.items = trimmed
      ? [{ label: `$(play) ${trimmed}`, description: 'Run custom instruction' }, ...baseItems]
      : baseItems;
  });

  return new Promise<string | undefined>((resolve) => {
    let settled = false;
    qp.onDidAccept(() => {
      settled = true;
      const selected = qp.selectedItems[0];
      const raw = selected ? selected.label.replace(/^\$\([a-zA-Z-]+\)\s*/, '') : qp.value.trim();
      qp.hide();
      resolve(raw.trim() || undefined);
    });
    qp.onDidHide(() => {
      qp.dispose();
      if (!settled) {
        resolve(undefined);
      }
    });
    qp.show();
  });
}

async function collectTerminalStreamTail(
  execution: vscode.TerminalShellExecution
): Promise<string> {
  let buffer = '';
  try {
    const stream = execution.read();
    for await (const chunk of stream) {
      buffer += chunk;
      if (buffer.length > MAX_RAW_TERMINAL_BUFFER) {
        buffer = buffer.slice(buffer.length - MAX_RAW_TERMINAL_BUFFER);
      }
    }
  } catch {
    // Stream may close early if terminal is disposed.
  }
  return buffer;
}

function withTimeout<T>(promise: Promise<T>, timeoutMs: number, fallback: T): Promise<T> {
  return new Promise<T>((resolve) => {
    let done = false;
    const timer = setTimeout(() => {
      if (!done) {
        done = true;
        resolve(fallback);
      }
    }, timeoutMs);

    promise
      .then((val) => {
        if (!done) {
          done = true;
          clearTimeout(timer);
          resolve(val);
        }
      })
      .catch(() => {
        if (!done) {
          done = true;
          clearTimeout(timer);
          resolve(fallback);
        }
      });
  });
}

/**
 * Strips OSC (e.g. VS Code `]633;...` shell integration markers) and CSI/ANSI escape sequences,
 * collapses carriage-return spinner overwrites, and extracts the clean output tail.
 */
export function cleanTerminalOutput(raw: string): string {
  if (!raw) {
    return '';
  }
  const withoutOsc = raw.replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '');
  const withoutCsi = withoutOsc
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, '')
    .replace(/\x1b[@-Z\\-_]/g, '');

  const normalizedLines = withoutCsi
    .replace(/\r\n/g, '\n')
    .split('\n')
    .map((line) => {
      const segments = line.split('\r');
      return (segments[segments.length - 1] ?? '').replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, '');
    });

  while (normalizedLines.length > 0 && normalizedLines[0].trim() === '') {
    normalizedLines.shift();
  }
  while (
    normalizedLines.length > 0 &&
    normalizedLines[normalizedLines.length - 1].trim() === ''
  ) {
    normalizedLines.pop();
  }

  const tailLines = normalizedLines.slice(-MAX_TERMINAL_OUTPUT_LINES);
  const joined = tailLines.join('\n');
  if (joined.length <= MAX_TERMINAL_OUTPUT_CHARS) {
    return joined;
  }
  return joined.slice(joined.length - MAX_TERMINAL_OUTPUT_CHARS);
}

async function collectDapStopDiagnosticPrompt(
  session: vscode.DebugSession,
  stackItem: vscode.DebugThread | vscode.DebugStackFrame | undefined
): Promise<string | undefined> {
  let threadId: number | undefined =
    stackItem && 'threadId' in stackItem ? stackItem.threadId : undefined;
  const preferredFrameId: number | undefined =
    stackItem && 'frameId' in stackItem ? stackItem.frameId : undefined;

  if (threadId === undefined) {
    try {
      const threadsRes = (await session.customRequest('threads')) as DapThreadsResponse;
      threadId = threadsRes.threads?.[0]?.id;
    } catch {
      threadId = 1;
    }
  }

  if (threadId === undefined) {
    return undefined;
  }

  let stackFrames: DapStackFrame[] = [];
  try {
    const stackRes = (await session.customRequest('stackTrace', {
      threadId,
      startFrame: 0,
      levels: 12,
    })) as DapStackTraceResponse;
    stackFrames = stackRes.stackFrames ?? [];
  } catch {
    return undefined;
  }

  if (stackFrames.length === 0) {
    return undefined;
  }

  const topFrame =
    (preferredFrameId !== undefined
      ? stackFrames.find((f) => f.id === preferredFrameId)
      : undefined) ?? stackFrames[0];

  let exceptionBlock = '';
  try {
    const exc = (await session.customRequest('exceptionInfo', {
      threadId,
    })) as DapExceptionInfoResponse;
    if (exc && (exc.exceptionId || exc.description || exc.details?.message)) {
      const parts: string[] = [];
      if (exc.exceptionId) {
        parts.push(`- **Exception:** \`${exc.exceptionId}\``);
      }
      if (exc.description) {
        parts.push(`- **Description:** ${exc.description}`);
      }
      if (exc.details?.message && exc.details.message !== exc.description) {
        parts.push(`- **Details:** ${exc.details.message}`);
      }
      exceptionBlock = `\n### Exception Info\n${parts.join('\n')}\n`;
    }
  } catch {
    // Debug adapter is paused on a normal breakpoint rather than an exception.
  }

  const scopeBlocks: string[] = [];
  try {
    const scopesRes = (await session.customRequest('scopes', {
      frameId: topFrame.id,
    })) as DapScopesResponse;
    const allScopes = scopesRes.scopes ?? [];
    const nonGlobalScopes = allScopes.filter(
      (s) => !s.expensive && s.variablesReference > 0 && !/global/i.test(s.name)
    );
    const chosenScopes = (nonGlobalScopes.length > 0 ? nonGlobalScopes : allScopes)
      .filter((s) => !s.expensive && s.variablesReference > 0)
      .slice(0, 3);

    for (const scope of chosenScopes) {
      try {
        const varsRes = (await session.customRequest('variables', {
          variablesReference: scope.variablesReference,
          count: 25,
        })) as DapVariablesResponse;
        const vars = (varsRes.variables ?? []).slice(0, 25);
        if (vars.length > 0) {
          const formattedVars = vars
            .map((v) => {
              const typeSuffix = v.type ? `: ${v.type}` : '';
              return `  ${v.name}${typeSuffix} = ${truncateInline(v.value ?? 'undefined', 260)}`;
            })
            .join('\n');
          scopeBlocks.push(`#### Scope: ${scope.name}\n\`\`\`text\n${formattedVars}\n\`\`\``);
        }
      } catch {
        // Skip unreadable scope.
      }
    }
  } catch {
    // Scopes unavailable for this frame.
  }

  const sourcePath = topFrame.source?.path;
  const displayLocation = sourcePath
    ? `${vscode.workspace.asRelativePath(vscode.Uri.file(sourcePath), false)}:${topFrame.line}:${
        topFrame.column || 1
      }`
    : `${topFrame.source?.name ?? '<unknown>'}:${topFrame.line}`;

  let sourceSnippetBlock = '';
  if (sourcePath && fs.existsSync(sourcePath)) {
    try {
      const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(sourcePath));
      const stopZeroBased = Math.max(0, Math.min(doc.lineCount - 1, topFrame.line - 1));
      const start = Math.max(0, stopZeroBased - 10);
      const end = Math.min(doc.lineCount - 1, stopZeroBased + 10);
      const numberedLines: string[] = [];
      for (let i = start; i <= end; i++) {
        const marker = i === stopZeroBased ? '>>>' : '   ';
        numberedLines.push(`${marker} ${String(i + 1).padStart(4, ' ')} | ${doc.lineAt(i).text}`);
      }
      sourceSnippetBlock = `\n### Paused Source Context (\`${displayLocation}\`)\n\`\`\`${
        doc.languageId || 'text'
      }\n${numberedLines.join('\n')}\n\`\`\`\n`;
    } catch {
      // Ignore if source file cannot be opened as text.
    }
  }

  const callStackText = stackFrames
    .slice(0, 8)
    .map((frame, idx) => {
      const frameLoc = frame.source?.path
        ? `${vscode.workspace.asRelativePath(vscode.Uri.file(frame.source.path), false)}:${
            frame.line
          }`
        : `${frame.source?.name ?? '<native>'}:${frame.line}`;
      return `#${idx} ${frame.name || '<anonymous>'} (${frameLoc})`;
    })
    .join('\n');

  const localsSection =
    scopeBlocks.length > 0
      ? `\n### Live Frame Variables\n${scopeBlocks.join('\n\n')}\n`
      : '\n*(No local variables reported by the debug adapter for this frame.)*\n';

  return [
    'Explain the current debugger stop / exception using the live DAP state below, identify the root cause, and propose or apply a fix:',
    '',
    `- **Debug Session:** \`${session.name}\` (\`${session.type}\`)`,
    `- **Paused Frame:** \`${topFrame.name || '<top>'}\` at \`${displayLocation}\``,
    exceptionBlock,
    '### Call Stack',
    '```text',
    callStackText,
    '```',
    sourceSnippetBlock,
    localsSection,
    'Diagnose why state reached this condition at the paused line and fix any bug in the code.',
  ]
    .filter((section) => section.length > 0)
    .join('\n');
}

async function fetchGitDiffContext(
  client: SidecarClient,
  workspaceRoot: string
): Promise<string | undefined> {
  try {
    const { promise } = client.request<{ context: string }>('git_context');
    const res = await promise;
    const ctx = (res.context ?? '').trim();
    const hasUnstagedDiff = ctx.length > 0 && !ctx.includes('(no unstaged diff)');
    if (hasUnstagedDiff) {
      return ctx;
    }
  } catch {
    // Fall back to direct `git diff HEAD` below if sidecar index is not initialized yet.
  }

  try {
    const [statusRes, diffHeadRes] = await Promise.all([
      execFileAsync('git', ['status', '--short'], { cwd: workspaceRoot, maxBuffer: 2 * 1024 * 1024 }),
      execFileAsync('git', ['diff', 'HEAD', '--unified=3'], {
        cwd: workspaceRoot,
        maxBuffer: 4 * 1024 * 1024,
      }).catch(() =>
        execFileAsync('git', ['diff', '--cached', '--unified=3'], {
          cwd: workspaceRoot,
          maxBuffer: 4 * 1024 * 1024,
        })
      ),
    ]);

    const status = statusRes.stdout.trim();
    const diff = diffHeadRes.stdout.trim().slice(0, 14_000);
    if (!diff) {
      return undefined;
    }
    return `--- git status --short ---\n${status || '(modified)'}\n--- git diff ---\n${diff}`;
  } catch {
    return undefined;
  }
}

export function parseReviewFindings(rawText: string): ReviewFinding[] {
  const trimmed = rawText.trim();
  if (!trimmed) {
    return [];
  }

  const fenceMatch = /```(?:json)?\s*([\s\S]*?)```/i.exec(trimmed);
  const candidate = fenceMatch ? fenceMatch[1].trim() : trimmed;

  const jsonStart = candidate.indexOf('[');
  const jsonEnd = candidate.lastIndexOf(']');
  let parsed: unknown;

  try {
    parsed = JSON.parse(candidate);
  } catch {
    if (jsonStart !== -1 && jsonEnd > jsonStart) {
      try {
        parsed = JSON.parse(candidate.slice(jsonStart, jsonEnd + 1));
      } catch {
        return [];
      }
    } else {
      return [];
    }
  }

  let list: unknown[] = [];
  if (Array.isArray(parsed)) {
    list = parsed;
  } else if (parsed && typeof parsed === 'object') {
    const obj = parsed as Record<string, unknown>;
    if (Array.isArray(obj.findings)) {
      list = obj.findings;
    } else if (Array.isArray(obj.issues)) {
      list = obj.issues;
    } else if (Array.isArray(obj.comments)) {
      list = obj.comments;
    }
  }

  const out: ReviewFinding[] = [];
  for (const item of list) {
    if (!item || typeof item !== 'object') {
      continue;
    }
    const rec = item as Record<string, unknown>;
    const rawFile = typeof rec.file === 'string' ? rec.file.trim() : '';
    const rawComment = typeof rec.comment === 'string' ? rec.comment.trim() : '';
    if (!rawFile || !rawComment) {
      continue;
    }

    const numericLine = Number(rec.line);
    const line = Number.isFinite(numericLine) && numericLine >= 1 ? Math.floor(numericLine) : 1;

    const rawSev = typeof rec.severity === 'string' ? rec.severity.toLowerCase().trim() : 'info';
    const severity: 'critical' | 'warning' | 'info' =
      rawSev === 'critical' || rawSev === 'error' || rawSev === 'high'
        ? 'critical'
        : rawSev === 'warning' || rawSev === 'warn' || rawSev === 'medium'
          ? 'warning'
          : 'info';

    const suggestedFix =
      typeof rec.suggestedFix === 'string' && rec.suggestedFix.trim().length > 0
        ? rec.suggestedFix.trim()
        : undefined;

    out.push({
      file: rawFile,
      line,
      severity,
      comment: rawComment,
      suggestedFix,
    });
  }

  return out;
}

function resolveWorkspaceFileUri(workspaceRoot: string, rawFile: string): vscode.Uri | undefined {
  const normalizedRoot = path.resolve(workspaceRoot);
  const cleaned = rawFile.replace(/\\/g, '/').replace(/^\.\//, '');

  const candidates = [cleaned];
  if (cleaned.startsWith('b/') || cleaned.startsWith('a/')) {
    candidates.push(cleaned.slice(2));
  }

  for (const rel of candidates) {
    const resolved = path.resolve(normalizedRoot, rel);
    if (
      resolved !== normalizedRoot &&
      !resolved.startsWith(`${normalizedRoot}${path.sep}`)
    ) {
      continue;
    }
    if (fs.existsSync(resolved)) {
      return vscode.Uri.file(resolved);
    }
  }

  const fallback = path.resolve(normalizedRoot, candidates[candidates.length - 1]);
  if (fallback.startsWith(`${normalizedRoot}${path.sep}`)) {
    return vscode.Uri.file(fallback);
  }
  return undefined;
}

export function parseRenameCandidates(rawText: string, currentName: string): string[] {
  const trimmed = rawText.trim();
  if (!trimmed) {
    return [];
  }

  const fenceMatch = /```(?:json)?\s*([\s\S]*?)```/i.exec(trimmed);
  const body = fenceMatch ? fenceMatch[1].trim() : trimmed;

  let rawStrings: string[] = [];
  const startBracket = body.indexOf('[');
  const endBracket = body.lastIndexOf(']');
  if (startBracket !== -1 && endBracket > startBracket) {
    try {
      const parsed = JSON.parse(body.slice(startBracket, endBracket + 1)) as unknown;
      if (Array.isArray(parsed)) {
        rawStrings = parsed
          .map((item) => {
            if (typeof item === 'string') {
              return item;
            }
            if (item && typeof item === 'object' && 'name' in item) {
              const nameVal = (item as { name?: unknown }).name;
              return typeof nameVal === 'string' ? nameVal : '';
            }
            return '';
          })
          .filter((s) => s.length > 0);
      }
    } catch {
      // Fall back to line parsing below.
    }
  }

  if (rawStrings.length === 0) {
    rawStrings = body
      .split(/\r?\n|,/)
      .map((line) => line.replace(/^[\s\-*0-9.)`"'[\]]+|[\s`"'[\]]+$/g, ''))
      .filter((s) => s.length > 0);
  }

  const allowHyphen = currentName.includes('-');
  const identifierPattern = allowHyphen
    ? /^[\p{L}_$][\p{L}\p{N}_$-]*$/u
    : /^[\p{L}_$][\p{L}\p{N}_$]*$/u;

  const seen = new Set<string>([currentName]);
  const valid: string[] = [];

  for (const candidate of rawStrings) {
    const cleaned = candidate.replace(/^[`"']|[`"']$/g, '').trim();
    if (!cleaned || seen.has(cleaned) || cleaned.length > 100) {
      continue;
    }
    if (identifierPattern.test(cleaned)) {
      seen.add(cleaned);
      valid.push(cleaned);
    }
  }

  return valid;
}
