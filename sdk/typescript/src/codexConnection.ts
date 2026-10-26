import { spawn } from 'node:child_process';
import { Duplex } from 'node:stream';
import WebSocket from 'ws';

export interface CodexConnectionConfig {
  codexArgv: string[];
  socket?: string;
}

export interface CodexNotification {
  method: string;
  params?: unknown;
}

export interface CodexConnection {
  initialized: unknown;
  request<T>(method: string, params: unknown): Promise<T>;
  close(): Promise<void>;
}

export function validateCodexConnectionConfig(config: CodexConnectionConfig): void {
  if (!Array.isArray(config.codexArgv) || !config.codexArgv.length
      || config.codexArgv.some(arg => typeof arg !== 'string' || !arg || arg.includes('\0'))
      || (config.socket !== undefined && typeof config.socket !== 'string')) {
    throw new Error('Invalid native Codex connection configuration');
  }
}

/** Connect to an existing native server. Never start, resume or discover one. */
export async function connectCodex(
  config: CodexConnectionConfig, onNotification?: (notification: CodexNotification) => void,
): Promise<CodexConnection> {
  validateCodexConnectionConfig(config);
  const child = spawn(config.codexArgv[0], [...config.codexArgv.slice(1), 'app-server', 'proxy',
    ...(config.socket ? ['--sock', config.socket] : [])], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true, shell: false });
  const wire = Duplex.from({ readable: child.stdout, writable: child.stdin });
  const socket = new WebSocket('ws://localhost/', { createConnection: () => wire, handshakeTimeout: 10000 });
  let stderr = '';
  child.stderr.on('data', bytes => { stderr = (stderr+String(bytes)).slice(-4096); });
  let serial = 0;
  let failure: Error | undefined;
  let closed = false;
  const pending = new Map<number, { resolve(value: unknown): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }>();
  const fail = (error: Error) => {
    failure ??= error;
    for (const value of pending.values()) { clearTimeout(value.timer); value.reject(error); }
    pending.clear();
  };
  child.on('error', fail);
  child.stdin.on('error', fail);
  child.on('close', code => fail(new Error(`Codex proxy exited ${code}: ${stderr}`)));
  socket.on('error', fail);
  socket.on('close', () => fail(new Error('Codex proxy closed; request outcome may be unknown')));
  socket.on('message', bytes => {
    try {
      const message = JSON.parse(bytes.toString()) as { id?: number; method?: string; params?: unknown; error?: unknown; result?: unknown };
      if (message.id === undefined) {
        if (typeof message.method === 'string') onNotification?.({ method: message.method, params: message.params });
        return;
      }
      const value = pending.get(message.id);
      if (!value) return;
      pending.delete(message.id);
      clearTimeout(value.timer);
      if (message.error) value.reject(new Error(JSON.stringify(message.error)));
      else value.resolve(message.result);
    } catch (error) { fail(new Error(`Invalid Codex proxy response: ${String(error)}`)); }
  });
  const request: CodexConnection['request'] = <T>(method: string, params: unknown): Promise<T> => {
    if (closed || failure || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(failure ?? new Error('Codex proxy is not connected'));
    }
    return new Promise<T>((resolve, reject) => {
      const id = ++serial;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${method} timed out; outcome unknown, no retry attempted`));
      }, 15000);
      pending.set(id, { resolve: value => resolve(value as T), reject, timer });
      socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }), error => { if (error) fail(error); });
    });
  };
  const close = async () => {
    if (closed) return;
    closed = true;
    fail(new Error('Codex connection owner closed'));
    socket.terminate();
    wire.destroy();
    child.kill(); // This is only our proxy, never the native server or frontend.
  };
  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(failure ?? new Error('Codex proxy handshake timed out')), 11000);
      socket.once('open', () => { clearTimeout(timer); resolve(); });
      socket.once('error', error => { clearTimeout(timer); reject(error); });
      child.once('error', error => { clearTimeout(timer); reject(error); });
    });
    const initialized = await request('initialize', { clientInfo: { name: 'scode-acp-callback', version: '1.0.0' }, capabilities: { experimentalApi: true } });
    socket.send(JSON.stringify({ jsonrpc: '2.0', method: 'initialized' }));
    return { initialized, request, close };
  } catch (error) { await close(); throw error; }
}
