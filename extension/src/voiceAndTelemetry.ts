import { ChildProcess, execFileSync, spawn } from 'node:child_process';
import * as fs from 'node:fs';
import * as fsp from 'node:fs/promises';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient, ToolEventPayload, ToolRequestPayload } from './sidecarClient';

const MAX_TELEMETRY_HISTORY = 500;
const MAX_VOICE_DURATION_MS = 120_000;
const MIN_VALID_WAV_BYTES = 128;

export type TelemetryMode =
  | 'autocomplete'
  | 'nes'
  | 'inlineEdit'
  | 'agent'
  | 'ask'
  | 'voice'
  | (string & {});

export interface TurnTelemetryMetric {
  mode: TelemetryMode;
  model: string;
  ttftMs: number;
  totalMs: number;
  outputChars: number;
  estInputTokens: number;
  estOutputTokens: number;
  cachedTokens: number;
  estCostUsd: number;
  timestamp: number;
}

export interface TurnTelemetryInput {
  mode: TelemetryMode;
  model: string;
  ttftMs: number;
  totalMs: number;
  outputChars: number;
  inputChars?: number;
  estInputTokens?: number;
  estOutputTokens?: number;
  cachedTokens?: number;
  estCostUsd?: number;
  timestamp?: number;
}

export interface ModeTelemetryBreakdown {
  count: number;
  p50TtftMs: number;
  p95TtftMs: number;
  avgTokPerSec: number;
  cachedTokens: number;
  costUsd: number;
}

export interface TelemetrySessionSummary {
  turnCount: number;
  p50TtftMs: number;
  p95TtftMs: number;
  p50TotalMs: number;
  p95TotalMs: number;
  avgTokensPerSec: number;
  totalInputTokens: number;
  totalOutputTokens: number;
  totalCachedTokens: number;
  cacheHitRatePct: number;
  promptCacheSavingsUsd: number;
  cumulativeCostUsd: number;
  byMode: Record<string, ModeTelemetryBreakdown>;
}

export interface ActiveTurnTimer {
  markFirstToken(): void;
  finish(
    outputChars: number,
    overrides?: Partial<
      Pick<TurnTelemetryMetric, 'estInputTokens' | 'estOutputTokens' | 'cachedTokens' | 'estCostUsd'>
    >
  ): TurnTelemetryMetric;
}

interface ModelPricing {
  inputPerMillion: number;
  cachedInputPerMillion: number;
  outputPerMillion: number;
}

const MODEL_PRICING_TABLE: Array<{ match: RegExp; pricing: ModelPricing }> = [
  {
    match: /ollama|mlx|local|qwen|llama|codellama|deepseek-coder|starcoder/i,
    pricing: { inputPerMillion: 0, cachedInputPerMillion: 0, outputPerMillion: 0 },
  },
  {
    match: /gemini-2\.5-flash-lite|flash-lite|gemini-2\.0-flash-lite/i,
    pricing: { inputPerMillion: 0.1, cachedInputPerMillion: 0.025, outputPerMillion: 0.4 },
  },
  {
    match: /gemini-2\.5-pro|gemini-1\.5-pro|gemini-exp/i,
    pricing: { inputPerMillion: 1.25, cachedInputPerMillion: 0.3125, outputPerMillion: 10.0 },
  },
  {
    match: /gemini/i,
    pricing: { inputPerMillion: 0.3, cachedInputPerMillion: 0.075, outputPerMillion: 2.5 },
  },
  {
    match: /claude-.*opus/i,
    pricing: { inputPerMillion: 15.0, cachedInputPerMillion: 1.5, outputPerMillion: 75.0 },
  },
  {
    match: /claude/i,
    pricing: { inputPerMillion: 3.0, cachedInputPerMillion: 0.3, outputPerMillion: 15.0 },
  },
  {
    match: /gpt-4o-mini|o3-mini|o4-mini/i,
    pricing: { inputPerMillion: 0.15, cachedInputPerMillion: 0.075, outputPerMillion: 0.6 },
  },
  {
    match: /gpt-4|o1|o3/i,
    pricing: { inputPerMillion: 2.5, cachedInputPerMillion: 1.25, outputPerMillion: 10.0 },
  },
];

function resolveModelPricing(model: string): ModelPricing {
  for (const entry of MODEL_PRICING_TABLE) {
    if (entry.match.test(model)) {
      return entry.pricing;
    }
  }
  return { inputPerMillion: 0.3, cachedInputPerMillion: 0.075, outputPerMillion: 2.5 };
}

/**
 * Estimates USD cost for a single LLM turn, accounting for discounted cached input tokens.
 */
export function estimateTurnCostUsd(
  model: string,
  estInputTokens: number,
  estOutputTokens: number,
  cachedTokens = 0
): number {
  const pricing = resolveModelPricing(model);
  const uncachedInput = Math.max(0, estInputTokens - cachedTokens);
  const billableCached = Math.max(0, cachedTokens);
  const billableOutput = Math.max(0, estOutputTokens);

  const inputCost = (uncachedInput / 1_000_000) * pricing.inputPerMillion;
  const cachedCost = (billableCached / 1_000_000) * pricing.cachedInputPerMillion;
  const outputCost = (billableOutput / 1_000_000) * pricing.outputPerMillion;
  return inputCost + cachedCost + outputCost;
}

/**
 * Estimates USD saved by serving `cachedTokens` from prompt cache rather than uncached input.
 */
export function estimateCacheSavingsUsd(model: string, cachedTokens: number): number {
  if (cachedTokens <= 0) {
    return 0;
  }
  const pricing = resolveModelPricing(model);
  const deltaPerMillion = Math.max(0, pricing.inputPerMillion - pricing.cachedInputPerMillion);
  return (cachedTokens / 1_000_000) * deltaPerMillion;
}

export function computePercentile(values: readonly number[], percentile: number): number {
  if (values.length === 0) {
    return 0;
  }
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil((percentile / 100) * sorted.length) - 1)
  );
  return sorted[rank];
}

function computeTurnTokPerSec(turn: TurnTelemetryMetric): number {
  if (turn.estOutputTokens <= 0) {
    return 0;
  }
  const decodeWindowMs =
    turn.totalMs > turn.ttftMs + 15 ? turn.totalMs - turn.ttftMs : Math.max(turn.totalMs, 1);
  return (turn.estOutputTokens / decodeWindowMs) * 1000;
}

function formatUsdCompact(usd: number): string {
  if (usd <= 0) {
    return '$0.000';
  }
  if (usd < 0.001) {
    return `$${usd.toFixed(4)}`;
  }
  if (usd < 1) {
    return `$${usd.toFixed(3)}`;
  }
  return `$${usd.toFixed(2)}`;
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

interface TelemetryQuickPickItem extends vscode.QuickPickItem {
  action?: 'openDashboard' | 'openOutput' | 'reset';
}

/**
 * Tracks per-turn latency (TTFT & total), token throughput, prompt cache savings,
 * and estimated session cost with a live VS Code status bar HUD (`rursor.showTelemetry`).
 */
export class TelemetryTracker implements vscode.Disposable {
  private readonly turns: TurnTelemetryMetric[] = [];
  private readonly statusBarItem: vscode.StatusBarItem;
  private readonly outputChannel: vscode.OutputChannel;
  private dashboardPanel: vscode.WebviewPanel | undefined;
  private readonly onDidRecordTurnEmitter = new vscode.EventEmitter<TurnTelemetryMetric>();
  public readonly onDidRecordTurn = this.onDidRecordTurnEmitter.event;

  constructor(priority = 98) {
    this.statusBarItem = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Right,
      priority
    );
    this.statusBarItem.command = 'rursor.showTelemetry';
    this.outputChannel = vscode.window.createOutputChannel('Rursor Telemetry');
    this.refreshStatusBar();
    this.statusBarItem.show();
  }

  /**
   * Starts a live timer for an in-flight LLM request and returns helpers to mark TTFT and completion.
   */
  public startTurn(
    mode: TelemetryMode,
    model: string,
    estInputTokens = 0,
    cachedTokens = 0
  ): ActiveTurnTimer {
    const startedAt = performance.now();
    let firstTokenAt: number | undefined;

    return {
      markFirstToken: () => {
        if (firstTokenAt === undefined) {
          firstTokenAt = performance.now();
        }
      },
      finish: (outputChars, overrides) => {
        const endedAt = performance.now();
        const totalMs = Math.max(1, Math.round(endedAt - startedAt));
        const ttftMs =
          firstTokenAt !== undefined
            ? Math.max(1, Math.round(firstTokenAt - startedAt))
            : totalMs;
        return this.recordTurn({
          mode,
          model,
          ttftMs,
          totalMs,
          outputChars,
          estInputTokens: overrides?.estInputTokens ?? estInputTokens,
          estOutputTokens: overrides?.estOutputTokens,
          cachedTokens: overrides?.cachedTokens ?? cachedTokens,
          estCostUsd: overrides?.estCostUsd,
        });
      },
    };
  }

  /**
   * Records a completed turn metric and updates the status bar item + open HUD views.
   */
  public recordTurn(input: TurnTelemetryInput): TurnTelemetryMetric {
    const outputChars = Math.max(0, Math.round(input.outputChars));
    const estOutputTokens =
      input.estOutputTokens !== undefined
        ? Math.max(0, Math.round(input.estOutputTokens))
        : Math.ceil(outputChars / 4);
    const estInputTokens =
      input.estInputTokens !== undefined
        ? Math.max(0, Math.round(input.estInputTokens))
        : Math.ceil(Math.max(0, input.inputChars ?? 0) / 4);
    const cachedTokens = Math.max(0, Math.round(input.cachedTokens ?? 0));
    const ttftMs = Math.max(0, Math.round(input.ttftMs));
    const totalMs = Math.max(ttftMs, Math.round(input.totalMs));
    const model = input.model.trim() || 'gemini-3.8-flash';
    const estCostUsd =
      input.estCostUsd !== undefined
        ? Math.max(0, input.estCostUsd)
        : estimateTurnCostUsd(model, estInputTokens, estOutputTokens, cachedTokens);

    const metric: TurnTelemetryMetric = {
      mode: input.mode,
      model,
      ttftMs,
      totalMs,
      outputChars,
      estInputTokens,
      estOutputTokens,
      cachedTokens,
      estCostUsd,
      timestamp: input.timestamp ?? Date.now(),
    };

    this.turns.push(metric);
    if (this.turns.length > MAX_TELEMETRY_HISTORY) {
      this.turns.splice(0, this.turns.length - MAX_TELEMETRY_HISTORY);
    }

    this.refreshStatusBar();
    this.refreshDashboardWebview();
    this.onDidRecordTurnEmitter.fire(metric);
    return metric;
  }

  public getTurns(): readonly TurnTelemetryMetric[] {
    return this.turns;
  }

  public clear(): void {
    this.turns.length = 0;
    this.refreshStatusBar();
    this.refreshDashboardWebview();
  }

  public getSummary(): TelemetrySessionSummary {
    if (this.turns.length === 0) {
      return {
        turnCount: 0,
        p50TtftMs: 0,
        p95TtftMs: 0,
        p50TotalMs: 0,
        p95TotalMs: 0,
        avgTokensPerSec: 0,
        totalInputTokens: 0,
        totalOutputTokens: 0,
        totalCachedTokens: 0,
        cacheHitRatePct: 0,
        promptCacheSavingsUsd: 0,
        cumulativeCostUsd: 0,
        byMode: {},
      };
    }

    const ttfts = this.turns.map((t) => t.ttftMs);
    const totals = this.turns.map((t) => t.totalMs);

    let totalInputTokens = 0;
    let totalOutputTokens = 0;
    let totalCachedTokens = 0;
    let promptCacheSavingsUsd = 0;
    let cumulativeCostUsd = 0;
    const tokRates: number[] = [];
    const grouped = new Map<string, TurnTelemetryMetric[]>();

    for (const turn of this.turns) {
      totalInputTokens += turn.estInputTokens;
      totalOutputTokens += turn.estOutputTokens;
      totalCachedTokens += turn.cachedTokens;
      promptCacheSavingsUsd += estimateCacheSavingsUsd(turn.model, turn.cachedTokens);
      cumulativeCostUsd += turn.estCostUsd;

      const rate = computeTurnTokPerSec(turn);
      if (rate > 0) {
        tokRates.push(rate);
      }

      const bucket = grouped.get(turn.mode) ?? [];
      bucket.push(turn);
      grouped.set(turn.mode, bucket);
    }

    const avgTokensPerSec =
      tokRates.length > 0 ? tokRates.reduce((acc, r) => acc + r, 0) / tokRates.length : 0;
    const cacheDenominator = Math.max(1, totalInputTokens);
    const cacheHitRatePct = Math.min(100, (totalCachedTokens / cacheDenominator) * 100);

    const byMode: Record<string, ModeTelemetryBreakdown> = {};
    for (const [mode, modeTurns] of grouped.entries()) {
      const modeTtfts = modeTurns.map((t) => t.ttftMs);
      const modeRates = modeTurns.map(computeTurnTokPerSec).filter((r) => r > 0);
      byMode[mode] = {
        count: modeTurns.length,
        p50TtftMs: computePercentile(modeTtfts, 50),
        p95TtftMs: computePercentile(modeTtfts, 95),
        avgTokPerSec:
          modeRates.length > 0 ? modeRates.reduce((a, b) => a + b, 0) / modeRates.length : 0,
        cachedTokens: modeTurns.reduce((a, t) => a + t.cachedTokens, 0),
        costUsd: modeTurns.reduce((a, t) => a + t.estCostUsd, 0),
      };
    }

    return {
      turnCount: this.turns.length,
      p50TtftMs: computePercentile(ttfts, 50),
      p95TtftMs: computePercentile(ttfts, 95),
      p50TotalMs: computePercentile(totals, 50),
      p95TotalMs: computePercentile(totals, 95),
      avgTokensPerSec,
      totalInputTokens,
      totalOutputTokens,
      totalCachedTokens,
      cacheHitRatePct,
      promptCacheSavingsUsd,
      cumulativeCostUsd,
      byMode,
    };
  }

  private refreshStatusBar(): void {
    const summary = this.getSummary();
    const latest = this.turns[this.turns.length - 1];

    if (!latest) {
      this.statusBarItem.text = '$(pulse) —ms · $0.000';
    } else {
      this.statusBarItem.text = `$(pulse) ${Math.round(latest.ttftMs)}ms · ${formatUsdCompact(
        summary.cumulativeCostUsd
      )}`;
    }

    const md = new vscode.MarkdownString(undefined, true);
    md.isTrusted = true;
    md.appendMarkdown('**Rursor Session Telemetry & Cache HUD**\n\n');
    if (summary.turnCount === 0) {
      md.appendMarkdown('_No AI turns recorded in this session yet._\n\n');
    } else {
      md.appendMarkdown(
        `- **TTFT (p50 / p95):** \`${summary.p50TtftMs}ms\` / \`${summary.p95TtftMs}ms\`\n`
      );
      md.appendMarkdown(
        `- **Avg Throughput:** \`${summary.avgTokensPerSec.toFixed(1)} tok/s\`\n`
      );
      md.appendMarkdown(
        `- **Prompt Cache:** \`${summary.totalCachedTokens.toLocaleString()} tok\` (\`${summary.cacheHitRatePct.toFixed(
          1
        )}%\`, saved \`${formatUsdCompact(summary.promptCacheSavingsUsd)}\`)\n`
      );
      md.appendMarkdown(
        `- **Cumulative Cost:** \`${formatUsdCompact(summary.cumulativeCostUsd)}\` across \`${
          summary.turnCount
        }\` turns\n\n`
      );
    }
    md.appendMarkdown('_Click to inspect full breakdown_');
    this.statusBarItem.tooltip = md;
  }

  /**
   * Opens the interactive QuickPick breakdown with options to launch the Webview HUD,
   * write a detailed report to the Output channel, or reset session metrics.
   */
  public async showTelemetry(): Promise<void> {
    const summary = this.getSummary();
    const items: TelemetryQuickPickItem[] = [
      {
        label: '$(graph) Session Latency (TTFT)',
        description:
          summary.turnCount > 0
            ? `p50: ${summary.p50TtftMs}ms · p95: ${summary.p95TtftMs}ms`
            : 'No turns recorded yet',
        detail:
          summary.turnCount > 0
            ? `End-to-end total latency — p50: ${summary.p50TotalMs}ms · p95: ${summary.p95TotalMs}ms (${summary.turnCount} turns)`
            : 'Run Tab Autocomplete, Cmd+K Inline Edit, Cmd+L Agent, or Voice-to-Code to populate metrics.',
      },
      {
        label: '$(zap) Streaming Throughput',
        description: `${summary.avgTokensPerSec.toFixed(1)} tok/s avg`,
        detail: `Input: ${summary.totalInputTokens.toLocaleString()} tok · Output: ${summary.totalOutputTokens.toLocaleString()} tok`,
      },
      {
        label: '$(database) Prompt Cache Savings',
        description: `${formatUsdCompact(summary.promptCacheSavingsUsd)} saved (${summary.cacheHitRatePct.toFixed(
          1
        )}% hit rate)`,
        detail: `${summary.totalCachedTokens.toLocaleString()} cached input tokens reused across turns`,
      },
      {
        label: '$(credit-card) Cumulative Session Cost',
        description: formatUsdCompact(summary.cumulativeCostUsd),
        detail: Object.entries(summary.byMode)
          .map(([mode, m]) => `${mode}: ${m.count}x (${m.p50TtftMs}ms p50, ${formatUsdCompact(m.costUsd)})`)
          .join('  ·  ') || 'Zero billable usage so far',
      },
      {
        label: 'Actions',
        kind: vscode.QuickPickItemKind.Separator,
      },
      {
        label: '$(dashboard) Open Live Telemetry Dashboard (Webview)',
        description: 'Interactive charts & per-turn table',
        action: 'openDashboard',
      },
      {
        label: '$(output) Print Full Breakdown to Output Channel',
        description: 'Rursor Telemetry log',
        action: 'openOutput',
      },
      {
        label: '$(clear-all) Reset Session Telemetry',
        description: 'Clear recorded turns',
        action: 'reset',
      },
    ];

    if (this.turns.length > 0) {
      items.push({
        label: 'Recent Turns',
        kind: vscode.QuickPickItemKind.Separator,
      });
      const recent = [...this.turns].reverse().slice(0, 10);
      for (const turn of recent) {
        const timeStr = new Date(turn.timestamp).toLocaleTimeString();
        const rate = computeTurnTokPerSec(turn);
        items.push({
          label: `$(history) [${turn.mode}] ${turn.model}`,
          description: `TTFT ${turn.ttftMs}ms · Total ${turn.totalMs}ms · ${rate.toFixed(1)} tok/s · ${formatUsdCompact(
            turn.estCostUsd
          )}`,
          detail: `${timeStr} — in: ${turn.estInputTokens} tok (cached: ${turn.cachedTokens}) · out: ${turn.estOutputTokens} tok (${turn.outputChars} chars)`,
          action: 'openDashboard',
        });
      }
    }

    const picked = await vscode.window.showQuickPick(items, {
      title: 'Rursor Telemetry & Prompt Cache HUD',
      placeHolder: 'Inspect session latency, token throughput, cache savings, and cost',
      matchOnDescription: true,
      matchOnDetail: true,
    });

    if (!picked?.action) {
      return;
    }

    if (picked.action === 'openDashboard') {
      this.openDashboardWebview();
    } else if (picked.action === 'openOutput') {
      this.writeReportToOutput();
      this.outputChannel.show(true);
    } else if (picked.action === 'reset') {
      this.clear();
      vscode.window.showInformationMessage('Rursor session telemetry reset.');
    }
  }

  public writeReportToOutput(): void {
    const s = this.getSummary();
    this.outputChannel.clear();
    this.outputChannel.appendLine('==============================================================');
    this.outputChannel.appendLine('  RURSOR SESSION TELEMETRY & PROMPT CACHE REPORT');
    this.outputChannel.appendLine('==============================================================');
    this.outputChannel.appendLine(`Total Turns          : ${s.turnCount}`);
    this.outputChannel.appendLine(`TTFT (p50 / p95)     : ${s.p50TtftMs} ms / ${s.p95TtftMs} ms`);
    this.outputChannel.appendLine(`Total (p50 / p95)    : ${s.p50TotalMs} ms / ${s.p95TotalMs} ms`);
    this.outputChannel.appendLine(`Avg Throughput       : ${s.avgTokensPerSec.toFixed(1)} tok/s`);
    this.outputChannel.appendLine(
      `Tokens (In/Out/Cache): ${s.totalInputTokens} / ${s.totalOutputTokens} / ${s.totalCachedTokens}`
    );
    this.outputChannel.appendLine(
      `Cache Savings        : ${formatUsdCompact(s.promptCacheSavingsUsd)} (${s.cacheHitRatePct.toFixed(1)}% hit)`
    );
    this.outputChannel.appendLine(`Cumulative Cost      : ${formatUsdCompact(s.cumulativeCostUsd)}`);
    this.outputChannel.appendLine('--------------------------------------------------------------');

    for (const [mode, m] of Object.entries(s.byMode)) {
      this.outputChannel.appendLine(
        `  [${mode.padEnd(12)}] turns=${String(m.count).padStart(3)} | p50=${String(
          m.p50TtftMs
        ).padStart(5)}ms | p95=${String(m.p95TtftMs).padStart(5)}ms | ${m.avgTokPerSec
          .toFixed(1)
          .padStart(6)} tok/s | ${formatUsdCompact(m.costUsd)}`
      );
    }
    this.outputChannel.appendLine('==============================================================');
  }

  public openDashboardWebview(): void {
    if (this.dashboardPanel) {
      this.dashboardPanel.reveal(vscode.ViewColumn.Beside);
      this.refreshDashboardWebview();
      return;
    }

    const panel = vscode.window.createWebviewPanel(
      'rursor.telemetryHud',
      'Rursor Telemetry HUD',
      vscode.ViewColumn.Beside,
      { enableScripts: true }
    );
    this.dashboardPanel = panel;

    panel.webview.onDidReceiveMessage((msg: { type?: string }) => {
      if (msg.type === 'reset') {
        this.clear();
      } else if (msg.type === 'exportOutput') {
        this.writeReportToOutput();
        this.outputChannel.show(true);
      }
    });

    panel.onDidDispose(() => {
      this.dashboardPanel = undefined;
    });

    this.refreshDashboardWebview();
  }

  private refreshDashboardWebview(): void {
    if (!this.dashboardPanel) {
      return;
    }
    const s = this.getSummary();
    const modeRows = Object.entries(s.byMode)
      .map(
        ([mode, m]) => `
        <tr>
          <td><code>${escapeHtml(mode)}</code></td>
          <td>${m.count}</td>
          <td>${m.p50TtftMs} ms</td>
          <td>${m.p95TtftMs} ms</td>
          <td>${m.avgTokPerSec.toFixed(1)} tok/s</td>
          <td>${m.cachedTokens.toLocaleString()}</td>
          <td>${escapeHtml(formatUsdCompact(m.costUsd))}</td>
        </tr>`
      )
      .join('');

    const recentRows = [...this.turns]
      .reverse()
      .slice(0, 30)
      .map((t) => {
        const rate = computeTurnTokPerSec(t);
        return `
        <tr>
          <td>${escapeHtml(new Date(t.timestamp).toLocaleTimeString())}</td>
          <td><code>${escapeHtml(t.mode)}</code></td>
          <td>${escapeHtml(t.model)}</td>
          <td>${t.ttftMs} ms</td>
          <td>${t.totalMs} ms</td>
          <td>${rate.toFixed(1)}</td>
          <td>${t.estInputTokens} / ${t.estOutputTokens}</td>
          <td>${t.cachedTokens}</td>
          <td>${escapeHtml(formatUsdCompact(t.estCostUsd))}</td>
        </tr>`;
      })
      .join('');

    this.dashboardPanel.webview.html = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <style>
    body {
      font-family: var(--vscode-font-family);
      color: var(--vscode-foreground);
      background: var(--vscode-editor-background);
      padding: 18px 22px;
      margin: 0;
    }
    .header {
      display: flex;
      justify-content: space-between;
      align-items: center;
      margin-bottom: 16px;
    }
    h2 { margin: 0; font-size: 16px; }
    .actions { display: flex; gap: 8px; }
    button {
      background: var(--vscode-button-secondaryBackground);
      color: var(--vscode-button-secondaryForeground);
      border: 1px solid var(--vscode-widget-border, transparent);
      border-radius: 4px;
      padding: 5px 10px;
      cursor: pointer;
      font-size: 12px;
    }
    button:hover { background: var(--vscode-button-secondaryHoverBackground); }
    .cards {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(175px, 1fr));
      gap: 12px;
      margin-bottom: 20px;
    }
    .card {
      background: var(--vscode-editorWidget-background);
      border: 1px solid var(--vscode-widget-border, rgba(128,128,128,0.25));
      border-radius: 6px;
      padding: 12px 14px;
    }
    .card-label {
      font-size: 11px;
      text-transform: uppercase;
      opacity: 0.75;
      margin-bottom: 6px;
    }
    .card-value {
      font-size: 20px;
      font-weight: 600;
    }
    .card-sub {
      font-size: 11px;
      opacity: 0.75;
      margin-top: 4px;
    }
    h3 { font-size: 13px; margin: 18px 0 8px; }
    table {
      width: 100%;
      border-collapse: collapse;
      font-size: 12px;
    }
    th, td {
      text-align: left;
      padding: 6px 8px;
      border-bottom: 1px solid var(--vscode-widget-border, rgba(128,128,128,0.2));
    }
    th { opacity: 0.75; font-weight: 600; }
    .empty { opacity: 0.7; font-style: italic; padding: 12px 0; }
  </style>
</head>
<body>
  <div class="header">
    <h2>Rursor Session Telemetry &amp; Cache HUD</h2>
    <div class="actions">
      <button onclick="vscode.postMessage({ type: 'exportOutput' })">Export to Output</button>
      <button onclick="vscode.postMessage({ type: 'reset' })">Reset Metrics</button>
    </div>
  </div>
  <div class="cards">
    <div class="card">
      <div class="card-label">TTFT Latency</div>
      <div class="card-value">${s.p50TtftMs}ms</div>
      <div class="card-sub">p50 · p95: ${s.p95TtftMs}ms (${s.turnCount} turns)</div>
    </div>
    <div class="card">
      <div class="card-label">Avg Throughput</div>
      <div class="card-value">${s.avgTokensPerSec.toFixed(1)} tok/s</div>
      <div class="card-sub">Total p50: ${s.p50TotalMs}ms · p95: ${s.p95TotalMs}ms</div>
    </div>
    <div class="card">
      <div class="card-label">Prompt Cache Saved</div>
      <div class="card-value">${escapeHtml(formatUsdCompact(s.promptCacheSavingsUsd))}</div>
      <div class="card-sub">${s.totalCachedTokens.toLocaleString()} cached tok (${s.cacheHitRatePct.toFixed(
      1
    )}%)</div>
    </div>
    <div class="card">
      <div class="card-label">Cumulative Cost</div>
      <div class="card-value">${escapeHtml(formatUsdCompact(s.cumulativeCostUsd))}</div>
      <div class="card-sub">In: ${s.totalInputTokens.toLocaleString()} · Out: ${s.totalOutputTokens.toLocaleString()}</div>
    </div>
  </div>

  <h3>Breakdown by Mode</h3>
  ${
    modeRows
      ? `<table>
          <thead>
            <tr><th>Mode</th><th>Turns</th><th>p50 TTFT</th><th>p95 TTFT</th><th>Throughput</th><th>Cached Tok</th><th>Est. Cost</th></tr>
          </thead>
          <tbody>${modeRows}</tbody>
        </table>`
      : '<div class="empty">No turns recorded yet.</div>'
  }

  <h3>Recent Turns</h3>
  ${
    recentRows
      ? `<table>
          <thead>
            <tr><th>Time</th><th>Mode</th><th>Model</th><th>TTFT</th><th>Total</th><th>tok/s</th><th>In / Out</th><th>Cached</th><th>Cost</th></tr>
          </thead>
          <tbody>${recentRows}</tbody>
        </table>`
      : '<div class="empty">Trigger an AI action to view live per-turn telemetry.</div>'
  }

  <script>
    const vscode = acquireVsCodeApi();
  </script>
</body>
</html>`;
  }

  public dispose(): void {
    this.dashboardPanel?.dispose();
    this.statusBarItem.dispose();
    this.outputChannel.dispose();
    this.onDidRecordTurnEmitter.dispose();
  }
}

export interface AudioInlineData {
  mimeType: 'audio/wav';
  data: string;
}

export interface SpatialEditorContext {
  filePath?: string;
  languageId?: string;
  startLine?: number;
  endLine?: number;
  rangeLabel?: string;
  selectedCode?: string;
  hoveredSymbol?: string;
  enclosingSymbol?: string;
  diagnosticsSummary?: string;
}

export interface VoicePromptPayload {
  inlineData: AudioInlineData;
  spatialContext: SpatialEditorContext;
  formattedPrompt: string;
  durationMs: number;
}

export type VoiceDispatchHandler = (payload: VoicePromptPayload) => Promise<void>;

type RecorderBackendKind = 'afrecord' | 'rec' | 'sox' | 'ffmpeg' | 'swift' | 'osascript';

interface DetectedRecorder {
  kind: RecorderBackendKind;
  binaryPath: string;
}

interface ActiveRecordingSession {
  wavPath: string;
  helperScriptPath?: string;
  proc: ChildProcess;
  recorder: DetectedRecorder;
  startedAt: number;
  spatialContext: SpatialEditorContext;
  timeoutHandle: ReturnType<typeof setTimeout>;
  cancelled: boolean;
  stopping: boolean;
}

const SWIFT_AV_AUDIO_RECORDER_SCRIPT = `import Foundation
import AVFoundation

let outPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "/tmp/rursor_voice.wav"
let url = URL(fileURLWithPath: outPath)
let settings: [String: Any] = [
    AVFormatIDKey: Int(kAudioFormatLinearPCM),
    AVSampleRateKey: 16000.0,
    AVNumberOfChannelsKey: 1,
    AVLinearPCMBitDepthKey: 16,
    AVLinearPCMIsBigEndianKey: false,
    AVLinearPCMIsFloatKey: false
]
let recorder = try AVAudioRecorder(url: url, settings: settings)
recorder.prepareToRecord()
guard recorder.record() else {
    fputs("Failed to start macOS AVAudioRecorder\\n", stderr)
    exit(1)
}
signal(SIGINT, SIG_IGN)
signal(SIGTERM, SIG_IGN)
let sigInt = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
let sigTerm = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
let finish = {
    recorder.stop()
    exit(0)
}
sigInt.setEventHandler(handler: finish)
sigTerm.setEventHandler(handler: finish)
sigInt.resume()
sigTerm.resume()
dispatchMain()
`;

const JXA_AV_AUDIO_RECORDER_SCRIPT = `ObjC.import('Foundation');
ObjC.import('AVFoundation');
const args = $.NSProcessInfo.processInfo.arguments;
const outPath = args.count > 4 ? ObjC.unwrap(args.objectAtIndex(4)) : '/tmp/rursor_voice.wav';
const url = $.NSURL.fileURLWithPath(outPath);
const settings = $({
  AVFormatIDKey: 1819304813,
  AVSampleRateKey: 16000.0,
  AVNumberOfChannelsKey: 1,
  AVLinearPCMBitDepthKey: 16,
  AVLinearPCMIsBigEndianKey: false,
  AVLinearPCMIsFloatKey: false
});
const errRef = Ref();
const rec = $.AVAudioRecorder.alloc.initWithURLSettingsError(url, settings, errRef);
if (!rec || !rec.prepareToRecord || !rec.record) {
  $.exit(1);
}
const stdin = $.NSFileHandle.fileHandleWithStandardInput;
stdin.readDataToEndOfFile;
rec.stop;
$.exit(0);
`;

function findExecutable(name: string, extraCandidates: readonly string[]): string | undefined {
  for (const candidate of extraCandidates) {
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // Continue searching
    }
  }
  try {
    const resolved = execFileSync('/usr/bin/which', [name], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
    if (resolved) {
      fs.accessSync(resolved, fs.constants.X_OK);
      return resolved;
    }
  } catch {
    // Binary not found in PATH
  }
  return undefined;
}

/**
 * Detects an available macOS audio recording tool using a clean fallback chain:
 * `afrecord` -> `rec` (SoX) -> `sox` -> `ffmpeg` -> `/usr/bin/swift` (`AVAudioRecorder`) -> `/usr/bin/osascript`.
 */
export function detectMacAudioRecorder(): DetectedRecorder | undefined {
  const afrecord = findExecutable('afrecord', ['/usr/bin/afrecord', '/usr/local/bin/afrecord']);
  if (afrecord) {
    return { kind: 'afrecord', binaryPath: afrecord };
  }

  const rec = findExecutable('rec', ['/opt/homebrew/bin/rec', '/usr/local/bin/rec']);
  if (rec) {
    return { kind: 'rec', binaryPath: rec };
  }

  const sox = findExecutable('sox', ['/opt/homebrew/bin/sox', '/usr/local/bin/sox']);
  if (sox) {
    return { kind: 'sox', binaryPath: sox };
  }

  const ffmpeg = findExecutable('ffmpeg', ['/opt/homebrew/bin/ffmpeg', '/usr/local/bin/ffmpeg']);
  if (ffmpeg) {
    return { kind: 'ffmpeg', binaryPath: ffmpeg };
  }

  const swift = findExecutable('swift', ['/usr/bin/swift']);
  if (swift) {
    return { kind: 'swift', binaryPath: swift };
  }

  const osascript = findExecutable('osascript', ['/usr/bin/osascript']);
  if (osascript) {
    return { kind: 'osascript', binaryPath: osascript };
  }

  return undefined;
}

function extractHoverText(hovers: readonly vscode.Hover[] | undefined): string | undefined {
  if (!hovers || hovers.length === 0) {
    return undefined;
  }
  const chunks: string[] = [];
  for (const hover of hovers) {
    for (const part of hover.contents) {
      if (typeof part === 'string') {
        if (part.trim()) {
          chunks.push(part.trim());
        }
      } else if (part instanceof vscode.MarkdownString) {
        if (part.value.trim()) {
          chunks.push(part.value.trim());
        }
      } else if (typeof part === 'object' && part !== null && 'value' in part) {
        const val = part.value.trim();
        if (val) {
          chunks.push(part.language ? `\`\`\`${part.language}\n${val}\n\`\`\`` : val);
        }
      }
    }
  }
  if (chunks.length === 0) {
    return undefined;
  }
  return chunks.join('\n').slice(0, 1_200);
}

function findEnclosingDocumentSymbol(
  symbols: readonly (vscode.DocumentSymbol | vscode.SymbolInformation)[] | undefined,
  pos: vscode.Position
): string | undefined {
  if (!symbols || symbols.length === 0) {
    return undefined;
  }

  const searchTree = (nodes: readonly vscode.DocumentSymbol[], prefix = ''): string | undefined => {
    for (const node of nodes) {
      if (node.range.contains(pos)) {
        const qualified = prefix ? `${prefix}.${node.name}` : node.name;
        const childHit = searchTree(node.children, qualified);
        if (childHit) {
          return childHit;
        }
        const kindLabel = vscode.SymbolKind[node.kind] ?? 'Symbol';
        return node.detail ? `${kindLabel} ${qualified} (${node.detail})` : `${kindLabel} ${qualified}`;
      }
    }
    return undefined;
  };

  const docSymbols = symbols.filter(
    (s): s is vscode.DocumentSymbol => 'selectionRange' in s && 'children' in s
  );
  if (docSymbols.length > 0) {
    return searchTree(docSymbols);
  }

  for (const sym of symbols) {
    if ('location' in sym && sym.location.range.contains(pos)) {
      const kindLabel = vscode.SymbolKind[sym.kind] ?? 'Symbol';
      return `${kindLabel} ${sym.name}`;
    }
  }
  return undefined;
}

/**
 * Captures the user's active spatial editor context: `file:startLine-endLine`, selected code
 * (or surrounding focus block), hovered symbol signature, enclosing AST symbol, and diagnostics.
 */
export async function captureSpatialEditorContext(): Promise<SpatialEditorContext> {
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    return {};
  }

  const doc = editor.document;
  const relPath = vscode.workspace.asRelativePath(doc.uri, false);
  const cursor = editor.selection.active;

  let startLineZero: number;
  let endLineZero: number;
  let selectedCode: string;

  if (!editor.selection.isEmpty) {
    startLineZero = editor.selection.start.line;
    endLineZero = editor.selection.end.line;
    selectedCode = doc.getText(editor.selection);
  } else {
    startLineZero = Math.max(0, cursor.line - 10);
    endLineZero = Math.min(doc.lineCount - 1, cursor.line + 15);
    const windowRange = new vscode.Range(
      new vscode.Position(startLineZero, 0),
      doc.lineAt(endLineZero).range.end
    );
    selectedCode = doc.getText(windowRange);
  }

  const startLine = startLineZero + 1;
  const endLine = endLineZero + 1;
  const rangeLabel = `${relPath}:${startLine}-${endLine}`;

  let hoveredSymbol: string | undefined;
  let enclosingSymbol: string | undefined;

  try {
    const [hovers, symbols] = await Promise.all([
      vscode.commands.executeCommand<vscode.Hover[]>(
        'vscode.executeHoverProvider',
        doc.uri,
        cursor
      ),
      vscode.commands.executeCommand<Array<vscode.DocumentSymbol | vscode.SymbolInformation>>(
        'vscode.executeDocumentSymbolProvider',
        doc.uri
      ),
    ]);
    hoveredSymbol = extractHoverText(hovers);
    enclosingSymbol = findEnclosingDocumentSymbol(symbols, cursor);
  } catch {
    // Ignore hover/symbol provider errors if language server is still starting
  }

  const diags = vscode.languages
    .getDiagnostics(doc.uri)
    .filter((d) => d.range.start.line >= startLineZero - 3 && d.range.end.line <= endLineZero + 3)
    .slice(0, 8)
    .map((d) => {
      const sev =
        d.severity === vscode.DiagnosticSeverity.Error
          ? 'error'
          : d.severity === vscode.DiagnosticSeverity.Warning
            ? 'warning'
            : 'info';
      return `[${sev}] line ${d.range.start.line + 1}: ${d.message}`;
    });

  return {
    filePath: relPath,
    languageId: doc.languageId,
    startLine,
    endLine,
    rangeLabel,
    selectedCode: selectedCode.slice(0, 12_000),
    hoveredSymbol,
    enclosingSymbol,
    diagnosticsSummary: diags.length > 0 ? diags.join('\n') : undefined,
  };
}

function buildSpatialVoicePrompt(ctx: SpatialEditorContext): string {
  const sections: string[] = [
    'Listen carefully to the attached spoken developer instruction (`audio/wav`) and execute it accurately against the spatial editor context below.',
  ];

  if (ctx.rangeLabel) {
    sections.push(`Active Focus Region: \`${ctx.rangeLabel}\` (${ctx.languageId ?? 'text'})`);
  }
  if (ctx.enclosingSymbol) {
    sections.push(`Enclosing Symbol: ${ctx.enclosingSymbol}`);
  }
  if (ctx.hoveredSymbol) {
    sections.push(`Hovered Symbol Signature / Type Info:\n${ctx.hoveredSymbol}`);
  }
  if (ctx.diagnosticsSummary) {
    sections.push(`Nearby Diagnostics:\n${ctx.diagnosticsSummary}`);
  }
  if (ctx.selectedCode) {
    sections.push(
      `Code at \`${ctx.rangeLabel ?? ctx.filePath ?? 'selection'}\`:\n\`\`\`${
        ctx.languageId ?? ''
      }\n${ctx.selectedCode}\n\`\`\``
    );
  }

  return sections.join('\n\n');
}

/**
 * Hold / Toggle Spatial Voice-to-Code controller (`rursor.voicePrompt`).
 * Records 16kHz mono WAV audio on macOS into `/tmp/rursor_voice_<ts>.wav`, bundles
 * active selection (`file:startLine-endLine`) + hovered symbol metadata, dispatches
 * `inlineData: { mimeType: 'audio/wav', data }` to Rursor's Agent/sidecar, and always
 * removes temporary audio files.
 */
export class VoicePromptController implements vscode.Disposable {
  private activeSession: ActiveRecordingSession | undefined;
  private readonly micStatusBar: vscode.StatusBarItem;
  private readonly outputChannel: vscode.OutputChannel;

  constructor(
    private readonly client: SidecarClient,
    private readonly telemetry?: TelemetryTracker,
    private readonly customDispatch?: VoiceDispatchHandler,
    priority = 97
  ) {
    this.micStatusBar = vscode.window.createStatusBarItem(
      vscode.StatusBarAlignment.Right,
      priority
    );
    this.micStatusBar.command = 'rursor.voicePrompt';
    this.outputChannel = vscode.window.createOutputChannel('Rursor Voice');
    this.refreshMicStatusBar();
    this.micStatusBar.show();
  }

  public get isRecording(): boolean {
    return this.activeSession !== undefined;
  }

  /**
   * Toggles Voice-to-Code recording: starts recording on first invocation,
   * or stops & dispatches if a recording is already active.
   */
  public async toggleVoicePrompt(): Promise<void> {
    if (this.activeSession) {
      await this.stopAndSendRecording();
      return;
    }
    await this.startRecording();
  }

  public async startRecording(): Promise<void> {
    if (this.activeSession) {
      return;
    }

    if (process.platform !== 'darwin') {
      vscode.window.showWarningMessage(
        'Rursor Spatial Voice-to-Code currently targets macOS audio input.'
      );
      return;
    }

    const recorder = detectMacAudioRecorder();
    if (!recorder) {
      vscode.window.showErrorMessage(
        'No macOS audio recorder found (checked afrecord, rec, sox, ffmpeg, /usr/bin/swift, and /usr/bin/osascript).'
      );
      return;
    }

    const spatialContext = await captureSpatialEditorContext();
    const ts = Date.now();
    const wavPath = `/tmp/rursor_voice_${ts}.wav`;
    let helperScriptPath: string | undefined;

    let proc: ChildProcess;
    try {
      if (recorder.kind === 'afrecord') {
        proc = spawn(
          recorder.binaryPath,
          ['-f', 'WAVE', '-d', 'LEI16@16000', '-c', '1', wavPath],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      } else if (recorder.kind === 'rec') {
        proc = spawn(
          recorder.binaryPath,
          ['-q', '-r', '16000', '-c', '1', '-b', '16', '-e', 'signed-integer', wavPath],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      } else if (recorder.kind === 'sox') {
        proc = spawn(
          recorder.binaryPath,
          ['-d', '-q', '-r', '16000', '-c', '1', '-b', '16', '-e', 'signed-integer', wavPath],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      } else if (recorder.kind === 'ffmpeg') {
        proc = spawn(
          recorder.binaryPath,
          [
            '-loglevel',
            'error',
            '-y',
            '-f',
            'avfoundation',
            '-i',
            ':0',
            '-ac',
            '1',
            '-ar',
            '16000',
            '-acodec',
            'pcm_s16le',
            wavPath,
          ],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      } else if (recorder.kind === 'swift') {
        helperScriptPath = `/tmp/rursor_voice_helper_${ts}.swift`;
        await fsp.writeFile(helperScriptPath, SWIFT_AV_AUDIO_RECORDER_SCRIPT, 'utf8');
        proc = spawn(
          recorder.binaryPath,
          ['-module-cache-path', '/tmp/rursor_swift_cache', helperScriptPath, wavPath],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      } else {
        proc = spawn(
          recorder.binaryPath,
          ['-l', 'JavaScript', '-e', JXA_AV_AUDIO_RECORDER_SCRIPT, wavPath],
          { stdio: ['pipe', 'ignore', 'pipe'] }
        );
      }
    } catch (err) {
      await this.safeCleanupFiles(wavPath, helperScriptPath);
      vscode.window.showErrorMessage(
        `Failed to start microphone capture: ${err instanceof Error ? err.message : String(err)}`
      );
      return;
    }

    proc.stderr?.on('data', (chunk: Buffer) => {
      const msg = chunk.toString('utf8').trim();
      if (msg) {
        this.outputChannel.appendLine(`[voice:${recorder.kind}] ${msg}`);
      }
    });

    const timeoutHandle = setTimeout(() => {
      if (this.activeSession?.wavPath === wavPath) {
        void this.stopAndSendRecording();
      }
    }, MAX_VOICE_DURATION_MS);

    const session: ActiveRecordingSession = {
      wavPath,
      helperScriptPath,
      proc,
      recorder,
      startedAt: Date.now(),
      spatialContext,
      timeoutHandle,
      cancelled: false,
      stopping: false,
    };
    this.activeSession = session;
    this.refreshMicStatusBar();

    const targetBadge = spatialContext.rangeLabel ? ` (${spatialContext.rangeLabel})` : '';
    void vscode.window
      .showInformationMessage(
        `Rursor Voice-to-Code recording${targetBadge} via ${recorder.kind}...`,
        'Stop & Send Recording',
        'Cancel'
      )
      .then(async (choice) => {
        if (this.activeSession !== session) {
          return;
        }
        if (choice === 'Stop & Send Recording') {
          await this.stopAndSendRecording();
        } else if (choice === 'Cancel') {
          await this.cancelRecording();
        }
      });
  }

  public async cancelRecording(): Promise<void> {
    const session = this.activeSession;
    if (!session || session.stopping) {
      return;
    }
    session.cancelled = true;
    session.stopping = true;
    clearTimeout(session.timeoutHandle);
    this.activeSession = undefined;
    this.refreshMicStatusBar();

    try {
      await this.gracefullyStopRecorder(session);
    } finally {
      await this.safeCleanupFiles(session.wavPath, session.helperScriptPath);
    }
  }

  public async stopAndSendRecording(): Promise<void> {
    const session = this.activeSession;
    if (!session || session.stopping) {
      return;
    }
    session.stopping = true;
    clearTimeout(session.timeoutHandle);
    this.activeSession = undefined;
    this.refreshMicStatusBar();

    const normWavPath = `${session.wavPath}.norm.wav`;
    try {
      await this.gracefullyStopRecorder(session);
      if (session.cancelled) {
        return;
      }

      const finalWavPath = await this.ensure16kMonoWav(session.wavPath, normWavPath);
      const stat = await fsp.stat(finalWavPath).catch(() => undefined);
      if (!stat || stat.size < MIN_VALID_WAV_BYTES) {
        vscode.window.showWarningMessage(
          'Recorded audio was empty or too short. Check macOS Microphone permissions for Rursor / VS Code.'
        );
        return;
      }

      const wavBytes = await fsp.readFile(finalWavPath);
      const base64Audio = wavBytes.toString('base64');
      const durationMs = Math.max(1, Date.now() - session.startedAt);

      const latestSpatial = vscode.window.activeTextEditor
        ? await captureSpatialEditorContext()
        : session.spatialContext;
      const mergedSpatial = latestSpatial.filePath ? latestSpatial : session.spatialContext;
      const formattedPrompt = buildSpatialVoicePrompt(mergedSpatial);

      const payload: VoicePromptPayload = {
        inlineData: {
          mimeType: 'audio/wav',
          data: base64Audio,
        },
        spatialContext: mergedSpatial,
        formattedPrompt,
        durationMs,
      };

      if (this.customDispatch) {
        await this.customDispatch(payload);
      } else {
        await this.dispatchToAgentViaSidecar(payload);
      }
    } catch (err) {
      vscode.window.showErrorMessage(
        `Rursor Voice-to-Code failed: ${err instanceof Error ? err.message : String(err)}`
      );
    } finally {
      await this.safeCleanupFiles(session.wavPath, session.helperScriptPath, normWavPath);
    }
  }

  /**
   * Optional helper to transcribe a base64 `audio/wav` clip to plain text via `rursor-sidecar`.
   */
  public async transcribeViaSidecar(base64Wav: string): Promise<string> {
    const model = vscode.workspace
      .getConfiguration('rursor')
      .get<string>('defaultModel', 'gemini-3.8-flash');
    const timer = this.telemetry?.startTurn('voice', model, 320, 0);
    const { promise } = this.client.request<{ text: string }>(
      'agent_chat',
      {
        mode: 'ask',
        model,
        useWorkspaceSearch: false,
        includeGitDiff: false,
        messages: [
          {
            role: 'user',
            content:
              'Transcribe the spoken developer instruction in this audio verbatim. Output ONLY the transcription text with no commentary.',
            images: [
              {
                mimeType: 'audio/wav',
                data: base64Wav,
                name: 'voice_prompt.wav',
              },
            ],
          },
        ],
      },
      {
        onStream: () => {
          timer?.markFirstToken();
        },
      }
    );
    const res = await promise;
    const transcript = (res.text ?? '').trim();
    timer?.finish(transcript.length);
    return transcript;
  }

  private async dispatchToAgentViaSidecar(payload: VoicePromptPayload): Promise<void> {
    await vscode.commands.executeCommand('rursor.openAgent').then(
      () => undefined,
      () => undefined
    );

    const cfg = vscode.workspace.getConfiguration('rursor');
    const model = cfg.get<string>('defaultModel', 'gemini-3.8-flash');
    const contextFiles: Array<{ path: string; content: string }> = [];
    if (payload.spatialContext.filePath && payload.spatialContext.selectedCode) {
      contextFiles.push({
        path: payload.spatialContext.filePath,
        content: payload.spatialContext.selectedCode,
      });
    }

    const estAudioTokens = Math.max(64, Math.ceil((payload.durationMs / 1000) * 32));
    const estPromptTokens = Math.ceil(payload.formattedPrompt.length / 4) + estAudioTokens;
    const timer = this.telemetry?.startTurn('voice', model, estPromptTokens, 0);

    this.outputChannel.appendLine(
      `[voice] Dispatching ${(payload.durationMs / 1000).toFixed(1)}s WAV + spatial context (${
        payload.spatialContext.rangeLabel ?? 'workspace'
      }) to ${model}...`
    );

    await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: `Rursor Voice-to-Code (${payload.spatialContext.rangeLabel ?? 'Agent'})`,
        cancellable: true,
      },
      async (progress, token) => {
        progress.report({ message: 'Streaming multimodal audio + code context to Gemini...' });
        let streamedChars = 0;

        const { id, promise } = this.client.request<{ text: string }>(
          'agent_chat',
          {
            sessionId: `voice_${Date.now()}`,
            mode: 'agent',
            model,
            useWorkspaceSearch: true,
            includeGitDiff: false,
            contextFiles,
            messages: [
              {
                role: 'user',
                content: payload.formattedPrompt,
                images: [
                  {
                    mimeType: payload.inlineData.mimeType,
                    data: payload.inlineData.data,
                    name: 'voice_prompt.wav',
                  },
                ],
              },
            ],
          },
          {
            onStream: (delta: string) => {
              timer?.markFirstToken();
              streamedChars += delta.length;
              this.outputChannel.append(delta);
            },
            onToolEvent: (ev: ToolEventPayload) => {
              if (ev.phase === 'start') {
                progress.report({ message: `Running ${ev.name}...` });
              }
            },
            onToolRequest: (req: ToolRequestPayload) => {
              void this.handleVoiceToolRequest(req);
            },
          }
        );

        token.onCancellationRequested(() => {
          void this.client.cancel(id);
        });

        const res = await promise;
        this.outputChannel.appendLine('\n');
        const fullText = res.text ?? '';
        timer?.finish(Math.max(streamedChars, fullText.length));

        const preview = fullText.replace(/\s+/g, ' ').trim().slice(0, 140);
        if (preview) {
          const action = await vscode.window.showInformationMessage(
            `Rursor Voice: ${preview}${fullText.length > 140 ? '…' : ''}`,
            'View Voice Output'
          );
          if (action === 'View Voice Output') {
            this.outputChannel.show(true);
          }
        }
      }
    );
  }

  private async handleVoiceToolRequest(req: ToolRequestPayload): Promise<void> {
    const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    if (!root) {
      await this.client.submitToolResult(req.callId, false, {
        error: 'No workspace folder is open.',
      });
      return;
    }

    if (req.name === 'edit_file' || req.name === 'write_file') {
      const relPath = String(req.args.path ?? '');
      const resolved = path.resolve(root, relPath);
      if (resolved !== root && !resolved.startsWith(`${root}${path.sep}`)) {
        await this.client.submitToolResult(req.callId, false, {
          error: `Refusing to modify path outside workspace: ${relPath}`,
        });
        return;
      }

      const approval = await vscode.window.showInformationMessage(
        `Rursor Voice Agent wants to ${req.name === 'edit_file' ? 'edit' : 'write'} ${relPath}`,
        { modal: false },
        'Apply',
        'Reject'
      );
      if (approval !== 'Apply') {
        await this.client.submitToolResult(req.callId, false, {
          error: 'User rejected file modification.',
        });
        return;
      }

      try {
        if (req.name === 'edit_file') {
          const current = await fsp.readFile(resolved, 'utf8');
          const oldText = String(req.args.old_text ?? '');
          const newText = String(req.args.new_text ?? '');
          const idx = current.indexOf(oldText);
          if (!oldText || idx === -1) {
            await this.client.submitToolResult(req.callId, false, {
              error: `old_text not found verbatim in ${relPath}.`,
            });
            return;
          }
          const updated = current.slice(0, idx) + newText + current.slice(idx + oldText.length);
          await fsp.writeFile(resolved, updated, 'utf8');
        } else {
          await fsp.mkdir(path.dirname(resolved), { recursive: true });
          await fsp.writeFile(resolved, String(req.args.content ?? ''), 'utf8');
        }
        await this.client.submitToolResult(req.callId, true, { applied: true, path: relPath });
      } catch (err) {
        await this.client.submitToolResult(req.callId, false, {
          error: err instanceof Error ? err.message : String(err),
        });
      }
      return;
    }

    await this.client.submitToolResult(req.callId, false, {
      error: `Tool '${req.name}' requires interactive Agent panel approval.`,
    });
  }

  private async gracefullyStopRecorder(session: ActiveRecordingSession): Promise<void> {
    const { proc, recorder } = session;
    if (proc.exitCode !== null || proc.killed) {
      return;
    }

    await new Promise<void>((resolve) => {
      let settled = false;
      const done = () => {
        if (!settled) {
          settled = true;
          resolve();
        }
      };

      proc.once('exit', done);
      proc.once('error', done);

      try {
        if (recorder.kind === 'ffmpeg') {
          proc.stdin?.write('q\n');
          proc.stdin?.end();
        } else if (recorder.kind === 'osascript') {
          proc.stdin?.end();
        } else {
          proc.kill('SIGINT');
        }
      } catch {
        // Ignore signal error if already exited
      }

      setTimeout(() => {
        if (!settled) {
          try {
            proc.kill('SIGINT');
          } catch {
            // Ignore
          }
        }
      }, 600);

      setTimeout(() => {
        if (!settled) {
          try {
            proc.kill('SIGKILL');
          } catch {
            // Ignore
          }
          done();
        }
      }, 2_000);
    });
  }

  private async ensure16kMonoWav(rawWavPath: string, normalizedPath: string): Promise<string> {
    const stat = await fsp.stat(rawWavPath).catch(() => undefined);
    if (!stat || stat.size === 0) {
      return rawWavPath;
    }

    const afconvert = findExecutable('afconvert', ['/usr/bin/afconvert']);
    if (!afconvert) {
      return rawWavPath;
    }

    try {
      await new Promise<void>((resolve, reject) => {
        const conv = spawn(
          afconvert,
          ['-f', 'WAVE', '-d', 'LEI16@16000', '-c', '1', rawWavPath, normalizedPath],
          { stdio: 'ignore' }
        );
        conv.once('error', reject);
        conv.once('exit', (code) => {
          if (code === 0) {
            resolve();
          } else {
            reject(new Error(`afconvert exited with code ${code}`));
          }
        });
      });
      const normStat = await fsp.stat(normalizedPath).catch(() => undefined);
      if (normStat && normStat.size >= MIN_VALID_WAV_BYTES) {
        return normalizedPath;
      }
    } catch {
      // Fall back to original recorded WAV if already 16kHz PCM
    }
    return rawWavPath;
  }

  private async safeCleanupFiles(...paths: Array<string | undefined>): Promise<void> {
    for (const p of paths) {
      if (!p) {
        continue;
      }
      await fsp.unlink(p).catch(() => undefined);
    }
  }

  private refreshMicStatusBar(): void {
    if (this.activeSession) {
      this.micStatusBar.text = '$(record) Rec · Stop & Send';
      this.micStatusBar.backgroundColor = new vscode.ThemeColor(
        'statusBarItem.warningBackground'
      );
      this.micStatusBar.tooltip =
        'Rursor Voice-to-Code recording active — click to Stop & Send Recording';
    } else {
      this.micStatusBar.text = '$(mic) Voice';
      this.micStatusBar.backgroundColor = undefined;
      this.micStatusBar.tooltip =
        'Rursor Spatial Voice-to-Code (Click to record spoken instruction with active selection + hover context)';
    }
  }

  public dispose(): void {
    if (this.activeSession) {
      const sess = this.activeSession;
      this.activeSession = undefined;
      clearTimeout(sess.timeoutHandle);
      try {
        sess.proc.kill('SIGKILL');
      } catch {
        // Ignore
      }
      void this.safeCleanupFiles(sess.wavPath, sess.helperScriptPath, `${sess.wavPath}.norm.wav`);
    }
    this.micStatusBar.dispose();
    this.outputChannel.dispose();
  }
}

/**
 * Convenience helper to instantiate and register both `TelemetryTracker` (`rursor.showTelemetry`)
 * and `VoicePromptController` (`rursor.voicePrompt`) on the extension context.
 */
export function registerVoiceAndTelemetry(
  context: vscode.ExtensionContext,
  client: SidecarClient,
  customVoiceDispatch?: VoiceDispatchHandler
): { telemetry: TelemetryTracker; voice: VoicePromptController } {
  const telemetry = new TelemetryTracker();
  const voice = new VoicePromptController(client, telemetry, customVoiceDispatch);

  context.subscriptions.push(
    telemetry,
    voice,
    vscode.commands.registerCommand('rursor.showTelemetry', () => telemetry.showTelemetry()),
    vscode.commands.registerCommand('rursor.voicePrompt', () => voice.toggleVoicePrompt())
  );

  return { telemetry, voice };
}
