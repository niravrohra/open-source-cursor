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

export interface StreamEvent {
  event: string;
  requestId: number;
  payload: { text?: string };
}

interface PendingCall {
  resolve: (value: unknown) => void;
  reject: (reason?: unknown) => void;
  onStream?: (text: string) => void;
}

export class SidecarClient implements vscode.Disposable {
  private proc: ChildProcessWithoutNullStreams | undefined;
  private buffer = '';
  private nextId = 1;
  private pending = new Map<number, PendingCall>();

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

    this.proc.on('exit', () => {
      for (const call of this.pending.values()) {
        call.reject(new Error('Rursor sidecar exited unexpectedly.'));
      }
      this.pending.clear();
      this.proc = undefined;
    });
  }

  private handleMessage(raw: string): void {
    let parsed: RpcResponse & Partial<StreamEvent>;
    try {
      parsed = JSON.parse(raw) as RpcResponse & Partial<StreamEvent>;
    } catch {
      return;
    }

    if (parsed.event === 'stream_chunk' && typeof parsed.requestId === 'number') {
      const call = this.pending.get(parsed.requestId);
      if (call?.onStream && parsed.payload?.text) {
        call.onStream(parsed.payload.text);
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
    onStream?: (text: string) => void
  ): { id: number; promise: Promise<T> } {
    this.start();
    const id = this.nextId++;
    const promise = new Promise<T>((resolve, reject) => {
      if (!this.proc) {
        reject(new Error('Failed to start rursor-sidecar.'));
        return;
      }
      this.pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
        onStream,
      });
      this.proc.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    });
    return { id, promise };
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
  }
}
