import * as crypto from 'node:crypto';
import * as fs from 'node:fs';
import * as fsp from 'node:fs/promises';
import * as os from 'node:os';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { SidecarClient, StreamCallbacks } from './sidecarClient';

export type SwarmRole =
  | 'planner'
  | 'researcher'
  | 'coder'
  | 'verifier'
  | 'mediaProducer'
  | (string & {});

export type SwarmNodeStatus =
  | 'pending'
  | 'running'
  | 'verifying'
  | 'completed'
  | 'failed'
  | 'blocked';

export interface CloneFileDiff {
  relPath: string;
  status: 'added' | 'modified' | 'deleted' | string;
  addedLines: number;
  removedLines: number;
}

export interface VerifyResult {
  label: string;
  command: string;
  exitCode: number;
  output: string;
}

export interface SwarmTaskSpec {
  id: string;
  title: string;
  role: SwarmRole;
  prompt: string;
  dependsOn: string[];
  writeEnabled: boolean;
  modelOverride?: string | null;
}

export interface SwarmNodeState {
  id: string;
  title: string;
  role: SwarmRole;
  dependsOn: string[];
  status: SwarmNodeStatus;
  cloneId?: string | null;
  clonePath?: string | null;
  summary: string;
  modifiedFiles: CloneFileDiff[];
  verifyResults: VerifyResult[];
  durationMs: number;
  error?: string | null;
}

export interface BlackboardEntry {
  key: string;
  value: string;
  authorId: string;
  updatedAt: number;
}

export interface AgentMailMessage {
  fromId: string;
  toId: string;
  message: string;
  createdAt: number;
}

export interface SwarmConflict {
  relPath: string;
  workerIds: string[];
}

export interface SwarmMergeReport {
  mergedFiles: string[];
  conflicts: SwarmConflict[];
  stagingCloneId?: string | null;
}

export interface SwarmMissionReport {
  missionId: string;
  goal: string;
  waves: SwarmTaskSpec[][];
  nodes: SwarmNodeState[];
  blackboard: BlackboardEntry[];
  mail: AgentMailMessage[];
  conflicts: SwarmConflict[];
  durationMs: number;
}

export interface ArenaCandidateResult {
  candidateId: string;
  model: string;
  provider: string;
  cloneId: string;
  clonePath: string;
  ok: boolean;
  summary: string;
  diffs: CloneFileDiff[];
  verifyPassed: boolean;
  verifyResults: VerifyResult[];
  durationMs: number;
  score: number;
}

export interface WorkerLiveAction {
  workerId: string;
  turn: number;
  action: string;
  detail: string;
  ok: boolean;
  timestamp: number;
}

interface MissionControlViewState {
  activeTab: 'swarm' | 'arena';
  workspaceRoot: string | null;
  permissionProfile: string;
  allowNetwork: boolean;
  swarmRunning: boolean;
  swarmError: string | null;
  mission: SwarmMissionReport | null;
  mergeReport: SwarmMergeReport | null;
  workerActions: Record<string, WorkerLiveAction>;
  arenaRunning: boolean;
  arenaError: string | null;
  arenaPrompt: string;
  arenaSelectedModels: string[];
  arenaCandidates: ArenaCandidateResult[];
  arenaStatuses: Record<string, string>;
  arenaDurationMs: number;
}

interface WebviewIncomingMessage {
  type: string;
  tab?: 'swarm' | 'arena';
  goal?: string;
  maxParallelWorkers?: number;
  prompt?: string;
  models?: string[];
  cloneId?: string;
  clonePath?: string;
  relPath?: string;
  workerTitle?: string;
}

type EventCapableSidecarClient = SidecarClient & {
  onEvent?: (
    eventOrListener:
      | string
      | ((eventName: string, payload: unknown, requestId?: number) => void)
      | ((envelope: { event: string; payload?: unknown; requestId?: number }) => void),
    maybeListener?: (payload: unknown, requestId?: number) => void
  ) => vscode.Disposable | (() => void) | void;
};

type SwarmAwareStreamCallbacks = StreamCallbacks & {
  onSwarmEvent?: (payload: unknown) => void;
  onEvent?: (eventName: string, payload: unknown) => void;
};

const ARENA_DEFAULT_MODELS: readonly string[] = [
  'gemini-3.1-pro-preview',
  'gemini-3.8-flash',
  'claude-sonnet-4-20250514',
  'gpt-4.1',
];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function asString(value: unknown, fallback = ''): string {
  return typeof value === 'string' ? value : fallback;
}

function asNumber(value: unknown, fallback = 0): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function asBoolean(value: unknown, fallback = false): boolean {
  return typeof value === 'boolean' ? value : fallback;
}

function normalizeStatus(raw: unknown): SwarmNodeStatus {
  const lower = asString(raw, 'pending').trim().toLowerCase();
  switch (lower) {
    case 'running':
      return 'running';
    case 'verifying':
      return 'verifying';
    case 'completed':
      return 'completed';
    case 'failed':
      return 'failed';
    case 'blocked':
      return 'blocked';
    default:
      return 'pending';
  }
}

function normalizeFileDiff(raw: unknown): CloneFileDiff | null {
  if (!isRecord(raw)) {
    return null;
  }
  const relPath = asString(raw.relPath ?? raw.rel_path);
  if (!relPath) {
    return null;
  }
  return {
    relPath,
    status: asString(raw.status, 'modified'),
    addedLines: asNumber(raw.addedLines ?? raw.added_lines, 0),
    removedLines: asNumber(raw.removedLines ?? raw.removed_lines, 0),
  };
}

function normalizeVerifyResult(raw: unknown): VerifyResult | null {
  if (!isRecord(raw)) {
    return null;
  }
  return {
    label: asString(raw.label, 'Check'),
    command: asString(raw.command),
    exitCode: asNumber(raw.exitCode ?? raw.exit_code, 0),
    output: asString(raw.output),
  };
}

function normalizeTaskSpec(raw: unknown): SwarmTaskSpec | null {
  if (!isRecord(raw)) {
    return null;
  }
  const id = asString(raw.id);
  if (!id) {
    return null;
  }
  const depsRaw = Array.isArray(raw.dependsOn ?? raw.depends_on)
    ? ((raw.dependsOn ?? raw.depends_on) as unknown[])
    : [];
  return {
    id,
    title: asString(raw.title, id),
    role: asString(raw.role, 'coder'),
    prompt: asString(raw.prompt),
    dependsOn: depsRaw.map((d) => asString(d)).filter(Boolean),
    writeEnabled: asBoolean(raw.writeEnabled ?? raw.write_enabled, false),
    modelOverride:
      typeof (raw.modelOverride ?? raw.model_override) === 'string'
        ? asString(raw.modelOverride ?? raw.model_override)
        : null,
  };
}

function normalizeNodeState(raw: unknown): SwarmNodeState | null {
  if (!isRecord(raw)) {
    return null;
  }
  const id = asString(raw.id);
  if (!id) {
    return null;
  }
  const depsRaw = Array.isArray(raw.dependsOn ?? raw.depends_on)
    ? ((raw.dependsOn ?? raw.depends_on) as unknown[])
    : [];
  const filesRaw = Array.isArray(raw.modifiedFiles ?? raw.modified_files)
    ? ((raw.modifiedFiles ?? raw.modified_files) as unknown[])
    : [];
  const verifyRaw = Array.isArray(raw.verifyResults ?? raw.verify_results)
    ? ((raw.verifyResults ?? raw.verify_results) as unknown[])
    : [];

  const cloneIdVal = raw.cloneId ?? raw.clone_id;
  const clonePathVal = raw.clonePath ?? raw.clone_path;
  const errorVal = raw.error;

  return {
    id,
    title: asString(raw.title, id),
    role: asString(raw.role, 'coder'),
    dependsOn: depsRaw.map((d) => asString(d)).filter(Boolean),
    status: normalizeStatus(raw.status),
    cloneId: typeof cloneIdVal === 'string' && cloneIdVal ? cloneIdVal : null,
    clonePath: typeof clonePathVal === 'string' && clonePathVal ? clonePathVal : null,
    summary: asString(raw.summary),
    modifiedFiles: filesRaw
      .map(normalizeFileDiff)
      .filter((item): item is CloneFileDiff => item !== null),
    verifyResults: verifyRaw
      .map(normalizeVerifyResult)
      .filter((item): item is VerifyResult => item !== null),
    durationMs: asNumber(raw.durationMs ?? raw.duration_ms, 0),
    error: typeof errorVal === 'string' && errorVal ? errorVal : null,
  };
}

function normalizeBlackboardEntry(raw: unknown): BlackboardEntry | null {
  if (!isRecord(raw)) {
    return null;
  }
  const key = asString(raw.key);
  if (!key) {
    return null;
  }
  return {
    key,
    value: asString(raw.value),
    authorId: asString(raw.authorId ?? raw.author_id, 'agent'),
    updatedAt: asNumber(raw.updatedAt ?? raw.updated_at, Date.now()),
  };
}

function normalizeMailMessage(raw: unknown): AgentMailMessage | null {
  if (!isRecord(raw)) {
    return null;
  }
  const fromId = asString(raw.fromId ?? raw.from_id);
  const toId = asString(raw.toId ?? raw.to_id);
  if (!fromId && !toId) {
    return null;
  }
  return {
    fromId: fromId || 'agent',
    toId: toId || '*',
    message: asString(raw.message),
    createdAt: asNumber(raw.createdAt ?? raw.created_at, Date.now()),
  };
}

function normalizeConflict(raw: unknown): SwarmConflict | null {
  if (!isRecord(raw)) {
    return null;
  }
  const relPath = asString(raw.relPath ?? raw.rel_path);
  if (!relPath) {
    return null;
  }
  const workersRaw = Array.isArray(raw.workerIds ?? raw.worker_ids)
    ? ((raw.workerIds ?? raw.worker_ids) as unknown[])
    : [];
  return {
    relPath,
    workerIds: workersRaw.map((w) => asString(w)).filter(Boolean),
  };
}

function normalizeMissionReport(raw: unknown, fallbackGoal = ''): SwarmMissionReport | null {
  if (!isRecord(raw)) {
    return null;
  }
  const target = isRecord(raw.report) ? raw.report : raw;
  const nodesRaw = Array.isArray(target.nodes) ? target.nodes : [];
  const wavesRaw = Array.isArray(target.waves) ? target.waves : [];
  const bbRaw = Array.isArray(target.blackboard) ? target.blackboard : [];
  const mailRaw = Array.isArray(target.mail) ? target.mail : [];
  const conflictsRaw = Array.isArray(target.conflicts) ? target.conflicts : [];

  const nodes = nodesRaw
    .map(normalizeNodeState)
    .filter((n): n is SwarmNodeState => n !== null);
  const waves: SwarmTaskSpec[][] = wavesRaw
    .map((wave) =>
      Array.isArray(wave)
        ? wave.map(normalizeTaskSpec).filter((t): t is SwarmTaskSpec => t !== null)
        : []
    )
    .filter((w) => w.length > 0);

  if (nodes.length === 0 && waves.length === 0) {
    return null;
  }

  return {
    missionId: asString(target.missionId ?? target.mission_id, `swarm_${Date.now()}`),
    goal: asString(target.goal, fallbackGoal),
    waves,
    nodes,
    blackboard: bbRaw
      .map(normalizeBlackboardEntry)
      .filter((e): e is BlackboardEntry => e !== null),
    mail: mailRaw
      .map(normalizeMailMessage)
      .filter((m): m is AgentMailMessage => m !== null),
    conflicts: conflictsRaw
      .map(normalizeConflict)
      .filter((c): c is SwarmConflict => c !== null),
    durationMs: asNumber(target.durationMs ?? target.duration_ms, 0),
  };
}

function normalizeMergeReport(raw: unknown): SwarmMergeReport {
  const source = isRecord(raw) && isRecord(raw.report) ? raw.report : isRecord(raw) ? raw : {};
  const mergedRaw = Array.isArray(source.mergedFiles ?? source.merged_files)
    ? ((source.mergedFiles ?? source.merged_files) as unknown[])
    : [];
  const conflictsRaw = Array.isArray(source.conflicts) ? source.conflicts : [];
  const stagingVal = source.stagingCloneId ?? source.staging_clone_id;

  return {
    mergedFiles: mergedRaw.map((f) => asString(f)).filter(Boolean),
    conflicts: conflictsRaw
      .map(normalizeConflict)
      .filter((c): c is SwarmConflict => c !== null),
    stagingCloneId: typeof stagingVal === 'string' && stagingVal ? stagingVal : null,
  };
}

function normalizeArenaCandidate(raw: unknown): ArenaCandidateResult | null {
  if (!isRecord(raw)) {
    return null;
  }
  const candidateId = asString(raw.candidateId ?? raw.candidate_id);
  const model = asString(raw.model);
  if (!candidateId && !model) {
    return null;
  }
  const diffsRaw = Array.isArray(raw.diffs) ? raw.diffs : [];
  const verifyRaw = Array.isArray(raw.verifyResults ?? raw.verify_results)
    ? ((raw.verifyResults ?? raw.verify_results) as unknown[])
    : [];

  return {
    candidateId: candidateId || `candidate_${model}`,
    model: model || candidateId,
    provider: asString(raw.provider, 'gemini'),
    cloneId: asString(raw.cloneId ?? raw.clone_id),
    clonePath: asString(raw.clonePath ?? raw.clone_path),
    ok: asBoolean(raw.ok, false),
    summary: asString(raw.summary),
    diffs: diffsRaw
      .map(normalizeFileDiff)
      .filter((d): d is CloneFileDiff => d !== null),
    verifyPassed: asBoolean(raw.verifyPassed ?? raw.verify_passed, false),
    verifyResults: verifyRaw
      .map(normalizeVerifyResult)
      .filter((v): v is VerifyResult => v !== null),
    durationMs: asNumber(raw.durationMs ?? raw.duration_ms, 0),
    score: asNumber(raw.score, 0),
  };
}

function normalizeArenaCandidatesResponse(raw: unknown): ArenaCandidateResult[] {
  if (Array.isArray(raw)) {
    return raw
      .map(normalizeArenaCandidate)
      .filter((c): c is ArenaCandidateResult => c !== null);
  }
  if (!isRecord(raw)) {
    return [];
  }
  const arr = Array.isArray(raw.candidates)
    ? raw.candidates
    : Array.isArray(raw.results)
      ? raw.results
      : [];
  return arr
    .map(normalizeArenaCandidate)
    .filter((c): c is ArenaCandidateResult => c !== null);
}

/**
 * Registers the `rursor.missionControl` command to open Mission Control directly.
 */
export function registerMissionControlCommands(
  context: vscode.ExtensionContext,
  client: SidecarClient
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand('rursor.missionControl', (initialTab?: 'swarm' | 'arena') => {
      const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      MissionControlPanel.openOrShow(context.extensionUri, client, root, initialTab);
    })
  );
}

/**
 * Singleton WebviewPanel (`rursor.missionControl`) providing Rursor's Multi-Agent Swarm Mission
 * DAG & Best-of-N Model Arena dashboard styled in Rursor Obsidian (`#0B0D12` / `#131822` / `#7C5CFF`).
 */
export class MissionControlPanel implements vscode.Disposable {
  public static readonly viewType = 'rursor.missionControl';
  private static currentPanel: MissionControlPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private readonly disposables: vscode.Disposable[] = [];
  private isDisposed = false;
  private workspaceRoot: string | null;
  private activeSwarmReqId: number | null = null;
  private activeArenaReqId: number | null = null;
  private hasClientEventHook = false;
  private emptyScratchPath: string | null = null;

  private state: MissionControlViewState = {
    activeTab: 'swarm',
    workspaceRoot: null,
    permissionProfile: 'safe-auto',
    allowNetwork: false,
    swarmRunning: false,
    swarmError: null,
    mission: null,
    mergeReport: null,
    workerActions: {},
    arenaRunning: false,
    arenaError: null,
    arenaPrompt: '',
    arenaSelectedModels: ['gemini-3.1-pro-preview', 'gemini-3.8-flash'],
    arenaCandidates: [],
    arenaStatuses: {},
    arenaDurationMs: 0,
  };

  public static openOrShow(
    extensionUri: vscode.Uri,
    client: SidecarClient,
    workspaceRoot?: string,
    initialTab?: 'swarm' | 'arena'
  ): MissionControlPanel {
    const column = vscode.window.activeTextEditor?.viewColumn ?? vscode.ViewColumn.One;
    const resolvedRoot =
      workspaceRoot ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? undefined;

    if (MissionControlPanel.currentPanel && !MissionControlPanel.currentPanel.isDisposed) {
      if (resolvedRoot) {
        MissionControlPanel.currentPanel.workspaceRoot = resolvedRoot;
        MissionControlPanel.currentPanel.state.workspaceRoot = resolvedRoot;
      }
      if (initialTab) {
        MissionControlPanel.currentPanel.state.activeTab = initialTab;
      }
      MissionControlPanel.currentPanel.panel.reveal(column);
      void MissionControlPanel.currentPanel.syncEnvironmentStatus();
      return MissionControlPanel.currentPanel;
    }

    const panel = vscode.window.createWebviewPanel(
      MissionControlPanel.viewType,
      'Rursor Mission Control',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [extensionUri],
      }
    );

    MissionControlPanel.currentPanel = new MissionControlPanel(
      panel,
      client,
      resolvedRoot ?? null,
      initialTab ?? 'swarm'
    );
    return MissionControlPanel.currentPanel;
  }

  public static createOrShow(
    extensionUri: vscode.Uri,
    client: SidecarClient,
    workspaceRoot?: string
  ): MissionControlPanel {
    return MissionControlPanel.openOrShow(extensionUri, client, workspaceRoot);
  }

  private constructor(
    panel: vscode.WebviewPanel,
    private readonly client: SidecarClient,
    workspaceRoot: string | null,
    initialTab: 'swarm' | 'arena'
  ) {
    this.panel = panel;
    this.workspaceRoot = workspaceRoot;
    this.state.workspaceRoot = workspaceRoot;
    this.state.activeTab = initialTab;

    this.panel.iconPath = new vscode.ThemeIcon('Broadcast');
    this.panel.webview.html = this.renderHtml(this.panel.webview);

    this.attachSidecarEventSubscription();

    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage(
      (msg: WebviewIncomingMessage) => {
        void this.handleWebviewMessage(msg);
      },
      null,
      this.disposables
    );

    this.disposables.push(
      vscode.workspace.onDidChangeConfiguration((e) => {
        if (e.affectsConfiguration('rursor')) {
          void this.syncEnvironmentStatus();
        }
      })
    );

    void this.syncEnvironmentStatus();
  }

  private resolveWorkspaceRoot(): string | null {
    return (
      this.workspaceRoot ??
      vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ??
      this.state.workspaceRoot
    );
  }

  private attachSidecarEventSubscription(): void {
    const eventClient = this.client as EventCapableSidecarClient;
    if (typeof eventClient.onEvent !== 'function') {
      return;
    }

    this.hasClientEventHook = true;
    const registerDisposal = (sub: vscode.Disposable | (() => void) | void) => {
      if (!sub) {
        return;
      }
      if (typeof sub === 'function') {
        this.disposables.push(new vscode.Disposable(sub));
      } else if (typeof sub.dispose === 'function') {
        this.disposables.push(sub);
      }
    };

    if (eventClient.onEvent.length >= 2) {
      const subSwarm = eventClient.onEvent('swarm_event', (payload, requestId) => {
        this.handleSwarmNotification(payload, requestId);
      });
      registerDisposal(subSwarm);
    } else {
      const subAll = eventClient.onEvent(
        (first: unknown, second?: unknown, third?: unknown) => {
          if (typeof first === 'string') {
            if (first === 'swarm_event' || first === 'arena_event') {
              this.handleSwarmNotification(
                second,
                typeof third === 'number' ? third : undefined
              );
            }
          } else if (isRecord(first)) {
            const evName = asString(first.event);
            if (evName === 'swarm_event' || evName === 'arena_event') {
              this.handleSwarmNotification(
                first.payload,
                typeof first.requestId === 'number' ? first.requestId : undefined
              );
            }
          }
        }
      );
      registerDisposal(subAll);
    }
  }

  private async syncEnvironmentStatus(): Promise<void> {
    if (this.isDisposed) {
      return;
    }
    const cfg = vscode.workspace.getConfiguration('rursor');
    this.state.permissionProfile = cfg.get<string>('agent.permissionProfile', 'safe-auto');
    this.state.allowNetwork = cfg.get<boolean>('agent.sandboxAllowNetwork', false);
    this.state.workspaceRoot = this.resolveWorkspaceRoot();

    try {
      const status = await this.client.request<{
        permissionProfile?: string;
        sandboxAllowNetwork?: boolean;
        workspaceRoot?: string | null;
      }>('status').promise;

      if (typeof status.permissionProfile === 'string' && status.permissionProfile) {
        this.state.permissionProfile = status.permissionProfile;
      }
      if (typeof status.sandboxAllowNetwork === 'boolean') {
        this.state.allowNetwork = status.sandboxAllowNetwork;
      }
      if (!this.state.workspaceRoot && typeof status.workspaceRoot === 'string') {
        this.state.workspaceRoot = status.workspaceRoot;
      }
    } catch {
      // Keep configuration defaults if sidecar is not yet warm
    }

    this.postState();
  }

  private postState(): void {
    if (this.isDisposed) {
      return;
    }
    void this.panel.webview.postMessage({
      type: 'state',
      payload: this.state,
    });
  }

  private handleSwarmNotification(rawPayload: unknown, _requestId?: number): void {
    if (this.isDisposed || !isRecord(rawPayload)) {
      return;
    }

    const kindRaw = asString(rawPayload.kind ?? rawPayload.type ?? rawPayload.event);
    const kind = kindRaw.replace(/_/g, '').toLowerCase();

    if (kind === 'planready') {
      const report = normalizeMissionReport(rawPayload, this.state.mission?.goal ?? '');
      if (report) {
        this.state.mission = report;
        this.state.mergeReport = null;
        this.postState();
      }
      return;
    }

    if (kind === 'nodeupdated') {
      const node = normalizeNodeState(rawPayload.node ?? rawPayload);
      if (!node) {
        return;
      }
      if (!this.state.mission) {
        this.state.mission = {
          missionId: asString(rawPayload.missionId ?? rawPayload.mission_id, `swarm_${Date.now()}`),
          goal: '',
          waves: [],
          nodes: [node],
          blackboard: [],
          mail: [],
          conflicts: [],
          durationMs: 0,
        };
      } else {
        const idx = this.state.mission.nodes.findIndex((n) => n.id === node.id);
        if (idx >= 0) {
          this.state.mission.nodes[idx] = node;
        } else {
          this.state.mission.nodes.push(node);
        }
      }
      this.postState();
      return;
    }

    if (kind === 'workeraction') {
      const workerId = asString(rawPayload.workerId ?? rawPayload.worker_id);
      if (workerId) {
        this.state.workerActions[workerId] = {
          workerId,
          turn: asNumber(rawPayload.turn, 1),
          action: asString(rawPayload.action, 'tool'),
          detail: asString(rawPayload.detail),
          ok: asBoolean(rawPayload.ok, true),
          timestamp: Date.now(),
        };
        this.postState();
      }
      return;
    }

    if (kind === 'blackboardpublished') {
      const entry = normalizeBlackboardEntry(rawPayload.entry ?? rawPayload);
      if (entry && this.state.mission) {
        const idx = this.state.mission.blackboard.findIndex((e) => e.key === entry.key);
        if (idx >= 0) {
          this.state.mission.blackboard[idx] = entry;
        } else {
          this.state.mission.blackboard.push(entry);
        }
        this.postState();
      }
      return;
    }

    if (kind === 'mailsent') {
      const mail = normalizeMailMessage(rawPayload.mail ?? rawPayload);
      if (mail && this.state.mission) {
        this.state.mission.mail.push(mail);
        this.postState();
      }
      return;
    }

    if (kind === 'swarmcompleted') {
      const completed = normalizeMissionReport(rawPayload, this.state.mission?.goal ?? '');
      if (completed) {
        this.state.mission = completed;
        this.postState();
      }
      return;
    }

    if (kind === 'arenacandidateupdated') {
      const model = asString(rawPayload.model);
      const status = asString(rawPayload.status, 'running');
      if (model) {
        this.state.arenaStatuses[model] = status;
        this.postState();
      }
      return;
    }

    if (kind === 'arenacompleted') {
      const candidates = normalizeArenaCandidatesResponse(rawPayload);
      if (candidates.length > 0) {
        this.state.arenaCandidates = candidates;
        this.state.arenaDurationMs = asNumber(
          rawPayload.durationMs ?? rawPayload.duration_ms,
          this.state.arenaDurationMs
        );
        this.postState();
      }
    }
  }

  private async handleWebviewMessage(msg: WebviewIncomingMessage): Promise<void> {
    switch (msg.type) {
      case 'ready': {
        await this.syncEnvironmentStatus();
        break;
      }
      case 'switchTab': {
        if (msg.tab === 'swarm' || msg.tab === 'arena') {
          this.state.activeTab = msg.tab;
          this.postState();
        }
        break;
      }
      case 'launchSwarm': {
        await this.launchSwarmMission(
          asString(msg.goal).trim(),
          Math.max(1, Math.min(8, asNumber(msg.maxParallelWorkers, 4)))
        );
        break;
      }
      case 'cancelSwarm': {
        if (this.activeSwarmReqId !== null) {
          await this.client.cancel(this.activeSwarmReqId);
          this.activeSwarmReqId = null;
          this.state.swarmRunning = false;
          this.state.swarmError = 'Swarm mission cancelled by user.';
          this.postState();
        }
        break;
      }
      case 'mergeSwarm': {
        await this.mergeSwarmChanges();
        break;
      }
      case 'openCloneInEditor': {
        await this.openCloneInEditor(msg.cloneId, msg.clonePath);
        break;
      }
      case 'cleanupSwarm': {
        await this.cleanupSwarmClones();
        break;
      }
      case 'runArena': {
        const models = Array.isArray(msg.models)
          ? msg.models.map((m) => asString(m).trim()).filter(Boolean)
          : [];
        await this.runParallelArena(asString(msg.prompt).trim(), models);
        break;
      }
      case 'cancelArena': {
        if (this.activeArenaReqId !== null) {
          await this.client.cancel(this.activeArenaReqId);
          this.activeArenaReqId = null;
          this.state.arenaRunning = false;
          this.state.arenaError = 'Arena run cancelled by user.';
          this.postState();
        }
        break;
      }
      case 'viewDiff': {
        await this.openCloneFileDiff(
          asString(msg.clonePath),
          asString(msg.relPath),
          asString(msg.workerTitle, 'Sandbox Clone')
        );
        break;
      }
      case 'promoteWinner': {
        await this.promoteCloneToWorkspace(asString(msg.cloneId), asString(msg.workerTitle));
        break;
      }
      default:
        break;
    }
  }

  private async launchSwarmMission(goal: string, maxParallelWorkers: number): Promise<void> {
    if (!goal) {
      vscode.window.showWarningMessage('Enter a Swarm mission goal before launching.');
      return;
    }
    const workspaceRoot = this.resolveWorkspaceRoot();
    if (!workspaceRoot) {
      vscode.window.showWarningMessage('Open a workspace folder before launching a Swarm mission.');
      return;
    }
    if (this.state.swarmRunning) {
      return;
    }

    this.state.swarmRunning = true;
    this.state.swarmError = null;
    this.state.mergeReport = null;
    this.state.workerActions = {};
    this.state.mission = {
      missionId: `swarm_${Date.now()}`,
      goal,
      waves: [],
      nodes: [],
      blackboard: [],
      mail: [],
      conflicts: [],
      durationMs: 0,
    };
    this.postState();

    const callbacks: SwarmAwareStreamCallbacks = {
      onSwarmEvent: (payload: unknown) => {
        if (!this.hasClientEventHook) {
          this.handleSwarmNotification(payload);
        }
      },
      onEvent: (eventName: string, payload: unknown) => {
        if (!this.hasClientEventHook && eventName === 'swarm_event') {
          this.handleSwarmNotification(payload);
        }
      },
    };

    try {
      const call = this.client.request<unknown>(
        'run_swarm',
        {
          goal,
          maxParallelWorkers,
          permissionProfile: this.state.permissionProfile,
          allowNetwork: this.state.allowNetwork,
          workspaceRoot,
        },
        callbacks
      );
      this.activeSwarmReqId = call.id;
      const result = await call.promise;
      this.activeSwarmReqId = null;
      this.state.swarmRunning = false;

      const parsed = normalizeMissionReport(result, goal);
      if (parsed) {
        this.state.mission = parsed;
      }
      this.postState();

      const completedCount =
        this.state.mission?.nodes.filter((n) => n.status === 'completed').length ?? 0;
      const totalCount = this.state.mission?.nodes.length ?? 0;
      const conflictCount = this.state.mission?.conflicts.length ?? 0;
      const conflictNote =
        conflictCount > 0 ? ` (${conflictCount} overlapping file conflict(s) detected)` : '';

      void vscode.window.showInformationMessage(
        `Rursor Swarm completed: ${completedCount}/${totalCount} worker nodes succeeded${conflictNote}.`
      );
    } catch (err) {
      this.activeSwarmReqId = null;
      this.state.swarmRunning = false;
      this.state.swarmError = err instanceof Error ? err.message : String(err);
      this.postState();
    }
  }

  private async mergeSwarmChanges(): Promise<void> {
    const workspaceRoot = this.resolveWorkspaceRoot();
    if (!workspaceRoot) {
      vscode.window.showWarningMessage('No active workspace root found.');
      return;
    }
    const mission = this.state.mission;
    if (!mission || mission.nodes.length === 0) {
      vscode.window.showInformationMessage('Run a Swarm mission first to generate worker clones.');
      return;
    }

    try {
      const cloneIds = mission.nodes
        .map((n) => n.cloneId)
        .filter((id): id is string => typeof id === 'string' && id.length > 0);

      const raw = await this.client.request<unknown>('merge_swarm', {
        missionId: mission.missionId,
        nodes: mission.nodes,
        cloneIds,
        workspaceRoot,
      }).promise;

      const report = normalizeMergeReport(raw);
      this.state.mergeReport = report;
      if (report.conflicts.length > 0) {
        mission.conflicts = report.conflicts;
      }
      this.postState();

      const conflictSuffix =
        report.conflicts.length > 0
          ? ` (${report.conflicts.length} conflicting file(s) skipped safely)`
          : '';
      void vscode.window.showInformationMessage(
        `Rursor Swarm merged ${report.mergedFiles.length} non-conflicting file(s) into workspace${conflictSuffix}.`
      );
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to merge Swarm clones: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }

  private async openCloneInEditor(cloneId?: string, clonePath?: string): Promise<void> {
    let targetNode: SwarmNodeState | undefined;
    const cloneNodes = (this.state.mission?.nodes ?? []).filter(
      (n) => Boolean(n.clonePath) || Boolean(n.cloneId)
    );

    if (clonePath) {
      targetNode = cloneNodes.find((n) => n.clonePath === clonePath);
    } else if (cloneId) {
      targetNode = cloneNodes.find((n) => n.cloneId === cloneId);
    } else if (cloneNodes.length === 1) {
      targetNode = cloneNodes[0];
    } else if (cloneNodes.length > 1) {
      const pickedNode = await vscode.window.showQuickPick(
        cloneNodes.map((n) => ({
          label: `${n.title} (${n.role})`,
          description: `${n.modifiedFiles.length} modified file(s) • ${n.cloneId ?? ''}`,
          detail: n.clonePath ?? undefined,
          node: n,
        })),
        { placeHolder: 'Select an APFS Copy-on-Write worker sandbox clone to inspect' }
      );
      if (!pickedNode) {
        return;
      }
      targetNode = pickedNode.node;
    }

    const resolvedClonePath = clonePath || targetNode?.clonePath || '';
    if (!resolvedClonePath || !fs.existsSync(resolvedClonePath)) {
      vscode.window.showWarningMessage('No active APFS CoW sandbox directory exists on disk.');
      return;
    }

    const modifiedFiles = targetNode?.modifiedFiles ?? [];
    if (modifiedFiles.length > 0) {
      const items: Array<{ label: string; description: string; relPath?: string; openFolder?: boolean }> =
        modifiedFiles.map((f) => ({
          label: f.relPath,
          description: `${f.status} (+${f.addedLines} / -${f.removedLines})`,
          relPath: f.relPath,
        }));
      items.push({
        label: '$(folder-opened) Reveal Clone Directory in Finder',
        description: resolvedClonePath,
        openFolder: true,
      });

      const choice = await vscode.window.showQuickPick(items, {
        placeHolder: `Inspect files in ${targetNode?.cloneId ?? path.basename(resolvedClonePath)}`,
      });
      if (!choice) {
        return;
      }
      if (choice.openFolder) {
        await vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(resolvedClonePath));
        return;
      }
      if (choice.relPath) {
        const absTarget = path.join(resolvedClonePath, choice.relPath);
        if (fs.existsSync(absTarget)) {
          const doc = await vscode.workspace.openTextDocument(vscode.Uri.file(absTarget));
          await vscode.window.showTextDocument(doc, {
            viewColumn: vscode.ViewColumn.Beside,
            preview: false,
          });
        }
        return;
      }
    }

    await vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(resolvedClonePath));
  }

  private async cleanupSwarmClones(): Promise<void> {
    const cloneIds = [
      ...(this.state.mission?.nodes ?? [])
        .map((n) => n.cloneId)
        .filter((id): id is string => Boolean(id)),
      ...this.state.arenaCandidates.map((c) => c.cloneId).filter(Boolean),
    ];

    try {
      const res = await this.client.request<{
        destroyedCloneIds?: string[];
        destroyed?: string[] | number;
      }>('cleanup_swarm', {
        missionId: this.state.mission?.missionId,
        cloneIds,
        workspaceRoot: this.resolveWorkspaceRoot(),
      }).promise;

      const count = Array.isArray(res.destroyedCloneIds)
        ? res.destroyedCloneIds.length
        : Array.isArray(res.destroyed)
          ? res.destroyed.length
          : typeof res.destroyed === 'number'
            ? res.destroyed
            : cloneIds.length;

      if (this.state.mission) {
        for (const node of this.state.mission.nodes) {
          node.cloneId = null;
          node.clonePath = null;
        }
      }
      this.postState();
      void vscode.window.showInformationMessage(
        `Cleaned up ${count} APFS Copy-on-Write sandbox clone(s).`
      );
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to clean up Swarm clones: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }

  private async runParallelArena(prompt: string, models: string[]): Promise<void> {
    if (!prompt) {
      vscode.window.showWarningMessage('Enter an Arena challenge prompt first.');
      return;
    }
    const chosenModels = models.length > 0 ? models : [...ARENA_DEFAULT_MODELS.slice(0, 2)];
    const workspaceRoot = this.resolveWorkspaceRoot();
    if (!workspaceRoot) {
      vscode.window.showWarningMessage('Open a workspace folder before running the Model Arena.');
      return;
    }
    if (this.state.arenaRunning) {
      return;
    }

    this.state.arenaRunning = true;
    this.state.arenaError = null;
    this.state.arenaPrompt = prompt;
    this.state.arenaSelectedModels = chosenModels;
    this.state.arenaCandidates = [];
    this.state.arenaStatuses = {};
    for (const m of chosenModels) {
      this.state.arenaStatuses[m] = 'running';
    }
    this.postState();

    const startedAt = Date.now();
    const callbacks: SwarmAwareStreamCallbacks = {
      onSwarmEvent: (payload: unknown) => {
        if (!this.hasClientEventHook) {
          this.handleSwarmNotification(payload);
        }
      },
      onEvent: (eventName: string, payload: unknown) => {
        if (!this.hasClientEventHook && (eventName === 'swarm_event' || eventName === 'arena_event')) {
          this.handleSwarmNotification(payload);
        }
      },
    };

    try {
      const call = this.client.request<unknown>(
        'run_arena',
        {
          prompt,
          models: chosenModels,
          workspaceRoot,
        },
        callbacks
      );
      this.activeArenaReqId = call.id;
      const raw = await call.promise;
      this.activeArenaReqId = null;
      this.state.arenaRunning = false;
      this.state.arenaDurationMs = Date.now() - startedAt;

      const candidates = normalizeArenaCandidatesResponse(raw);
      if (candidates.length > 0) {
        this.state.arenaCandidates = candidates;
      }
      this.postState();

      const winner = this.state.arenaCandidates[0];
      if (winner) {
        void vscode.window.showInformationMessage(
          `Rursor Arena complete! Winner: ${winner.model} (score ${winner.score}, ${
            winner.verifyPassed ? 'tests passed' : 'unverified'
          }).`
        );
      } else {
        void vscode.window.showInformationMessage('Rursor Best-of-N Arena run completed.');
      }
    } catch (err) {
      this.activeArenaReqId = null;
      this.state.arenaRunning = false;
      this.state.arenaError = err instanceof Error ? err.message : String(err);
      this.postState();
    }
  }

  private async ensureEmptyScratchFile(): Promise<vscode.Uri> {
    if (!this.emptyScratchPath) {
      this.emptyScratchPath = path.join(os.tmpdir(), 'rursor-mission-empty-diff-side');
    }
    try {
      await fsp.writeFile(this.emptyScratchPath, '', 'utf8');
    } catch {
      // Ignore write error if already present
    }
    return vscode.Uri.file(this.emptyScratchPath);
  }

  private async openCloneFileDiff(
    clonePath: string,
    relPath: string,
    label: string
  ): Promise<void> {
    const workspaceRoot = this.resolveWorkspaceRoot();
    if (!workspaceRoot || !clonePath) {
      vscode.window.showWarningMessage('Cannot open diff: workspace or clone path is unavailable.');
      return;
    }

    let chosenRelPath = relPath.trim();
    if (!chosenRelPath) {
      const candidate =
        this.state.arenaCandidates.find((c) => c.clonePath === clonePath) ??
        this.state.mission?.nodes.find((n) => n.clonePath === clonePath);
      const files =
        candidate && 'diffs' in candidate
          ? candidate.diffs
          : candidate && 'modifiedFiles' in candidate
            ? candidate.modifiedFiles
            : [];
      if (files.length === 0) {
        vscode.window.showInformationMessage('No modified files recorded in this sandbox clone.');
        return;
      }
      if (files.length === 1) {
        chosenRelPath = files[0].relPath;
      } else {
        const picked = await vscode.window.showQuickPick(
          files.map((f) => ({
            label: f.relPath,
            description: `${f.status} (+${f.addedLines} / -${f.removedLines})`,
            relPath: f.relPath,
          })),
          { placeHolder: `Select file diff to inspect (${label})` }
        );
        if (!picked) {
          return;
        }
        chosenRelPath = picked.relPath;
      }
    }

    const normalizedRel = path.normalize(chosenRelPath).replace(/^(\.\.(\/|\\|$))+/, '');
    const wsAbs = path.join(workspaceRoot, normalizedRel);
    const cloneAbs = path.join(clonePath, normalizedRel);

    const wsExists = fs.existsSync(wsAbs);
    const cloneExists = fs.existsSync(cloneAbs);
    if (!wsExists && !cloneExists) {
      vscode.window.showWarningMessage(`File "${normalizedRel}" no longer exists on disk.`);
      return;
    }

    const leftUri = wsExists ? vscode.Uri.file(wsAbs) : await this.ensureEmptyScratchFile();
    const rightUri = cloneExists ? vscode.Uri.file(cloneAbs) : await this.ensureEmptyScratchFile();

    await vscode.commands.executeCommand(
      'vscode.diff',
      leftUri,
      rightUri,
      `${normalizedRel} (Workspace ↔ ${label})`
    );
  }

  private async promoteCloneToWorkspace(cloneId: string, label: string): Promise<void> {
    if (!cloneId) {
      vscode.window.showWarningMessage('Candidate does not have an active APFS CoW clone ID.');
      return;
    }

    try {
      const res = await this.client.request<{ appliedFiles?: string[] }>('apply_cow_clone', {
        cloneId,
      }).promise;
      const appliedCount = Array.isArray(res.appliedFiles) ? res.appliedFiles.length : 0;
      void vscode.window.showInformationMessage(
        `Promoted ${label || cloneId} to workspace (${appliedCount} file(s) applied).`
      );
    } catch (err) {
      vscode.window.showErrorMessage(
        `Failed to promote clone '${cloneId}': ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }

  private renderHtml(webview: vscode.Webview): string {
    const nonce = crypto.randomBytes(16).toString('base64');

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta
    http-equiv="Content-Security-Policy"
    content="default-src 'none'; style-src ${webview.cspSource} 'unsafe-inline'; script-src 'nonce-${nonce}';"
  />
  <title>Rursor Mission Control</title>
  <style>
    :root {
      --bg-obsidian: #0B0D12;
      --bg-card: #131822;
      --bg-card-elevated: #171E2B;
      --bg-input: #0E121B;
      --border-subtle: #20293A;
      --border-highlight: rgba(124, 92, 255, 0.45);
      --accent-violet: #7C5CFF;
      --accent-violet-hover: #8E72FF;
      --accent-violet-dim: rgba(124, 92, 255, 0.16);
      --text-primary: #EDF2F7;
      --text-secondary: #94A3B8;
      --text-muted: #64748B;
      --emerald: #34D399;
      --emerald-dim: rgba(52, 211, 153, 0.15);
      --cyan: #38BDF8;
      --cyan-dim: rgba(56, 189, 248, 0.15);
      --amber: #FBBF24;
      --amber-dim: rgba(251, 191, 36, 0.16);
      --rose: #FB7185;
      --rose-dim: rgba(251, 113, 133, 0.16);
      --radius-lg: 12px;
      --radius-md: 8px;
      --radius-sm: 6px;
      --font-sans: -apple-system, BlinkMacSystemFont, 'SF Pro Text', 'Inter', system-ui, sans-serif;
      --font-mono: 'SF Mono', 'JetBrains Mono', Menlo, Monaco, Consolas, monospace;
    }

    * {
      box-sizing: border-box;
    }

    body {
      margin: 0;
      padding: 0;
      background: var(--bg-obsidian);
      color: var(--text-primary);
      font-family: var(--font-sans);
      font-size: 13px;
      line-height: 1.5;
    }

    .shell {
      max-width: 1440px;
      margin: 0 auto;
      padding: 20px 24px 40px;
    }

    .topbar {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      padding-bottom: 16px;
      border-bottom: 1px solid var(--border-subtle);
      margin-bottom: 20px;
      flex-wrap: wrap;
    }

    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
    }

    .brand-orb {
      width: 34px;
      height: 34px;
      border-radius: 10px;
      background: linear-gradient(135deg, #7C5CFF 0%, #4F46E5 100%);
      display: flex;
      align-items: center;
      justify-content: center;
      font-weight: 700;
      font-size: 15px;
      color: #FFFFFF;
      box-shadow: 0 0 18px rgba(124, 92, 255, 0.38);
    }

    .brand-title {
      font-size: 16px;
      font-weight: 700;
      letter-spacing: -0.01em;
      margin: 0;
    }

    .brand-sub {
      font-size: 11.5px;
      color: var(--text-secondary);
      margin-top: 2px;
      font-family: var(--font-mono);
    }

    .tabs {
      display: inline-flex;
      background: var(--bg-card);
      border: 1px solid var(--border-subtle);
      border-radius: 10px;
      padding: 4px;
      gap: 4px;
    }

    .tab-btn {
      border: none;
      background: transparent;
      color: var(--text-secondary);
      font-family: var(--font-sans);
      font-size: 12.5px;
      font-weight: 600;
      padding: 7px 14px;
      border-radius: 7px;
      cursor: pointer;
      transition: all 0.15s ease;
    }

    .tab-btn:hover {
      color: var(--text-primary);
    }

    .tab-btn.active {
      background: var(--accent-violet);
      color: #FFFFFF;
      box-shadow: 0 2px 10px rgba(124, 92, 255, 0.35);
    }

    .card {
      background: var(--bg-card);
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-lg);
      padding: 16px 18px;
      margin-bottom: 16px;
    }

    .card-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      margin-bottom: 12px;
      flex-wrap: wrap;
    }

    .card-title {
      font-size: 13.5px;
      font-weight: 600;
      color: var(--text-primary);
      margin: 0;
      display: flex;
      align-items: center;
      gap: 8px;
    }

    textarea {
      width: 100%;
      min-height: 82px;
      resize: vertical;
      background: var(--bg-input);
      color: var(--text-primary);
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-md);
      padding: 10px 12px;
      font-family: var(--font-sans);
      font-size: 13px;
      outline: none;
      transition: border-color 0.15s ease;
    }

    textarea:focus {
      border-color: var(--accent-violet);
    }

    select {
      background: var(--bg-input);
      color: var(--text-primary);
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-sm);
      padding: 6px 10px;
      font-size: 12.5px;
      outline: none;
    }

    .controls-row {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      margin-top: 12px;
      flex-wrap: wrap;
    }

    .controls-left {
      display: flex;
      align-items: center;
      gap: 12px;
      flex-wrap: wrap;
    }

    .control-group {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      font-size: 12px;
      color: var(--text-secondary);
    }

    .badge {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      padding: 3px 9px;
      border-radius: 999px;
      font-size: 11px;
      font-weight: 600;
      letter-spacing: 0.02em;
      border: 1px solid transparent;
    }

    .badge-violet {
      background: var(--accent-violet-dim);
      color: #C4B5FD;
      border-color: rgba(124, 92, 255, 0.35);
    }

    .badge-emerald {
      background: var(--emerald-dim);
      color: var(--emerald);
      border-color: rgba(52, 211, 153, 0.3);
    }

    .badge-cyan {
      background: var(--cyan-dim);
      color: var(--cyan);
      border-color: rgba(56, 189, 248, 0.3);
    }

    .badge-amber {
      background: var(--amber-dim);
      color: var(--amber);
      border-color: rgba(251, 191, 36, 0.3);
    }

    .badge-rose {
      background: var(--rose-dim);
      color: var(--rose);
      border-color: rgba(251, 113, 133, 0.3);
    }

    .badge-muted {
      background: rgba(148, 163, 184, 0.12);
      color: var(--text-secondary);
      border-color: rgba(148, 163, 184, 0.2);
    }

    .btn {
      display: inline-flex;
      align-items: center;
      gap: 7px;
      border: 1px solid var(--border-subtle);
      background: var(--bg-card-elevated);
      color: var(--text-primary);
      padding: 7px 14px;
      border-radius: var(--radius-md);
      font-size: 12.5px;
      font-weight: 600;
      cursor: pointer;
      transition: all 0.15s ease;
    }

    .btn:hover:not(:disabled) {
      border-color: var(--accent-violet);
      background: #1B2434;
    }

    .btn:disabled {
      opacity: 0.5;
      cursor: not-allowed;
    }

    .btn-primary {
      background: var(--accent-violet);
      border-color: var(--accent-violet);
      color: #FFFFFF;
      box-shadow: 0 2px 12px rgba(124, 92, 255, 0.32);
    }

    .btn-primary:hover:not(:disabled) {
      background: var(--accent-violet-hover);
      border-color: var(--accent-violet-hover);
    }

    .btn-danger {
      border-color: rgba(251, 113, 133, 0.4);
      color: var(--rose);
    }

    .btn-sm {
      padding: 4px 9px;
      font-size: 11.5px;
      border-radius: var(--radius-sm);
    }

    .swimlanes {
      display: grid;
      grid-auto-flow: column;
      grid-auto-columns: minmax(290px, 1fr);
      gap: 14px;
      overflow-x: auto;
      padding-bottom: 6px;
    }

    .wave-col {
      background: #0E131C;
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-lg);
      padding: 12px;
      display: flex;
      flex-direction: column;
      gap: 10px;
    }

    .wave-header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      font-size: 11.5px;
      font-weight: 700;
      text-transform: uppercase;
      letter-spacing: 0.06em;
      color: var(--text-secondary);
      padding-bottom: 8px;
      border-bottom: 1px solid var(--border-subtle);
    }

    .node-card {
      background: var(--bg-card);
      border: 1px solid var(--border-subtle);
      border-radius: 10px;
      padding: 12px;
      display: flex;
      flex-direction: column;
      gap: 8px;
    }

    .node-card.status-running,
    .node-card.status-verifying {
      border-color: var(--accent-violet);
      box-shadow: 0 0 0 1px rgba(124, 92, 255, 0.25);
    }

    .node-card.status-completed {
      border-color: rgba(52, 211, 153, 0.35);
    }

    .node-card.status-failed {
      border-color: rgba(251, 113, 133, 0.45);
    }

    .node-top {
      display: flex;
      align-items: flex-start;
      justify-content: space-between;
      gap: 8px;
    }

    .node-title {
      font-weight: 600;
      font-size: 13px;
      color: var(--text-primary);
    }

    .node-id {
      font-family: var(--font-mono);
      font-size: 11px;
      color: var(--text-muted);
    }

    .meta-pills {
      display: flex;
      align-items: center;
      gap: 6px;
      flex-wrap: wrap;
    }

    .mono-tag {
      font-family: var(--font-mono);
      font-size: 11px;
      background: var(--bg-input);
      border: 1px solid var(--border-subtle);
      color: var(--text-secondary);
      padding: 2px 7px;
      border-radius: 5px;
    }

    .file-pills {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }

    .file-pill {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      font-family: var(--font-mono);
      font-size: 11px;
      padding: 3px 8px;
      border-radius: 6px;
      background: var(--bg-input);
      border: 1px solid var(--border-subtle);
      color: var(--text-primary);
      cursor: pointer;
    }

    .file-pill:hover {
      border-color: var(--accent-violet);
    }

    .diff-add {
      color: var(--emerald);
      font-weight: 600;
    }

    .diff-del {
      color: var(--rose);
      font-weight: 600;
    }

    .live-ticker {
      font-family: var(--font-mono);
      font-size: 11px;
      color: var(--cyan);
      background: rgba(56, 189, 248, 0.08);
      border-left: 2px solid var(--cyan);
      padding: 4px 8px;
      border-radius: 4px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    details summary {
      cursor: pointer;
      font-size: 11.5px;
      color: var(--text-secondary);
      user-select: none;
    }

    details pre,
    .summary-box {
      margin: 6px 0 0;
      padding: 8px 10px;
      background: var(--bg-input);
      border: 1px solid var(--border-subtle);
      border-radius: 6px;
      font-size: 11.5px;
      white-space: pre-wrap;
      word-break: break-word;
      color: var(--text-primary);
      max-height: 200px;
      overflow-y: auto;
    }

    .split-feed {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(340px, 1fr));
      gap: 16px;
    }

    .feed-list {
      display: flex;
      flex-direction: column;
      gap: 8px;
      max-height: 260px;
      overflow-y: auto;
    }

    .feed-item {
      background: var(--bg-input);
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-md);
      padding: 9px 11px;
      font-size: 12px;
    }

    .feed-item-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin-bottom: 4px;
      font-family: var(--font-mono);
      font-size: 11px;
      color: var(--text-secondary);
    }

    .action-bar {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      flex-wrap: wrap;
    }

    .action-buttons {
      display: flex;
      align-items: center;
      gap: 10px;
      flex-wrap: wrap;
    }

    .model-checks {
      display: flex;
      align-items: center;
      gap: 10px;
      flex-wrap: wrap;
      margin-top: 12px;
    }

    .model-check {
      display: inline-flex;
      align-items: center;
      gap: 7px;
      background: var(--bg-input);
      border: 1px solid var(--border-subtle);
      padding: 6px 11px;
      border-radius: var(--radius-md);
      cursor: pointer;
      font-family: var(--font-mono);
      font-size: 12px;
    }

    .model-check input {
      accent-color: var(--accent-violet);
    }

    .arena-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
      gap: 16px;
    }

    .arena-card {
      background: var(--bg-card);
      border: 1px solid var(--border-subtle);
      border-radius: var(--radius-lg);
      padding: 16px;
      display: flex;
      flex-direction: column;
      gap: 11px;
      position: relative;
    }

    .arena-card.winner {
      border-color: var(--accent-violet);
      box-shadow: 0 0 24px rgba(124, 92, 255, 0.22);
    }

    .winner-crown {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      background: linear-gradient(135deg, #7C5CFF 0%, #F59E0B 100%);
      color: #FFFFFF;
      font-size: 11px;
      font-weight: 800;
      letter-spacing: 0.06em;
      padding: 3px 9px;
      border-radius: 999px;
    }

    .alert-banner {
      padding: 10px 14px;
      border-radius: var(--radius-md);
      margin-bottom: 14px;
      font-size: 12.5px;
    }

    .alert-error {
      background: var(--rose-dim);
      border: 1px solid rgba(251, 113, 133, 0.4);
      color: var(--rose);
    }

    .alert-warn {
      background: var(--amber-dim);
      border: 1px solid rgba(251, 191, 36, 0.4);
      color: var(--amber);
    }

    .alert-success {
      background: var(--emerald-dim);
      border: 1px solid rgba(52, 211, 153, 0.4);
      color: var(--emerald);
    }

    .empty-state {
      text-align: center;
      padding: 28px 16px;
      color: var(--text-muted);
      font-size: 12.5px;
    }

    .hidden {
      display: none !important;
    }
  </style>
</head>
<body>
  <div class="shell">
    <header class="topbar">
      <div class="brand">
        <div class="brand-orb">M</div>
        <div>
          <h1 class="brand-title">Rursor Mission Control</h1>
          <div class="brand-sub" id="workspaceLabel">Initializing workspace sandbox...</div>
        </div>
      </div>
      <nav class="tabs" role="tablist">
        <button class="tab-btn active" id="tabBtnSwarm" data-action="switch-tab" data-tab="swarm">
          Swarm Mission DAG
        </button>
        <button class="tab-btn" id="tabBtnArena" data-action="switch-tab" data-tab="arena">
          Best-of-N Model Arena
        </button>
      </nav>
    </header>

    <!-- TAB 1: SWARM MISSION DAG -->
    <section id="viewSwarm">
      <div class="card">
        <div class="card-header">
          <h2 class="card-title">Autonomous Multi-Agent Swarm Composer</h2>
          <span class="badge badge-violet" id="profileBadge">Seatbelt: safe-auto</span>
        </div>
        <textarea
          id="swarmGoalInput"
          placeholder="Describe a multi-file engineering goal (e.g., Design shared auth contracts, implement API handlers and UI views in parallel APFS CoW clones, and run verification checks)..."
        ></textarea>
        <div class="controls-row">
          <div class="controls-left">
            <label class="control-group" for="maxWorkersSelect">
              <span>Max Parallel Workers:</span>
              <select id="maxWorkersSelect">
                <option value="1">1 Worker</option>
                <option value="2">2 Workers</option>
                <option value="3">3 Workers</option>
                <option value="4" selected>4 Workers</option>
                <option value="6">6 Workers</option>
              </select>
            </label>
            <span class="badge badge-cyan" id="cowSummaryBadge">APFS CoW Isolation</span>
          </div>
          <div class="action-buttons">
            <button class="btn btn-danger hidden" id="btnCancelSwarm" data-action="cancel-swarm">
              Stop Swarm
            </button>
            <button class="btn btn-primary" id="btnLaunchSwarm" data-action="launch-swarm">
              Launch Autonomous Swarm
            </button>
          </div>
        </div>
      </div>

      <div id="swarmAlerts"></div>

      <div class="card">
        <div class="card-header">
          <h2 class="card-title">Live Topological DAG Swimlanes</h2>
          <span class="badge badge-muted" id="dagStatsBadge">Idle</span>
        </div>
        <div id="dagSwimlanesContainer">
          <div class="empty-state">
            Launch an autonomous Swarm mission above to decompose your goal into parallel APFS Copy-on-Write worker waves.
          </div>
        </div>
      </div>

      <div class="split-feed">
        <div class="card">
          <div class="card-header">
            <h2 class="card-title">Shared Swarm Blackboard</h2>
            <span class="badge badge-violet" id="blackboardCountBadge">0 entries</span>
          </div>
          <div class="feed-list" id="blackboardFeed">
            <div class="empty-state">Published API contracts, ports, and schemas appear here live.</div>
          </div>
        </div>

        <div class="card">
          <div class="card-header">
            <h2 class="card-title">Inter-Agent Mailbox</h2>
            <span class="badge badge-cyan" id="mailboxCountBadge">0 messages</span>
          </div>
          <div class="feed-list" id="mailboxFeed">
            <div class="empty-state">Real-time sibling coordination messages appear here live.</div>
          </div>
        </div>
      </div>

      <div class="card">
        <div class="action-bar">
          <div>
            <div class="card-title">Workspace Clone Integration</div>
            <div class="brand-sub">Non-conflicting CoW worker changes merge atomically into your workspace.</div>
          </div>
          <div class="action-buttons">
            <button class="btn btn-primary" id="btnMergeSwarm" data-action="merge-swarm">
              Merge Non-Conflicting Changes
            </button>
            <button class="btn" id="btnOpenClone" data-action="open-clone">
              Open Clone in Editor
            </button>
            <button class="btn btn-danger" id="btnCleanupSwarm" data-action="cleanup-swarm">
              Clean Up Swarm Clones
            </button>
          </div>
        </div>
      </div>
    </section>

    <!-- TAB 2: BEST-OF-N MODEL ARENA -->
    <section id="viewArena" class="hidden">
      <div class="card">
        <div class="card-header">
          <h2 class="card-title">Best-of-N Parallel Model Arena</h2>
          <span class="badge badge-violet">Separate APFS CoW Clones + Auto-Verify Scoring</span>
        </div>
        <textarea
          id="arenaPromptInput"
          placeholder="Enter a coding task or bugfix to benchmark models head-to-head in isolated APFS CoW sandboxes..."
        ></textarea>
        <div class="model-checks" id="arenaModelChecks">
          <label class="model-check">
            <input type="checkbox" value="gemini-3.1-pro-preview" checked />
            <span>gemini-3.1-pro-preview</span>
          </label>
          <label class="model-check">
            <input type="checkbox" value="gemini-3.8-flash" checked />
            <span>gemini-3.8-flash</span>
          </label>
          <label class="model-check">
            <input type="checkbox" value="claude-sonnet-4-20250514" />
            <span>claude-sonnet-4-20250514</span>
          </label>
          <label class="model-check">
            <input type="checkbox" value="gpt-4.1" />
            <span>gpt-4.1</span>
          </label>
        </div>
        <div class="controls-row">
          <div class="brand-sub" id="arenaMetaLabel">
            Deterministic scoring: Verify Passed (+1000) • Completed (+200) • Non-Empty Diff (+100) • Latency Tie-Breaker
          </div>
          <div class="action-buttons">
            <button class="btn btn-danger hidden" id="btnCancelArena" data-action="cancel-arena">
              Stop Arena
            </button>
            <button class="btn btn-primary" id="btnRunArena" data-action="run-arena">
              Run Parallel Arena
            </button>
          </div>
        </div>
      </div>

      <div id="arenaAlerts"></div>

      <div id="arenaResultsContainer">
        <div class="card empty-state">
          Select 2 or more frontier models and click "Run Parallel Arena" to compare verified implementations side-by-side.
        </div>
      </div>
    </section>
  </div>

  <script nonce="${nonce}">
    (function () {
      const vscode = acquireVsCodeApi();

      function escapeHtml(raw) {
        const str = raw === null || raw === undefined ? '' : String(raw);
        return str
          .replace(/&/g, '&amp;')
          .replace(/</g, '&lt;')
          .replace(/>/g, '&gt;')
          .replace(/"/g, '&quot;')
          .replace(/'/g, '&#39;');
      }

      function formatDuration(ms) {
        const n = Number(ms) || 0;
        if (n <= 0) {
          return '—';
        }
        if (n < 1000) {
          return n + 'ms';
        }
        return (n / 1000).toFixed(2) + 's';
      }

      function statusBadgeClass(status) {
        const s = String(status || 'pending').toLowerCase();
        if (s === 'completed') return 'badge-emerald';
        if (s === 'running' || s === 'verifying') return 'badge-violet';
        if (s === 'failed') return 'badge-rose';
        if (s === 'blocked') return 'badge-amber';
        return 'badge-muted';
      }

      function statusDisplayLabel(status) {
        const s = String(status || 'pending').toLowerCase();
        if (s === 'running') return 'Running';
        if (s === 'verifying') return 'Verifying';
        if (s === 'completed') return 'Completed';
        if (s === 'failed') return 'Failed';
        if (s === 'blocked') return 'Blocked';
        return 'Pending';
      }

      function roleBadgeClass(role) {
        const r = String(role || 'coder').toLowerCase();
        if (r === 'planner') return 'badge-amber';
        if (r === 'researcher') return 'badge-cyan';
        if (r === 'verifier') return 'badge-emerald';
        if (r === 'mediaproducer') return 'badge-violet';
        return 'badge-violet';
      }

      function computeWavesFromNodes(mission) {
        if (!mission || !Array.isArray(mission.nodes) || mission.nodes.length === 0) {
          return [];
        }
        const nodeById = {};
        for (let i = 0; i < mission.nodes.length; i++) {
          const n = mission.nodes[i];
          nodeById[n.id] = n;
        }

        if (Array.isArray(mission.waves) && mission.waves.length > 0) {
          const mapped = [];
          for (let w = 0; w < mission.waves.length; w++) {
            const waveSpecs = mission.waves[w] || [];
            const col = [];
            for (let j = 0; j < waveSpecs.length; j++) {
              const spec = waveSpecs[j];
              if (nodeById[spec.id]) {
                col.push(nodeById[spec.id]);
              }
            }
            if (col.length > 0) {
              mapped.push(col);
            }
          }
          if (mapped.length > 0) {
            return mapped;
          }
        }

        // Topological level fallback directly from dependsOn
        const depths = {};
        function getDepth(id, stack) {
          if (depths[id] !== undefined) return depths[id];
          if (stack[id]) return 0;
          stack[id] = true;
          const node = nodeById[id];
          let d = 0;
          if (node && Array.isArray(node.dependsOn)) {
            for (let k = 0; k < node.dependsOn.length; k++) {
              const depDepth = getDepth(node.dependsOn[k], stack);
              if (depDepth + 1 > d) d = depDepth + 1;
            }
          }
          depths[id] = d;
          return d;
        }

        const buckets = [];
        for (let i = 0; i < mission.nodes.length; i++) {
          const n = mission.nodes[i];
          const lv = getDepth(n.id, {});
          if (!buckets[lv]) buckets[lv] = [];
          buckets[lv].push(n);
        }
        return buckets.filter(Boolean);
      }

      function renderVerifyBadge(node) {
        const results = Array.isArray(node.verifyResults) ? node.verifyResults : [];
        if (results.length === 0) {
          if (node.cloneId) {
            return '<span class="badge badge-muted">No Verify Script</span>';
          }
          return '<span class="badge badge-muted">Read-Only</span>';
        }
        let passed = 0;
        for (let i = 0; i < results.length; i++) {
          if (results[i].exitCode === 0) passed++;
        }
        if (passed === results.length) {
          return '<span class="badge badge-emerald">Verify ' + passed + '/' + results.length + ' Passed</span>';
        }
        return '<span class="badge badge-rose">Verify ' + passed + '/' + results.length + ' Passed</span>';
      }

      function renderSwarmAlerts(state) {
        const container = document.getElementById('swarmAlerts');
        let html = '';

        if (state.swarmError) {
          html += '<div class="alert-banner alert-error">' + escapeHtml(state.swarmError) + '</div>';
        }

        const conflicts =
          (state.mergeReport && state.mergeReport.conflicts && state.mergeReport.conflicts.length > 0
            ? state.mergeReport.conflicts
            : state.mission && state.mission.conflicts) || [];

        if (conflicts.length > 0) {
          const parts = [];
          for (let i = 0; i < conflicts.length; i++) {
            const c = conflicts[i];
            parts.push(
              escapeHtml(c.relPath) + ' (' + escapeHtml((c.workerIds || []).join(', ')) + ')'
            );
          }
          html +=
            '<div class="alert-banner alert-warn">Overlapping worker edits detected on ' +
            conflicts.length +
            ' file(s): ' +
            parts.join(' • ') +
            '. Non-conflicting files can still be merged safely.</div>';
        }

        if (state.mergeReport && state.mergeReport.mergedFiles && state.mergeReport.mergedFiles.length > 0) {
          html +=
            '<div class="alert-banner alert-success">Merged ' +
            state.mergeReport.mergedFiles.length +
            ' file(s) into workspace: ' +
            escapeHtml(state.mergeReport.mergedFiles.join(', ')) +
            '</div>';
        }

        container.innerHTML = html;
      }

      function renderSwimlanes(state) {
        const container = document.getElementById('dagSwimlanesContainer');
        const statsBadge = document.getElementById('dagStatsBadge');
        const mission = state.mission;

        if (!mission || !Array.isArray(mission.nodes) || mission.nodes.length === 0) {
          statsBadge.textContent = state.swarmRunning ? 'Decomposing Goal into DAG...' : 'Idle';
          container.innerHTML =
            '<div class="empty-state">' +
            (state.swarmRunning
              ? 'SmartRouter is decomposing your mission goal into parallel APFS Copy-on-Write execution waves...'
              : 'Launch an autonomous Swarm mission above to decompose your goal into parallel APFS Copy-on-Write worker waves.') +
            '</div>';
          return;
        }

        let doneCount = 0;
        for (let i = 0; i < mission.nodes.length; i++) {
          if (mission.nodes[i].status === 'completed') doneCount++;
        }
        statsBadge.textContent =
          doneCount + '/' + mission.nodes.length + ' Completed' +
          (mission.durationMs ? ' • ' + formatDuration(mission.durationMs) : '');

        const waves = computeWavesFromNodes(mission);
        let html = '<div class="swimlanes">';

        for (let w = 0; w < waves.length; w++) {
          const waveNodes = waves[w];
          html += '<div class="wave-col">';
          html +=
            '<div class="wave-header"><span>Wave ' +
            (w + 1) +
            '</span><span>' +
            waveNodes.length +
            ' worker' +
            (waveNodes.length === 1 ? '' : 's') +
            '</span></div>';

          for (let j = 0; j < waveNodes.length; j++) {
            const node = waveNodes[j];
            const statusStr = String(node.status || 'pending').toLowerCase();
            const liveAction = state.workerActions ? state.workerActions[node.id] : null;
            const files = Array.isArray(node.modifiedFiles) ? node.modifiedFiles : [];

            html += '<div class="node-card status-' + escapeHtml(statusStr) + '">';
            html += '<div class="node-top">';
            html += '<div>';
            html += '<div class="node-title">' + escapeHtml(node.title) + '</div>';
            html += '<div class="node-id">#' + escapeHtml(node.id) + '</div>';
            html += '</div>';
            html +=
              '<span class="badge ' +
              statusBadgeClass(statusStr) +
              '">' +
              escapeHtml(statusDisplayLabel(statusStr)) +
              '</span>';
            html += '</div>';

            html += '<div class="meta-pills">';
            html +=
              '<span class="badge ' +
              roleBadgeClass(node.role) +
              '">' +
              escapeHtml(node.role) +
              '</span>';
            html += renderVerifyBadge(node);
            if (node.durationMs > 0) {
              html += '<span class="mono-tag">' + escapeHtml(formatDuration(node.durationMs)) + '</span>';
            }
            html += '</div>';

            if (Array.isArray(node.dependsOn) && node.dependsOn.length > 0) {
              html +=
                '<div class="node-id">Depends on: ' +
                escapeHtml(node.dependsOn.join(', ')) +
                '</div>';
            }

            if (node.cloneId) {
              html += '<div class="meta-pills">';
              html += '<span class="mono-tag" title="APFS Copy-on-Write Clone">' + escapeHtml(node.cloneId) + '</span>';
              if (node.clonePath) {
                html +=
                  '<button class="btn btn-sm" data-action="open-clone" data-clone-id="' +
                  escapeHtml(node.cloneId) +
                  '" data-clone-path="' +
                  escapeHtml(node.clonePath) +
                  '">Open Clone</button>';
              }
              html += '</div>';
            }

            if (liveAction && (statusStr === 'running' || statusStr === 'verifying')) {
              html +=
                '<div class="live-ticker">T' +
                escapeHtml(liveAction.turn) +
                ' ' +
                escapeHtml(liveAction.action) +
                ': ' +
                escapeHtml(liveAction.detail) +
                '</div>';
            }

            if (files.length > 0) {
              html += '<div class="file-pills">';
              for (let f = 0; f < files.length; f++) {
                const df = files[f];
                html +=
                  '<button class="file-pill" data-action="view-diff" data-clone-path="' +
                  escapeHtml(node.clonePath || '') +
                  '" data-rel-path="' +
                  escapeHtml(df.relPath) +
                  '" data-worker-title="' +
                  escapeHtml(node.title) +
                  '">' +
                  '<span>' +
                  escapeHtml(df.relPath) +
                  '</span>' +
                  '<span class="diff-add">+' +
                  escapeHtml(df.addedLines) +
                  '</span>' +
                  '<span class="diff-del">-' +
                  escapeHtml(df.removedLines) +
                  '</span>' +
                  '</button>';
              }
              html += '</div>';
            }

            if (node.summary || node.error) {
              html += '<details>';
              html += '<summary>Worker Summary & Verification Log</summary>';
              const detailBody = (node.summary || '') + (node.error ? '\n\nError:\n' + node.error : '');
              html += '<pre>' + escapeHtml(detailBody.trim()) + '</pre>';
              html += '</details>';
            }

            html += '</div>';
          }

          html += '</div>';
        }

        html += '</div>';
        container.innerHTML = html;
      }

      function renderBlackboardAndMail(state) {
        const bbFeed = document.getElementById('blackboardFeed');
        const mailFeed = document.getElementById('mailboxFeed');
        const bbBadge = document.getElementById('blackboardCountBadge');
        const mailBadge = document.getElementById('mailboxCountBadge');

        const entries = (state.mission && state.mission.blackboard) || [];
        const mails = (state.mission && state.mission.mail) || [];

        bbBadge.textContent = entries.length + (entries.length === 1 ? ' entry' : ' entries');
        mailBadge.textContent = mails.length + (mails.length === 1 ? ' message' : ' messages');

        if (entries.length === 0) {
          bbFeed.innerHTML =
            '<div class="empty-state">Published API contracts, ports, and schemas appear here live.</div>';
        } else {
          let html = '';
          for (let i = 0; i < entries.length; i++) {
            const e = entries[i];
            html += '<div class="feed-item">';
            html += '<div class="feed-item-head">';
            html += '<span class="badge badge-violet">' + escapeHtml(e.key) + '</span>';
            html += '<span>by #' + escapeHtml(e.authorId) + '</span>';
            html += '</div>';
            html += '<div>' + escapeHtml(e.value) + '</div>';
            html += '</div>';
          }
          bbFeed.innerHTML = html;
        }

        if (mails.length === 0) {
          mailFeed.innerHTML =
            '<div class="empty-state">Real-time sibling coordination messages appear here live.</div>';
        } else {
          let html = '';
          for (let i = mails.length - 1; i >= 0; i--) {
            const m = mails[i];
            html += '<div class="feed-item">';
            html += '<div class="feed-item-head">';
            html +=
              '<span>#' +
              escapeHtml(m.fromId) +
              ' → #' +
              escapeHtml(m.toId) +
              '</span>';
            html += '</div>';
            html += '<div>' + escapeHtml(m.message) + '</div>';
            html += '</div>';
          }
          mailFeed.innerHTML = html;
        }
      }

      function renderArena(state) {
        const alertsEl = document.getElementById('arenaAlerts');
        const container = document.getElementById('arenaResultsContainer');

        alertsEl.innerHTML = state.arenaError
          ? '<div class="alert-banner alert-error">' + escapeHtml(state.arenaError) + '</div>'
          : '';

        const candidates = Array.isArray(state.arenaCandidates) ? state.arenaCandidates : [];
        if (candidates.length === 0) {
          if (state.arenaRunning) {
            const models = state.arenaSelectedModels || [];
            let liveHtml = '<div class="arena-grid">';
            for (let i = 0; i < models.length; i++) {
              const m = models[i];
              const st = (state.arenaStatuses && state.arenaStatuses[m]) || 'running';
              liveHtml += '<div class="arena-card">';
              liveHtml += '<div class="node-top">';
              liveHtml += '<div class="node-title">' + escapeHtml(m) + '</div>';
              liveHtml += '<span class="badge badge-violet">' + escapeHtml(st) + '</span>';
              liveHtml += '</div>';
              liveHtml +=
                '<div class="brand-sub">Executing in isolated APFS Copy-on-Write sandbox & running auto-verify...</div>';
              liveHtml += '</div>';
            }
            liveHtml += '</div>';
            container.innerHTML = liveHtml;
          } else {
            container.innerHTML =
              '<div class="card empty-state">Select 2 or more frontier models and click "Run Parallel Arena" to compare verified implementations side-by-side.</div>';
          }
          return;
        }

        let html = '<div class="arena-grid">';
        for (let idx = 0; idx < candidates.length; idx++) {
          const c = candidates[idx];
          const isWinner = idx === 0 && (c.ok || c.score > 0);
          let totalAdded = 0;
          let totalRemoved = 0;
          const diffs = Array.isArray(c.diffs) ? c.diffs : [];
          for (let d = 0; d < diffs.length; d++) {
            totalAdded += Number(diffs[d].addedLines) || 0;
            totalRemoved += Number(diffs[d].removedLines) || 0;
          }

          html += '<div class="arena-card' + (isWinner ? ' winner' : '') + '">';
          html += '<div class="node-top">';
          html += '<div>';
          html += '<div class="node-title">' + escapeHtml(c.model) + '</div>';
          html += '<div class="node-id">' + escapeHtml(c.provider) + ' • ' + escapeHtml(c.cloneId || 'no-clone') + '</div>';
          html += '</div>';
          if (isWinner) {
            html += '<span class="winner-crown">👑 WINNER</span>';
          } else {
            html += '<span class="badge badge-muted">Rank #' + (idx + 1) + '</span>';
          }
          html += '</div>';

          html += '<div class="meta-pills">';
          html += '<span class="badge badge-violet">Score: ' + escapeHtml(c.score) + '</span>';
          if (c.verifyPassed) {
            html += '<span class="badge badge-emerald">TESTS PASSED</span>';
          } else if (c.ok) {
            html += '<span class="badge badge-rose">TESTS FAILED</span>';
          } else {
            html += '<span class="badge badge-rose">RUN FAILED</span>';
          }
          html += '<span class="mono-tag">' + escapeHtml(formatDuration(c.durationMs)) + '</span>';
          html +=
            '<span class="mono-tag"><span class="diff-add">+' +
            totalAdded +
            '</span> / <span class="diff-del">-' +
            totalRemoved +
            '</span></span>';
          html += '</div>';

          if (diffs.length > 0) {
            html += '<div class="file-pills">';
            for (let d = 0; d < diffs.length; d++) {
              const df = diffs[d];
              html +=
                '<button class="file-pill" data-action="view-diff" data-clone-path="' +
                escapeHtml(c.clonePath) +
                '" data-rel-path="' +
                escapeHtml(df.relPath) +
                '" data-worker-title="' +
                escapeHtml(c.model) +
                '">' +
                '<span>' +
                escapeHtml(df.relPath) +
                '</span>' +
                '<span class="diff-add">+' +
                escapeHtml(df.addedLines) +
                '</span>' +
                '<span class="diff-del">-' +
                escapeHtml(df.removedLines) +
                '</span>' +
                '</button>';
            }
            html += '</div>';
          }

          if (c.summary) {
            html += '<div class="summary-box">' + escapeHtml(c.summary) + '</div>';
          }

          html += '<div class="action-buttons">';
          html +=
            '<button class="btn btn-sm" data-action="view-diff" data-clone-path="' +
            escapeHtml(c.clonePath) +
            '" data-rel-path="' +
            escapeHtml(diffs[0] ? diffs[0].relPath : '') +
            '" data-worker-title="' +
            escapeHtml(c.model) +
            '"' +
            (diffs.length === 0 ? ' disabled' : '') +
            '>View Diff</button>';
          html +=
            '<button class="btn btn-sm ' +
            (isWinner ? 'btn-primary' : '') +
            '" data-action="promote-winner" data-clone-id="' +
            escapeHtml(c.cloneId) +
            '" data-worker-title="' +
            escapeHtml(c.model) +
            '"' +
            (!c.cloneId || diffs.length === 0 ? ' disabled' : '') +
            '>' +
            (isWinner ? 'Promote Winner to Workspace' : 'Promote to Workspace') +
            '</button>';
          html += '</div>';

          html += '</div>';
        }
        html += '</div>';
        container.innerHTML = html;
      }

      function applyState(state) {
        document.getElementById('workspaceLabel').textContent = state.workspaceRoot
          ? 'Workspace: ' + state.workspaceRoot
          : 'No workspace folder open';

        document.getElementById('profileBadge').textContent =
          'Seatbelt: ' + (state.permissionProfile || 'safe-auto');

        const isSwarm = state.activeTab !== 'arena';
        document.getElementById('tabBtnSwarm').classList.toggle('active', isSwarm);
        document.getElementById('tabBtnArena').classList.toggle('active', !isSwarm);
        document.getElementById('viewSwarm').classList.toggle('hidden', !isSwarm);
        document.getElementById('viewArena').classList.toggle('hidden', isSwarm);

        document.getElementById('btnLaunchSwarm').disabled = Boolean(state.swarmRunning);
        document.getElementById('btnCancelSwarm').classList.toggle('hidden', !state.swarmRunning);
        document.getElementById('btnRunArena').disabled = Boolean(state.arenaRunning);
        document.getElementById('btnCancelArena').classList.toggle('hidden', !state.arenaRunning);

        let activeClones = 0;
        if (state.mission && Array.isArray(state.mission.nodes)) {
          for (let i = 0; i < state.mission.nodes.length; i++) {
            if (state.mission.nodes[i].cloneId) activeClones++;
          }
        }
        document.getElementById('cowSummaryBadge').textContent =
          activeClones > 0 ? activeClones + ' Active APFS CoW Clones' : 'APFS CoW Isolation';

        renderSwarmAlerts(state);
        renderSwimlanes(state);
        renderBlackboardAndMail(state);
        renderArena(state);
      }

      document.addEventListener('click', function (ev) {
        const target = ev.target && ev.target.closest ? ev.target.closest('[data-action]') : null;
        if (!target) return;

        const action = target.getAttribute('data-action');
        if (action === 'switch-tab') {
          vscode.postMessage({ type: 'switchTab', tab: target.getAttribute('data-tab') });
        } else if (action === 'launch-swarm') {
          const goal = document.getElementById('swarmGoalInput').value || '';
          const maxWorkers = Number(document.getElementById('maxWorkersSelect').value) || 4;
          vscode.postMessage({
            type: 'launchSwarm',
            goal: goal,
            maxParallelWorkers: maxWorkers,
          });
        } else if (action === 'cancel-swarm') {
          vscode.postMessage({ type: 'cancelSwarm' });
        } else if (action === 'merge-swarm') {
          vscode.postMessage({ type: 'mergeSwarm' });
        } else if (action === 'open-clone') {
          vscode.postMessage({
            type: 'openCloneInEditor',
            cloneId: target.getAttribute('data-clone-id') || '',
            clonePath: target.getAttribute('data-clone-path') || '',
          });
        } else if (action === 'cleanup-swarm') {
          vscode.postMessage({ type: 'cleanupSwarm' });
        } else if (action === 'run-arena') {
          const prompt = document.getElementById('arenaPromptInput').value || '';
          const checks = document.querySelectorAll('#arenaModelChecks input[type="checkbox"]:checked');
          const models = [];
          for (let i = 0; i < checks.length; i++) {
            models.push(checks[i].value);
          }
          vscode.postMessage({ type: 'runArena', prompt: prompt, models: models });
        } else if (action === 'cancel-arena') {
          vscode.postMessage({ type: 'cancelArena' });
        } else if (action === 'view-diff') {
          vscode.postMessage({
            type: 'viewDiff',
            clonePath: target.getAttribute('data-clone-path') || '',
            relPath: target.getAttribute('data-rel-path') || '',
            workerTitle: target.getAttribute('data-worker-title') || '',
          });
        } else if (action === 'promote-winner') {
          vscode.postMessage({
            type: 'promoteWinner',
            cloneId: target.getAttribute('data-clone-id') || '',
            workerTitle: target.getAttribute('data-worker-title') || '',
          });
        }
      });

      window.addEventListener('message', function (event) {
        const msg = event.data;
        if (msg && msg.type === 'state' && msg.payload) {
          applyState(msg.payload);
        }
      });

      vscode.postMessage({ type: 'ready' });
    })();
  </script>
</body>
</html>`;
  }

  public dispose(): void {
    if (this.isDisposed) {
      return;
    }
    this.isDisposed = true;
    if (MissionControlPanel.currentPanel === this) {
      MissionControlPanel.currentPanel = undefined;
    }
    if (this.activeSwarmReqId !== null) {
      void this.client.cancel(this.activeSwarmReqId);
    }
    if (this.activeArenaReqId !== null) {
      void this.client.cancel(this.activeArenaReqId);
    }
    this.panel.dispose();
    while (this.disposables.length > 0) {
      const d = this.disposables.pop();
      d?.dispose();
    }
  }
}
