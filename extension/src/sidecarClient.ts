import * as fs from 'node:fs';
import * as path from 'node:path';
import { ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import * as vscode from 'vscode';

export interface RpcResponse<T = Record<string, unknown>> {
  id: number;
  ok: boolean;
  result?: T;
  error?: string;
}

export interface ToolEventPayload {
  callId: string;
  phase: 'start' | 'done';
  name: string;
  args?: Record<string, unknown>;
  ok?: boolean;
  output?: Record<string, unknown>;
}

export interface ToolRequestPayload {
  callId: string;
  name: 'edit_file' | 'write_file' | 'run_command' | string;
  args: Record<string, unknown>;
}

export interface StreamCallbacks {
  onStream?: (text: string) => void;
  onToolEvent?: (payload: ToolEventPayload) => void;
  onToolRequest?: (payload: ToolRequestPayload) => void;
}

interface PendingCall {
  resolve: (value: unknown) => void;
  reject: (reason?: unknown) => void;
  callbacks?: StreamCallbacks;
}

export class SidecarClient implements vscode.Disposable {
  private proc: ChildProcessWithoutNullStreams | undefined;
  private buffer = '';
  private nextId = 1;
  private pending = new Map<number, PendingCall>();
  private readonly outputChannel = vscode.window.createOutputChannel('Rursor AI');

  constructor(private readonly extensionPath: string) {}

  private resolveBinaryPath(): string {
    const candidates = [
      path.resolve(this.extensionPath, 'bin', 'rursor-sidecar'),
      path.resolve(this.extensionPath, '..', 'sidecar', 'target', 'release', 'rursor-sidecar'),
      path.resolve(this.extensionPath, '..', 'sidecar', 'target', 'debug', 'rursor-sidecar'),
    ];
    for (const candidate of candidates) {
      if (fs.existsSync(candidate)) {
        return candidate;
      }
    }
    return candidates[2];
  }

  start(): void {
    if (this.proc) {
      return;
    }

    const binPath = this.resolveBinaryPath();
    this.outputChannel.appendLine(`[sidecar] starting ${binPath}`);
    this.proc = spawn(binPath, [], { stdio: ['pipe', 'pipe', 'pipe'] });

    this.proc.stdout.on('data', (chunk: Buffer) => {
      this.buffer += chunk.toString('utf8');
      let idx = this.buffer.indexOf('\n');
      while (idx !== -1) {
        const line = this.buffer.slice(0, idx).trim();
        this.buffer = this.buffer.slice(idx + 1);
        if (line) {
          this.handleMessage(line);
        }
        idx = this.buffer.indexOf('\n');
      }
    });

    this.proc.stderr.on('data', (chunk: Buffer) => {
      const text = chunk.toString('utf8').trim();
      if (text) {
        this.outputChannel.appendLine(`[sidecar:stderr] ${text}`);
      }
    });

    this.proc.on('exit', (code, signal) => {
      this.outputChannel.appendLine(`[sidecar] exited (code=${code}, signal=${signal})`);
      for (const call of this.pending.values()) {
        call.reject(new Error('Rursor sidecar exited unexpectedly.'));
      }
      this.pending.clear();
      this.proc = undefined;
    });
  }

  private handleMessage(raw: string): void {
    let parsed: RpcResponse & { event?: string; requestId?: number; payload?: unknown };
    try {
      parsed = JSON.parse(raw) as RpcResponse & {
        event?: string;
        requestId?: number;
        payload?: unknown;
      };
    } catch {
      return;
    }

    if (parsed.event && typeof parsed.requestId === 'number') {
      const call = this.pending.get(parsed.requestId);
      if (!call?.callbacks) {
        return;
      }
      if (parsed.event === 'stream_chunk') {
        const payload = parsed.payload as { text?: string } | undefined;
        if (payload?.text) {
          call.callbacks.onStream?.(payload.text);
        }
      } else if (parsed.event === 'tool_event') {
        call.callbacks.onToolEvent?.(parsed.payload as ToolEventPayload);
      } else if (parsed.event === 'tool_request') {
        call.callbacks.onToolRequest?.(parsed.payload as ToolRequestPayload);
      }
      return;
    }

    if (typeof parsed.id === 'number') {
      const call = this.pending.get(parsed.id);
      if (!call) {
        return;
      }
      this.pending.delete(parsed.id);
      if (parsed.ok) {
        call.resolve(parsed.result ?? {});
      } else {
        call.reject(new Error(parsed.error ?? 'Unknown sidecar error'));
      }
    }
  }

  request<T = Record<string, unknown>>(
    method: string,
    params: Record<string, unknown> = {},
    callbacks?: StreamCallbacks | ((text: string) => void)
  ): { id: number; promise: Promise<T> } {
    this.start();
    const id = this.nextId++;
    const normalizedCallbacks: StreamCallbacks | undefined =
      typeof callbacks === 'function' ? { onStream: callbacks } : callbacks;

    const promise = new Promise<T>((resolve, reject) => {
      if (!this.proc) {
        reject(new Error('Failed to start rursor-sidecar.'));
        return;
      }
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
        callbacks: normalizedCallbacks,
      });
      this.proc.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    });
    return { id, promise };
  }

  async submitToolResult(
    callId: string,
    ok: boolean,
    output: Record<string, unknown>
  ): Promise<void> {
    const { promise } = this.request('submit_tool_result', { callId, ok, output });
    await promise.catch((err) => {
      this.outputChannel.appendLine(`[sidecar] submit_tool_result failed: ${String(err)}`);
    });
  }

  async cancel(requestId: number): Promise<void> {
    if (!this.proc) {
      return;
    }
    const { promise } = this.request('cancel', { requestId });
    await promise.catch(() => undefined);
  }

  dispose(): void {
    this.proc?.kill();
    this.proc = undefined;
    this.pending.clear();
    this.outputChannel.dispose();
  }
}
