import * as vscode from 'vscode';

const DEFAULT_LSP_TIMEOUT_MS = 160;
const DEFAULT_LSP_MAX_CHARS = 6000;
const DEFAULT_MAX_SYMBOLS = 8;
const CONTEXT_MARGIN_LINES = 3;
const DEFINITION_CONTEXT_LINES = 8;
const MAX_CALLER_SITES = 10;
const MAX_HOVER_CHARS_PER_SYMBOL = 360;
const MAX_PREVIEW_CHARS = 120;
const MAX_FILE_READ_BYTES = 512 * 1024;
const DEFAULT_DIAGNOSTIC_WAIT_MS = 220;
const DEFAULT_INTENT_CAPACITY = 15;
const NAVIGATION_LINE_JUMP_THRESHOLD = 8;

const STOP_KEYWORDS: ReadonlySet<string> = new Set([
  // JavaScript / TypeScript keywords & ubiquitous globals
  'abstract',
  'any',
  'arguments',
  'as',
  'async',
  'await',
  'bigint',
  'boolean',
  'break',
  'case',
  'catch',
  'class',
  'const',
  'constructor',
  'continue',
  'debugger',
  'declare',
  'default',
  'delete',
  'do',
  'else',
  'enum',
  'export',
  'extends',
  'false',
  'finally',
  'for',
  'from',
  'function',
  'get',
  'global',
  'if',
  'implements',
  'import',
  'in',
  'infer',
  'instanceof',
  'interface',
  'is',
  'keyof',
  'let',
  'module',
  'namespace',
  'never',
  'new',
  'null',
  'number',
  'object',
  'of',
  'override',
  'package',
  'private',
  'protected',
  'public',
  'readonly',
  'require',
  'return',
  'satisfies',
  'set',
  'static',
  'string',
  'super',
  'switch',
  'symbol',
  'this',
  'throw',
  'true',
  'try',
  'type',
  'typeof',
  'undefined',
  'unique',
  'unknown',
  'using',
  'var',
  'void',
  'while',
  'with',
  'yield',
  'Array',
  'Boolean',
  'Date',
  'Error',
  'Function',
  'JSON',
  'Map',
  'Math',
  'Number',
  'Object',
  'Partial',
  'Promise',
  'Readonly',
  'Record',
  'RegExp',
  'Required',
  'Set',
  'String',
  'console',
  // Rust keywords & prelude items
  'Self',
  'Some',
  'None',
  'Ok',
  'Err',
  'Option',
  'Result',
  'Vec',
  'Box',
  'Arc',
  'Rc',
  'Clone',
  'Copy',
  'Debug',
  'Default',
  'Send',
  'Sync',
  'bool',
  'char',
  'crate',
  'dyn',
  'extern',
  'f32',
  'f64',
  'fn',
  'i8',
  'i16',
  'i32',
  'i64',
  'i128',
  'impl',
  'isize',
  'loop',
  'macro_rules',
  'match',
  'mod',
  'move',
  'mut',
  'pub',
  'ref',
  'self',
  'str',
  'struct',
  'trait',
  'u8',
  'u16',
  'u32',
  'u64',
  'u128',
  'unsafe',
  'use',
  'usize',
  'where',
  // Python keywords & builtins
  'False',
  'None',
  'True',
  'and',
  'assert',
  'classmethod',
  'cls',
  'def',
  'del',
  'dict',
  'elif',
  'except',
  'float',
  'int',
  'isinstance',
  'issubclass',
  'lambda',
  'len',
  'list',
  'nonlocal',
  'not',
  'or',
  'pass',
  'print',
  'property',
  'raise',
  'range',
  'set',
  'staticmethod',
  'tuple',
]);

const HASH_COMMENT_LANGUAGES: ReadonlySet<string> = new Set([
  'python',
  'ruby',
  'perl',
  'shellscript',
  'yaml',
  'toml',
  'dockerfile',
  'makefile',
  'r',
]);

export interface LspContextOptions {
  timeoutMs?: number;
  maxChars?: number;
}

interface CandidateSymbol {
  name: string;
  position: vscode.Position;
  score: number;
}

interface NormalizedLocation {
  uri: vscode.Uri;
  range: vscode.Range;
}

interface SymbolLspFacts {
  symbol: CandidateSymbol;
  hoverSignature?: string;
  definitions: NormalizedLocation[];
  references: NormalizedLocation[];
}

/**
 * Queries VS Code's active language server providers concurrently for symbols in `range`
 * (+/- 3 surrounding lines) under a strict overall deadline (`timeoutMs ?? 160` ms) and
 * formats a bounded `<lsp_context>...</lsp_context>` block within `maxChars ?? 6000`.
 */
export async function collectLspContext(
  document: vscode.TextDocument,
  range: vscode.Range,
  options?: LspContextOptions
): Promise<string> {
  if (document.isClosed || document.lineCount === 0) {
    return '';
  }

  const timeoutMs = Math.max(15, options?.timeoutMs ?? DEFAULT_LSP_TIMEOUT_MS);
  const maxChars = Math.max(256, options?.maxChars ?? DEFAULT_LSP_MAX_CHARS);
  const startTime = Date.now();
  const hardDeadline = startTime + timeoutMs;
  // Reserve ~22% of the budget so fast symbols still get snippet/caller file reads
  // even when one slow language server provider times out.
  const lspQueryDeadline = startTime + Math.max(10, Math.floor(timeoutMs * 0.78));

  const symbols = extractCandidateSymbols(document, range, DEFAULT_MAX_SYMBOLS);
  if (symbols.length === 0) {
    return '';
  }

  const windowStartLine = Math.max(0, range.start.line - CONTEXT_MARGIN_LINES);
  const windowEndLine = Math.min(document.lineCount - 1, range.end.line + CONTEXT_MARGIN_LINES);
  const fileLineCache = new Map<string, Promise<string[] | undefined>>();

  const readLinesCached = (uri: vscode.Uri): Promise<string[] | undefined> => {
    const key = uri.toString();
    const existing = fileLineCache.get(key);
    if (existing) {
      return existing;
    }
    const pending = readWorkspaceFileLines(uri, hardDeadline);
    fileLineCache.set(key, pending);
    return pending;
  };

  // Launch all symbol provider queries concurrently against the LSP query deadline.
  const symbolFactPromises: Array<Promise<SymbolLspFacts>> = symbols.map(async (symbol) => {
    const [hovers, defs, typeDefs, refs] = await Promise.all([
      executeBeforeDeadline<vscode.Hover[]>(
        vscode.commands.executeCommand<vscode.Hover[]>(
          'vscode.executeHoverProvider',
          document.uri,
          symbol.position
        ),
        lspQueryDeadline,
        []
      ),
      executeBeforeDeadline<Array<vscode.Location | vscode.LocationLink>>(
        vscode.commands.executeCommand<Array<vscode.Location | vscode.LocationLink>>(
          'vscode.executeDefinitionProvider',
          document.uri,
          symbol.position
        ),
        lspQueryDeadline,
        []
      ),
      executeBeforeDeadline<Array<vscode.Location | vscode.LocationLink>>(
        vscode.commands.executeCommand<Array<vscode.Location | vscode.LocationLink>>(
          'vscode.executeTypeDefinitionProvider',
          document.uri,
          symbol.position
        ),
        lspQueryDeadline,
        []
      ),
      executeBeforeDeadline<Array<vscode.Location | vscode.LocationLink>>(
        vscode.commands.executeCommand<Array<vscode.Location | vscode.LocationLink>>(
          'vscode.executeReferenceProvider',
          document.uri,
          symbol.position
        ),
        lspQueryDeadline,
        []
      ),
    ]);

    const hoverSignature = extractHoverSignature(hovers);
    const definitions = normalizeWorkspaceLocations([...(defs ?? []), ...(typeDefs ?? [])]).filter(
      (loc) =>
        !(
          loc.uri.toString() === document.uri.toString() &&
          loc.range.start.line >= windowStartLine &&
          loc.range.start.line <= windowEndLine
        )
    );
    const references = normalizeWorkspaceLocations(refs ?? []).filter(
      (loc) =>
        !(
          loc.uri.toString() === document.uri.toString() &&
          loc.range.start.line === symbol.position.line
        )
    );

    return {
      symbol,
      hoverSignature,
      definitions,
      references,
    };
  });

  const facts = await executeBeforeDeadline(Promise.all(symbolFactPromises), hardDeadline, []);
  if (facts.length === 0) {
    return '';
  }

  // 1. Collect concise markdown type signatures from hovers.
  const hoverBlocks: string[] = [];
  const seenHoverText = new Set<string>();
  for (const item of facts) {
    if (!item.hoverSignature) {
      continue;
    }
    const normalizedKey = `${item.symbol.name}:${item.hoverSignature}`;
    if (seenHoverText.has(normalizedKey)) {
      continue;
    }
    seenHoverText.add(normalizedKey);
    hoverBlocks.push(`- \`${item.symbol.name}\`: ${formatInlineOrBlockSignature(item.hoverSignature)}`);
  }

  // 2. Collect +/- 8 line workspace definition & type-definition snippets.
  const definitionBlocks: string[] = [];
  const seenDefBuckets = new Set<string>();
  for (const item of facts) {
    if (Date.now() >= hardDeadline) {
      break;
    }
    for (const def of item.definitions.slice(0, 2)) {
      if (Date.now() >= hardDeadline || definitionBlocks.length >= 6) {
        break;
      }
      const centerLine = def.range.start.line;
      const bucketKey = `${def.uri.toString()}:${Math.floor(centerLine / DEFINITION_CONTEXT_LINES)}`;
      if (seenDefBuckets.has(bucketKey)) {
        continue;
      }
      seenDefBuckets.add(bucketKey);

      const lines = await readLinesCached(def.uri);
      if (!lines || lines.length === 0) {
        continue;
      }
      const snippetStart = Math.max(0, centerLine - DEFINITION_CONTEXT_LINES);
      const snippetEnd = Math.min(lines.length - 1, centerLine + DEFINITION_CONTEXT_LINES);
      const relPath = toWorkspaceRelativePath(def.uri);
      const numbered = lines
        .slice(snippetStart, snippetEnd + 1)
        .map((lineText, idx) => `${snippetStart + idx + 1}: ${lineText}`)
        .join('\n');
      definitionBlocks.push(
        `#### ${relPath}:${snippetStart + 1}-${snippetEnd + 1} (${item.symbol.name})\n\`\`\`\n${numbered}\n\`\`\``
      );
    }
  }

  // 3. Collect up to 10 workspace caller/reference sites (`relPath:line: preview`) round-robin across symbols.
  const callerLines: string[] = [];
  const seenCallerKeys = new Set<string>();
  const factsWithRefs = facts.filter((f) => f.references.length > 0);
  let refIdx = 0;
  let madeProgress = true;

  while (
    callerLines.length < MAX_CALLER_SITES &&
    madeProgress &&
    Date.now() < hardDeadline &&
    factsWithRefs.length > 0
  ) {
    madeProgress = false;
    for (const item of factsWithRefs) {
      if (callerLines.length >= MAX_CALLER_SITES || Date.now() >= hardDeadline) {
        break;
      }
      if (refIdx >= item.references.length) {
        continue;
      }
      madeProgress = true;
      const ref = item.references[refIdx];
      const relPath = toWorkspaceRelativePath(ref.uri);
      const line1Based = ref.range.start.line + 1;
      const callerKey = `${relPath}:${line1Based}`;
      if (seenCallerKeys.has(callerKey)) {
        continue;
      }
      seenCallerKeys.add(callerKey);

      const lines = await readLinesCached(ref.uri);
      const rawLine = lines?.[ref.range.start.line]?.trim() ?? `(${item.symbol.name} reference)`;
      const preview = truncateSingleLine(rawLine, MAX_PREVIEW_CHARS);
      callerLines.push(`- ${relPath}:${line1Based}: ${preview}`);
    }
    refIdx++;
  }

  if (hoverBlocks.length === 0 && definitionBlocks.length === 0 && callerLines.length === 0) {
    return '';
  }

  const sections: string[] = [];
  if (hoverBlocks.length > 0) {
    sections.push(`### Type Signatures\n${hoverBlocks.join('\n')}`);
  }
  if (definitionBlocks.length > 0) {
    sections.push(`### Definitions\n${definitionBlocks.join('\n\n')}`);
  }
  if (callerLines.length > 0) {
    sections.push(`### Workspace References\n${callerLines.join('\n')}`);
  }

  return formatBoundedTagBlock('lsp_context', sections.join('\n\n'), maxChars);
}

function extractCandidateSymbols(
  document: vscode.TextDocument,
  range: vscode.Range,
  maxSymbols: number
): CandidateSymbol[] {
  const startLine = Math.max(0, range.start.line - CONTEXT_MARGIN_LINES);
  const endLine = Math.min(document.lineCount - 1, range.end.line + CONTEXT_MARGIN_LINES);
  const stripHashComments = HASH_COMMENT_LANGUAGES.has(document.languageId);

  const bestBySymbol = new Map<string, CandidateSymbol>();
  const identRegex = /[A-Za-z_][A-Za-z0-9_]{2,}/g;

  for (let lineIdx = startLine; lineIdx <= endLine; lineIdx++) {
    const rawLine = document.lineAt(lineIdx).text;
    const maskedLine = maskCommentsAndStringsPreservingColumns(rawLine, stripHashComments);
    identRegex.lastIndex = 0;

    let match: RegExpExecArray | null;
    while ((match = identRegex.exec(maskedLine)) !== null) {
      const name = match[0];
      if (STOP_KEYWORDS.has(name)) {
        continue;
      }
      // Skip pure screaming-numeric/hex-like noise while keeping meaningful identifiers.
      if (/^_[0-9_]+$/.test(name)) {
        continue;
      }

      const col = match.index;
      const inRange = isTokenInsideFocalRange(lineIdx, col, name.length, range);
      const afterToken = maskedLine.slice(col + name.length);
      const beforeChar = col > 0 ? maskedLine[col - 1] : '';

      let score = inRange ? 100 : 25;
      // Extra boost when cursor rests directly on or adjacent to this token.
      if (
        lineIdx === range.start.line &&
        range.start.character >= col &&
        range.start.character <= col + name.length
      ) {
        score += 50;
      }
      // Call site or generic type application (`foo(`, `foo::<`, `Foo<`)
      if (/^\s*(?:\(|::<|<[A-Za-z_])/.test(afterToken)) {
        score += 25;
      }
      // PascalCase type / trait / class / interface identifier
      if (/^[A-Z][a-zA-Z0-9_]*$/.test(name) && /[a-z]/.test(name)) {
        score += 20;
      }
      // Member / qualified symbol access (`.method`, `::item`)
      if (beforeChar === '.' || beforeChar === ':') {
        score += 10;
      }
      // Slight preference for closer lines to selection start
      score -= Math.min(15, Math.abs(lineIdx - range.start.line) * 2);

      const existing = bestBySymbol.get(name);
      if (!existing || score > existing.score) {
        bestBySymbol.set(name, {
          name,
          position: new vscode.Position(lineIdx, col),
          score,
        });
      }
    }
  }

  return Array.from(bestBySymbol.values())
    .sort((a, b) => b.score - a.score || a.position.line - b.position.line)
    .slice(0, maxSymbols);
}

function isTokenInsideFocalRange(
  lineIdx: number,
  col: number,
  length: number,
  range: vscode.Range
): boolean {
  if (lineIdx < range.start.line || lineIdx > range.end.line) {
    return false;
  }
  if (range.isEmpty) {
    return lineIdx === range.start.line;
  }
  if (lineIdx === range.start.line && col + length <= range.start.character) {
    return false;
  }
  if (lineIdx === range.end.line && col >= range.end.character) {
    return false;
  }
  return true;
}

/**
 * Replaces string literals and inline/trailing comments on a single line with spaces of the
 * exact same character width so regex `match.index` continues to match `vscode.Position` columns.
 */
function maskCommentsAndStringsPreservingColumns(
  line: string,
  stripHashComments: boolean
): string {
  const blank = (m: string): string => ' '.repeat(m.length);
  let out = line
    .replace(/\/\*.*?\*\//g, blank)
    .replace(/"(?:\\.|[^"\\])*"/g, blank)
    .replace(/'(?:\\.|[^'\\])*'/g, blank)
    .replace(/`(?:\\.|[^`$\\])*`/g, blank)
    .replace(/\/\/.*$/g, blank);

  if (stripHashComments) {
    out = out.replace(/#.*$/g, blank);
  }
  return out;
}

function extractHoverSignature(hovers: vscode.Hover[] | undefined): string | undefined {
  if (!hovers || hovers.length === 0) {
    return undefined;
  }

  const codeBlocks: string[] = [];
  const docLines: string[] = [];

  for (const hover of hovers) {
    if (!hover?.contents) {
      continue;
    }
    for (const part of hover.contents) {
      const md = markedPartToMarkdown(part).trim();
      if (!md) {
        continue;
      }

      const fenceRegex = /```[a-zA-Z0-9_+-]*\n([\s\S]*?)```/g;
      let fenceMatch: RegExpExecArray | null;
      let matchedFence = false;
      while ((fenceMatch = fenceRegex.exec(md)) !== null) {
        matchedFence = true;
        const cleaned = fenceMatch[1].trim();
        if (cleaned && !codeBlocks.includes(cleaned)) {
          codeBlocks.push(cleaned);
        }
      }

      const remainder = (matchedFence ? md.replace(/```[\s\S]*?```/g, '') : md)
        .replace(/\[[^\]]+\]\(command:[^)]+\)/g, '')
        .replace(/^---+$/gm, '')
        .trim();

      if (remainder) {
        const firstLine = remainder
          .split(/\r?\n/)
          .map((l) => l.trim())
          .find((l) => l.length > 0 && !l.startsWith('@'));
        if (firstLine && !docLines.includes(firstLine)) {
          docLines.push(firstLine);
        }
      }
    }
  }

  if (codeBlocks.length === 0 && docLines.length === 0) {
    return undefined;
  }

  const signaturePart = codeBlocks.join(' | ');
  const summaryPart = docLines[0] ? truncateSingleLine(docLines[0], 120) : '';
  const combined =
    signaturePart && summaryPart
      ? `${signaturePart} — ${summaryPart}`
      : signaturePart || summaryPart;

  return combined.length > MAX_HOVER_CHARS_PER_SYMBOL
    ? `${combined.slice(0, MAX_HOVER_CHARS_PER_SYMBOL - 1)}…`
    : combined;
}

function markedPartToMarkdown(part: vscode.MarkdownString | vscode.MarkedString): string {
  if (typeof part === 'string') {
    return part;
  }
  if ('language' in part && typeof part.language === 'string') {
    return `\`\`\`${part.language}\n${part.value}\n\`\`\``;
  }
  return part.value ?? '';
}

function formatInlineOrBlockSignature(signature: string): string {
  if (!signature.includes('\n')) {
    return `\`${signature.replace(/`/g, "'")}\``;
  }
  return `\n\`\`\`\n${signature}\n\`\`\``;
}

function normalizeWorkspaceLocations(
  rawLocations: ReadonlyArray<vscode.Location | vscode.LocationLink>
): NormalizedLocation[] {
  const out: NormalizedLocation[] = [];
  const hasWorkspaceFolders = (vscode.workspace.workspaceFolders?.length ?? 0) > 0;

  for (const item of rawLocations) {
    if (!item) {
      continue;
    }
    const uri = 'targetUri' in item ? item.targetUri : item.uri;
    const range =
      'targetUri' in item ? (item.targetSelectionRange ?? item.targetRange) : item.range;

    if (!uri || !range || uri.scheme !== 'file') {
      continue;
    }
    if (hasWorkspaceFolders && !vscode.workspace.getWorkspaceFolder(uri)) {
      continue;
    }
    const fsPathNormalized = uri.fsPath.replace(/\\/g, '/');
    if (
      fsPathNormalized.includes('/node_modules/') ||
      fsPathNormalized.includes('/.git/') ||
      fsPathNormalized.includes('/target/debug/') ||
      fsPathNormalized.includes('/target/release/')
    ) {
      continue;
    }
    out.push({ uri, range });
  }
  return out;
}

async function readWorkspaceFileLines(
  uri: vscode.Uri,
  deadline: number
): Promise<string[] | undefined> {
  const openDoc = vscode.workspace.textDocuments.find(
    (doc) => doc.uri.toString() === uri.toString()
  );
  if (openDoc) {
    return openDoc.getText().split(/\r?\n/);
  }

  if (Date.now() >= deadline) {
    return undefined;
  }

  const rawBytes = await executeBeforeDeadline<Uint8Array | undefined>(
    vscode.workspace.fs.readFile(uri),
    deadline,
    undefined
  );
  if (!rawBytes || rawBytes.byteLength > MAX_FILE_READ_BYTES) {
    return undefined;
  }
  // Skip binary buffers containing NUL bytes.
  if (rawBytes.subarray(0, Math.min(rawBytes.byteLength, 1024)).includes(0)) {
    return undefined;
  }
  return Buffer.from(rawBytes).toString('utf8').split(/\r?\n/);
}

function executeBeforeDeadline<T>(
  thenable: Thenable<T | undefined | null>,
  deadline: number,
  fallback: T
): Promise<T> {
  const remaining = deadline - Date.now();
  if (remaining <= 0) {
    return Promise.resolve(fallback);
  }

  return new Promise<T>((resolve) => {
    let settled = false;
    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        resolve(fallback);
      }
    }, remaining);

    Promise.resolve(thenable).then(
      (value) => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(value ?? fallback);
        }
      },
      () => {
        if (!settled) {
          settled = true;
          clearTimeout(timer);
          resolve(fallback);
        }
      }
    );
  });
}

function formatBoundedTagBlock(tagName: string, body: string, maxChars: number): string {
  const openTag = `<${tagName}>\n`;
  const closeTag = `\n</${tagName}>`;
  const full = `${openTag}${body}${closeTag}`;
  if (full.length <= maxChars) {
    return full;
  }

  const suffix = `\n... (truncated)${closeTag}`;
  const availableBodyChars = Math.max(0, maxChars - openTag.length - suffix.length);
  let sliced = body.slice(0, availableBodyChars);
  const lastNewline = sliced.lastIndexOf('\n');
  if (lastNewline > availableBodyChars * 0.6) {
    sliced = sliced.slice(0, lastNewline);
  }
  return `${openTag}${sliced}${suffix}`;
}

function toWorkspaceRelativePath(uri: vscode.Uri): string {
  return vscode.workspace.asRelativePath(uri, false);
}

function truncateSingleLine(text: string, maxLen: number): string {
  const single = text.replace(/\s+/g, ' ').trim();
  if (single.length <= maxLen) {
    return single;
  }
  return `${single.slice(0, Math.max(1, maxLen - 1))}…`;
}

// ============================================================================
// 2. Diagnostic Delta Engine (`DiagnosticSnapshot` & `DiagnosticSnapshotter`)
// ============================================================================

export type DiagnosticSeverityLabel = 'error' | 'warning' | 'info' | 'hint';

export interface FormattedDiagnostic {
  path: string;
  line: number;
  column: number;
  severity: DiagnosticSeverityLabel;
  message: string;
  source?: string;
  code?: string;
}

export interface DiagnosticSnapshot {
  readonly capturedAt: number;
  readonly scopeUri?: string;
  readonly scopePath?: string;
  readonly items: readonly FormattedDiagnostic[];
}

export interface DiagnosticDelta {
  introduced: FormattedDiagnostic[];
  resolved: FormattedDiagnostic[];
  summary: string;
}

/**
 * Captures an immutable snapshot of active Errors and Warnings either for a single `uri`
 * or across all workspace `file:` URIs when `uri` is omitted.
 */
export function captureDiagnostics(uri?: vscode.Uri): DiagnosticSnapshot {
  const items: FormattedDiagnostic[] = [];

  if (uri) {
    const relPath = toWorkspaceRelativePath(uri);
    for (const diag of vscode.languages.getDiagnostics(uri)) {
      const formatted = formatVscodeDiagnostic(relPath, diag);
      if (formatted) {
        items.push(formatted);
      }
    }
    sortDiagnostics(items);
    return {
      capturedAt: Date.now(),
      scopeUri: uri.toString(),
      scopePath: relPath,
      items,
    };
  }

  for (const [entryUri, diags] of vscode.languages.getDiagnostics()) {
    if (entryUri.scheme !== 'file') {
      continue;
    }
    const relPath = toWorkspaceRelativePath(entryUri);
    for (const diag of diags) {
      const formatted = formatVscodeDiagnostic(relPath, diag);
      if (formatted) {
        items.push(formatted);
      }
    }
  }

  sortDiagnostics(items);
  return {
    capturedAt: Date.now(),
    items,
  };
}

/**
 * Computes the semantic multiset diff between two diagnostic snapshots, tolerating line-number
 * shifts caused by intermediate edits so unchanged shifted diagnostics are not misreported.
 */
export function computeDiagnosticDelta(
  before: DiagnosticSnapshot,
  after: DiagnosticSnapshot
): DiagnosticDelta {
  const scopedPath = before.scopePath ?? after.scopePath;
  const beforeItems =
    scopedPath && !before.scopePath
      ? before.items.filter((d) => d.path === scopedPath)
      : before.items;
  const afterItems =
    scopedPath && !after.scopePath
      ? after.items.filter((d) => d.path === scopedPath)
      : after.items;

  const beforeGroups = groupBySemanticKey(beforeItems);
  const afterGroups = groupBySemanticKey(afterItems);
  const allKeys = new Set<string>([...beforeGroups.keys(), ...afterGroups.keys()]);

  const introduced: FormattedDiagnostic[] = [];
  const resolved: FormattedDiagnostic[] = [];

  for (const key of allKeys) {
    const bList = beforeGroups.get(key) ?? [];
    const aList = afterGroups.get(key) ?? [];

    if (bList.length === 0) {
      introduced.push(...aList);
      continue;
    }
    if (aList.length === 0) {
      resolved.push(...bList);
      continue;
    }

    // Pair occurrences with identical semantic identity, preferring exact line matches first,
    // then closest shifted line numbers.
    const usedBefore = new Array<boolean>(bList.length).fill(false);
    const usedAfter = new Array<boolean>(aList.length).fill(false);

    for (let ai = 0; ai < aList.length; ai++) {
      const exactIdx = bList.findIndex(
        (b, bi) => !usedBefore[bi] && b.line === aList[ai].line && b.column === aList[ai].column
      );
      if (exactIdx !== -1) {
        usedBefore[exactIdx] = true;
        usedAfter[ai] = true;
      }
    }

    for (let ai = 0; ai < aList.length; ai++) {
      if (usedAfter[ai]) {
        continue;
      }
      let bestBi = -1;
      let bestDist = Number.POSITIVE_INFINITY;
      for (let bi = 0; bi < bList.length; bi++) {
        if (usedBefore[bi]) {
          continue;
        }
        const dist = Math.abs(bList[bi].line - aList[ai].line);
        if (dist < bestDist) {
          bestDist = dist;
          bestBi = bi;
        }
      }
      if (bestBi !== -1) {
        usedBefore[bestBi] = true;
        usedAfter[ai] = true;
      }
    }

    for (let ai = 0; ai < aList.length; ai++) {
      if (!usedAfter[ai]) {
        introduced.push(aList[ai]);
      }
    }
    for (let bi = 0; bi < bList.length; bi++) {
      if (!usedBefore[bi]) {
        resolved.push(bList[bi]);
      }
    }
  }

  sortDiagnostics(introduced);
  sortDiagnostics(resolved);

  const summary = formatDiagnosticDeltaSummary(introduced, resolved, afterItems);
  return { introduced, resolved, summary };
}

/**
 * Waits briefly for `vscode.languages.onDidChangeDiagnostics` on `uri` (or resolves after
 * `waitMs ?? 220` ms if the language server stays quiet) and returns the diagnostic delta.
 */
export function waitForLspDiagnosticsDelta(
  uri: vscode.Uri,
  before: DiagnosticSnapshot,
  waitMs?: number
): Promise<DiagnosticDelta> {
  const timeoutMs = Math.max(0, waitMs ?? DEFAULT_DIAGNOSTIC_WAIT_MS);
  const targetUriStr = uri.toString();
  const targetFsPath = uri.fsPath;

  return new Promise<DiagnosticDelta>((resolve) => {
    let settled = false;
    let subscription: vscode.Disposable | undefined;

    const finish = (): void => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      subscription?.dispose();
      const after = before.scopeUri ? captureDiagnostics(uri) : captureDiagnostics();
      resolve(computeDiagnosticDelta(before, after));
    };

    const timer = setTimeout(finish, timeoutMs);

    subscription = vscode.languages.onDidChangeDiagnostics((event) => {
      const touched = event.uris.some(
        (changedUri) =>
          changedUri.toString() === targetUriStr || changedUri.fsPath === targetFsPath
      );
      if (touched) {
        finish();
      }
    });
  });
}

/**
 * Stateful/stateless facade for capturing LSP diagnostics and computing edit deltas.
 */
export class DiagnosticSnapshotter {
  captureDiagnostics(uri?: vscode.Uri): DiagnosticSnapshot {
    return captureDiagnostics(uri);
  }

  computeDiagnosticDelta(before: DiagnosticSnapshot, after: DiagnosticSnapshot): DiagnosticDelta {
    return computeDiagnosticDelta(before, after);
  }

  waitForLspDiagnosticsDelta(
    uri: vscode.Uri,
    before: DiagnosticSnapshot,
    waitMs?: number
  ): Promise<DiagnosticDelta> {
    return waitForLspDiagnosticsDelta(uri, before, waitMs);
  }

  static captureDiagnostics(uri?: vscode.Uri): DiagnosticSnapshot {
    return captureDiagnostics(uri);
  }

  static computeDiagnosticDelta(
    before: DiagnosticSnapshot,
    after: DiagnosticSnapshot
  ): DiagnosticDelta {
    return computeDiagnosticDelta(before, after);
  }

  static waitForLspDiagnosticsDelta(
    uri: vscode.Uri,
    before: DiagnosticSnapshot,
    waitMs?: number
  ): Promise<DiagnosticDelta> {
    return waitForLspDiagnosticsDelta(uri, before, waitMs);
  }
}

function formatVscodeDiagnostic(
  relPath: string,
  diag: vscode.Diagnostic
): FormattedDiagnostic | undefined {
  // Focus delta tracking on actionable compiler/linter Errors and Warnings.
  if (
    diag.severity !== vscode.DiagnosticSeverity.Error &&
    diag.severity !== vscode.DiagnosticSeverity.Warning
  ) {
    return undefined;
  }

  const severity: DiagnosticSeverityLabel =
    diag.severity === vscode.DiagnosticSeverity.Error ? 'error' : 'warning';
  const code = extractDiagnosticCode(diag.code);

  return {
    path: relPath,
    line: diag.range.start.line + 1,
    column: diag.range.start.character + 1,
    severity,
    message: diag.message.trim(),
    ...(diag.source ? { source: diag.source } : {}),
    ...(code ? { code } : {}),
  };
}

function extractDiagnosticCode(code: vscode.Diagnostic['code']): string | undefined {
  if (code === undefined) {
    return undefined;
  }
  if (typeof code === 'object' && code !== null) {
    return String(code.value);
  }
  return String(code);
}

function groupBySemanticKey(
  items: readonly FormattedDiagnostic[]
): Map<string, FormattedDiagnostic[]> {
  const map = new Map<string, FormattedDiagnostic[]>();
  for (const item of items) {
    const normalizedMsg = item.message.replace(/\s+/g, ' ').trim();
    const key = `${item.path}\0${item.severity}\0${item.source ?? ''}\0${item.code ?? ''}\0${normalizedMsg}`;
    const list = map.get(key);
    if (list) {
      list.push(item);
    } else {
      map.set(key, [item]);
    }
  }
  return map;
}

function sortDiagnostics(items: FormattedDiagnostic[]): void {
  const rank = (s: DiagnosticSeverityLabel): number =>
    s === 'error' ? 0 : s === 'warning' ? 1 : s === 'info' ? 2 : 3;
  items.sort(
    (a, b) =>
      rank(a.severity) - rank(b.severity) ||
      a.path.localeCompare(b.path) ||
      a.line - b.line ||
      a.column - b.column
  );
}

function formatDiagnosticDeltaSummary(
  introduced: readonly FormattedDiagnostic[],
  resolved: readonly FormattedDiagnostic[],
  activeAfter: readonly FormattedDiagnostic[]
): string {
  if (introduced.length === 0 && resolved.length === 0) {
    const activeErrors = activeAfter.filter((d) => d.severity === 'error').length;
    const activeWarnings = activeAfter.filter((d) => d.severity === 'warning').length;
    return `No diagnostic changes (${activeErrors} error(s), ${activeWarnings} warning(s) active).`;
  }

  const introErrors = introduced.filter((d) => d.severity === 'error').length;
  const introWarnings = introduced.length - introErrors;
  const headerParts: string[] = [];
  if (introduced.length > 0) {
    headerParts.push(
      `+${introduced.length} introduced (${introErrors} error(s), ${introWarnings} warning(s))`
    );
  } else {
    headerParts.push('+0 introduced');
  }
  headerParts.push(`-${resolved.length} resolved`);

  const lines: string[] = [`Diagnostics delta: ${headerParts.join(', ')}.`];
  for (const d of introduced.slice(0, 8)) {
    const tag = d.source || d.code ? ` (${[d.source, d.code].filter(Boolean).join(' ')})` : '';
    lines.push(
      `  + [${d.severity}] ${d.path}:${d.line}:${d.column}${tag}: ${truncateSingleLine(d.message, 160)}`
    );
  }
  if (introduced.length > 8) {
    lines.push(`  + ...and ${introduced.length - 8} more introduced diagnostic(s).`);
  }
  for (const d of resolved.slice(0, 5)) {
    const tag = d.source || d.code ? ` (${[d.source, d.code].filter(Boolean).join(' ')})` : '';
    lines.push(
      `  - [${d.severity}] ${d.path}:${d.line}:${d.column}${tag}: ${truncateSingleLine(d.message, 140)}`
    );
  }
  if (resolved.length > 5) {
    lines.push(`  - ...and ${resolved.length - 5} more resolved diagnostic(s).`);
  }
  return lines.join('\n');
}

// ============================================================================
// 3. Temporal Developer Intent Ledger (`TemporalIntentLedger`)
// ============================================================================

export type IntentActionKind = 'edit' | 'selection' | 'navigate' | 'save' | 'terminal';

export interface IntentEntry {
  kind: IntentActionKind;
  timestamp: number;
  path?: string;
  line?: number;
  endLine?: number;
  command?: string;
  exitCode?: number;
  preview?: string;
  editCount?: number;
}

/**
 * Tracks a bounded ring buffer of the developer's last 15 meaningful IDE actions
 * (edits coalesced per line, cross-file/long-jump navigations & selections, file saves,
 * and terminal shell command completions) to ground AI edits and agent turns in temporal intent.
 */
export class TemporalIntentLedger implements vscode.Disposable {
  private readonly buffer: Array<IntentEntry | undefined>;
  private readonly capacity: number;
  private head = 0;
  private count = 0;
  private lastCursorPath: string | undefined;
  private lastCursorLine: number | undefined;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(capacity = DEFAULT_INTENT_CAPACITY) {
    this.capacity = Math.max(1, capacity);
    this.buffer = new Array<IntentEntry | undefined>(this.capacity);

    this.disposables.push(
      vscode.window.onDidChangeTextEditorSelection((event) => {
        this.handleSelectionChange(event);
      }),
      vscode.workspace.onDidChangeTextDocument((event) => {
        this.handleDocumentChange(event);
      }),
      vscode.workspace.onDidSaveTextDocument((document) => {
        this.handleDocumentSave(document);
      })
    );

    if (
      'onDidEndTerminalShellExecution' in vscode.window &&
      typeof vscode.window.onDidEndTerminalShellExecution === 'function'
    ) {
      this.disposables.push(
        vscode.window.onDidEndTerminalShellExecution((event) => {
          this.handleTerminalExecutionEnd(event);
        })
      );
    }
  }

  /**
   * Formats the most recent developer actions (up to `maxItems`, default 15) as a compact,
   * chronological `<developer_intent>...</developer_intent>` block, or `''` if empty.
   */
  formatRecentIntent(maxItems?: number): string {
    const entries = this.getEntries(maxItems);
    if (entries.length === 0) {
      return '';
    }

    const now = Date.now();
    const lines = entries.map((entry) => {
      const age = formatRelativeAge(now - entry.timestamp);
      switch (entry.kind) {
        case 'edit': {
          const rangeLabel =
            entry.endLine && entry.endLine !== entry.line
              ? `${entry.line}-${entry.endLine}`
              : `${entry.line ?? 1}`;
          const coalesced =
            entry.editCount && entry.editCount > 1 ? ` (${entry.editCount} edits)` : '';
          const preview = entry.preview ? ` — ${entry.preview}` : '';
          return `- [${age}] edit ${entry.path}:${rangeLabel}${coalesced}${preview}`;
        }
        case 'selection': {
          const rangeLabel =
            entry.endLine && entry.endLine !== entry.line
              ? `${entry.line}-${entry.endLine}`
              : `${entry.line ?? 1}`;
          const preview = entry.preview ? ` — ${entry.preview}` : '';
          return `- [${age}] select ${entry.path}:${rangeLabel}${preview}`;
        }
        case 'navigate': {
          const preview = entry.preview ? ` — ${entry.preview}` : '';
          return `- [${age}] navigate ${entry.path}:${entry.line ?? 1}${preview}`;
        }
        case 'save': {
          const preview = entry.preview ? ` (${entry.preview})` : '';
          return `- [${age}] save ${entry.path}${preview}`;
        }
        case 'terminal': {
          const status =
            entry.exitCode === undefined
              ? 'finished'
              : entry.exitCode === 0
                ? 'exit 0'
                : `failed: exit ${entry.exitCode}`;
          return `- [${age}] terminal \`${entry.command ?? ''}\` (${status})`;
        }
      }
    });

    return `<developer_intent>\n${lines.join('\n')}\n</developer_intent>`;
  }

  /** Returns recorded entries in chronological order (oldest to newest). */
  getEntries(maxItems?: number): IntentEntry[] {
    if (this.count === 0) {
      return [];
    }
    const limit = Math.max(1, Math.min(this.count, maxItems ?? this.capacity));
    const startOffset = this.count - limit;
    const out: IntentEntry[] = [];
    for (let i = startOffset; i < this.count; i++) {
      const idx = (this.head + i) % this.capacity;
      const item = this.buffer[idx];
      if (item) {
        out.push({ ...item });
      }
    }
    return out;
  }

  clear(): void {
    this.buffer.fill(undefined);
    this.head = 0;
    this.count = 0;
    this.lastCursorPath = undefined;
    this.lastCursorLine = undefined;
  }

  dispose(): void {
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables.length = 0;
  }

  private handleSelectionChange(event: vscode.TextEditorSelectionChangeEvent): void {
    const doc = event.textEditor.document;
    if (doc.uri.scheme !== 'file' || event.selections.length === 0) {
      return;
    }

    const sel = event.selections[0];
    const relPath = toWorkspaceRelativePath(doc.uri);
    const activeLine1 = sel.active.line + 1;
    const now = Date.now();

    if (!sel.isEmpty) {
      const startLine1 = sel.start.line + 1;
      const endLine1 = sel.end.line + 1;
      const selectedSnippet = truncateSingleLine(doc.getText(sel), MAX_PREVIEW_CHARS);
      if (!selectedSnippet) {
        return;
      }

      const tail = this.peekTail();
      if (
        tail &&
        tail.kind === 'selection' &&
        tail.path === relPath &&
        now - tail.timestamp < 2500
      ) {
        tail.timestamp = now;
        tail.line = startLine1;
        tail.endLine = endLine1;
        tail.preview = selectedSnippet;
      } else {
        this.pushEntry({
          kind: 'selection',
          timestamp: now,
          path: relPath,
          line: startLine1,
          endLine: endLine1,
          preview: selectedSnippet,
        });
      }
      this.lastCursorPath = relPath;
      this.lastCursorLine = activeLine1;
      return;
    }

    // Empty caret movement: record only meaningful cross-file switches or >= 8 line jumps,
    // ignoring implicit caret motion right after typing on the same line.
    const tail = this.peekTail();
    if (
      tail &&
      tail.kind === 'edit' &&
      tail.path === relPath &&
      (tail.line === activeLine1 || tail.endLine === activeLine1)
    ) {
      this.lastCursorPath = relPath;
      this.lastCursorLine = activeLine1;
      return;
    }

    const switchedFile = this.lastCursorPath !== relPath;
    const jumpedFar =
      this.lastCursorLine === undefined ||
      Math.abs(activeLine1 - this.lastCursorLine) >= NAVIGATION_LINE_JUMP_THRESHOLD;

    if (!switchedFile && !jumpedFar) {
      return;
    }

    this.lastCursorPath = relPath;
    this.lastCursorLine = activeLine1;

    const lineText =
      sel.active.line < doc.lineCount
        ? truncateSingleLine(doc.lineAt(sel.active.line).text, MAX_PREVIEW_CHARS)
        : '';

    if (tail && tail.kind === 'navigate' && tail.path === relPath && now - tail.timestamp < 2000) {
      tail.timestamp = now;
      tail.line = activeLine1;
      tail.preview = lineText || undefined;
      return;
    }

    this.pushEntry({
      kind: 'navigate',
      timestamp: now,
      path: relPath,
      line: activeLine1,
      preview: lineText || undefined,
    });
  }

  private handleDocumentChange(event: vscode.TextDocumentChangeEvent): void {
    const doc = event.document;
    if (doc.uri.scheme !== 'file' || event.contentChanges.length === 0) {
      return;
    }

    const meaningfulChanges = event.contentChanges.filter(
      (c) => !(c.text.length === 0 && c.range.isEmpty)
    );
    if (meaningfulChanges.length === 0) {
      return;
    }

    const primary = meaningfulChanges[0];
    const relPath = toWorkspaceRelativePath(doc.uri);
    const startLine0 = primary.range.start.line;
    const addedLineBreaks = (primary.text.match(/\n/g) ?? []).length;
    const targetLine0 = Math.min(doc.lineCount - 1, Math.max(0, startLine0 + addedLineBreaks));
    const startLine1 = startLine0 + 1;
    const endLine1 = targetLine0 + 1;
    const preview =
      targetLine0 >= 0 && targetLine0 < doc.lineCount
        ? truncateSingleLine(doc.lineAt(targetLine0).text, MAX_PREVIEW_CHARS)
        : undefined;
    const now = Date.now();

    // Coalesce consecutive edits targeting the same line in the same file.
    const tail = this.peekTail();
    if (
      tail &&
      tail.kind === 'edit' &&
      tail.path === relPath &&
      (tail.line === startLine1 || tail.endLine === startLine1 || tail.line === endLine1)
    ) {
      tail.timestamp = now;
      tail.line = Math.min(tail.line ?? startLine1, startLine1);
      tail.endLine = endLine1;
      tail.preview = preview || tail.preview;
      tail.editCount = (tail.editCount ?? 1) + 1;
      this.lastCursorPath = relPath;
      this.lastCursorLine = endLine1;
      return;
    }

    this.pushEntry({
      kind: 'edit',
      timestamp: now,
      path: relPath,
      line: startLine1,
      endLine: endLine1,
      preview,
      editCount: 1,
    });
    this.lastCursorPath = relPath;
    this.lastCursorLine = endLine1;
  }

  private handleDocumentSave(document: vscode.TextDocument): void {
    if (document.uri.scheme !== 'file') {
      return;
    }

    const relPath = toWorkspaceRelativePath(document.uri);
    const errorCount = vscode.languages
      .getDiagnostics(document.uri)
      .filter((d) => d.severity === vscode.DiagnosticSeverity.Error).length;
    const statusLabel = errorCount === 0 ? '0 errors' : `${errorCount} error(s)`;
    const now = Date.now();

    const tail = this.peekTail();
    if (tail && tail.kind === 'save' && tail.path === relPath) {
      tail.timestamp = now;
      tail.preview = statusLabel;
      return;
    }

    this.pushEntry({
      kind: 'save',
      timestamp: now,
      path: relPath,
      preview: statusLabel,
    });
  }

  private handleTerminalExecutionEnd(event: vscode.TerminalShellExecutionEndEvent): void {
    const rawCommand = event.execution?.commandLine?.value?.trim();
    if (!rawCommand) {
      return;
    }

    this.pushEntry({
      kind: 'terminal',
      timestamp: Date.now(),
      command: truncateSingleLine(rawCommand, 140),
      exitCode: event.exitCode,
    });
  }

  private peekTail(): IntentEntry | undefined {
    if (this.count === 0) {
      return undefined;
    }
    const tailIdx = (this.head + this.count - 1) % this.capacity;
    return this.buffer[tailIdx];
  }

  private pushEntry(entry: IntentEntry): void {
    if (this.count < this.capacity) {
      const insertIdx = (this.head + this.count) % this.capacity;
      this.buffer[insertIdx] = entry;
      this.count++;
      return;
    }
    this.buffer[this.head] = entry;
    this.head = (this.head + 1) % this.capacity;
  }
}

function formatRelativeAge(deltaMs: number): string {
  const secs = Math.max(0, Math.round(deltaMs / 1000));
  if (secs < 60) {
    return `${secs}s ago`;
  }
  const mins = Math.round(secs / 60);
  if (mins < 60) {
    return `${mins}m ago`;
  }
  const hours = Math.round(mins / 60);
  return `${hours}h ago`;
}
