import WebSocket from 'ws';
import type { Logger } from 'pino';
import type { PendingTask, GpuTier, TaskType } from './types.js';

/** JSON-RPC request */
interface JsonRpcRequest {
  jsonrpc: '2.0';
  method: string;
  params: unknown[];
  id: number;
}

/** JSON-RPC response */
interface JsonRpcResponse {
  jsonrpc: '2.0';
  result?: unknown;
  error?: { code: number; message: string };
  id: number;
}

/** Event emitted when a new task is detected */
export type TaskCallback = (task: PendingTask) => void;

/**
 * ChainWatcher — subscribes to QFC node via WebSocket for new pending tasks.
 * Falls back to HTTP polling if WebSocket is unavailable.
 */
export class ChainWatcher {
  private wsUrl: string;
  private httpUrl: string;
  private ws: WebSocket | null = null;
  private log: Logger;
  private onTask: TaskCallback;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pollTimer: ReturnType<typeof setInterval> | null = null;
  private pollIntervalMs: number;
  private rpcId = 1;
  private lastSeenEpoch = 0;
  private stopped = false;

  constructor(
    wsUrl: string,
    httpUrl: string,
    pollIntervalMs: number,
    onTask: TaskCallback,
    log: Logger,
  ) {
    this.wsUrl = wsUrl;
    this.httpUrl = httpUrl;
    this.pollIntervalMs = pollIntervalMs;
    this.onTask = onTask;
    this.log = log.child({ component: 'chain-watcher' });
  }

  /** Start watching the chain */
  start(): void {
    this.stopped = false;
    this.connectWebSocket();
  }

  /** Stop watching */
  stop(): void {
    this.stopped = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }

  private connectWebSocket(): void {
    if (this.stopped) return;

    this.log.info({ url: this.wsUrl }, 'Connecting to QFC node via WebSocket');

    try {
      this.ws = new WebSocket(this.wsUrl);
    } catch (err) {
      this.log.warn({ err }, 'WebSocket creation failed, falling back to polling');
      this.startPolling();
      return;
    }

    this.ws.on('open', () => {
      this.log.info('WebSocket connected');
      // Subscribe to new block events
      this.sendWs({
        jsonrpc: '2.0',
        method: 'eth_subscribe',
        params: ['newHeads'],
        id: this.rpcId++,
      });
    });

    this.ws.on('message', (data: WebSocket.Data) => {
      try {
        const msg = JSON.parse(data.toString());
        this.handleWsMessage(msg);
      } catch (err) {
        this.log.warn({ err }, 'Failed to parse WebSocket message');
      }
    });

    this.ws.on('error', (err) => {
      this.log.warn({ err }, 'WebSocket error');
    });

    this.ws.on('close', () => {
      this.log.info('WebSocket disconnected');
      this.ws = null;
      if (!this.stopped) {
        this.reconnectTimer = setTimeout(() => this.connectWebSocket(), 5000);
        // Start polling as fallback while reconnecting
        this.startPolling();
      }
    });
  }

  private handleWsMessage(msg: Record<string, unknown>): void {
    // Handle subscription notifications (new block)
    if (msg.method === 'eth_subscription' && msg.params) {
      const params = msg.params as { result?: { number?: string } };
      const blockNumber = params.result?.number;
      if (blockNumber) {
        this.log.debug({ blockNumber }, 'New block');
        // Poll for pending tasks on each new block
        void this.pollPendingTasks();
      }
    }
  }

  private startPolling(): void {
    if (this.pollTimer) return;
    this.log.info({ intervalMs: this.pollIntervalMs }, 'Starting HTTP poll fallback');
    this.pollTimer = setInterval(() => {
      void this.pollPendingTasks();
    }, this.pollIntervalMs);
  }

  /** Poll the node for pending inference tasks */
  async pollPendingTasks(): Promise<void> {
    try {
      const response = await fetch(this.httpUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0',
          method: 'qfc_getPendingInferenceTasks',
          params: [],
          id: this.rpcId++,
        }),
      });

      const json = (await response.json()) as JsonRpcResponse;
      if (json.error) {
        this.log.warn({ error: json.error }, 'RPC error fetching pending tasks');
        return;
      }

      const tasks = json.result as RawPendingTask[] | null;
      if (!tasks || !Array.isArray(tasks)) return;

      for (const raw of tasks) {
        const task = parsePendingTask(raw);
        if (task) {
          this.onTask(task);
        }
      }
    } catch (err) {
      this.log.warn({ err }, 'Failed to poll pending tasks');
    }
  }

  private sendWs(msg: JsonRpcRequest): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  /** Make an HTTP JSON-RPC call */
  async rpcCall<T>(method: string, params: unknown[] = []): Promise<T | null> {
    const response = await fetch(this.httpUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0',
        method,
        params,
        id: this.rpcId++,
      }),
    });

    const json = (await response.json()) as JsonRpcResponse;
    if (json.error) {
      throw new Error(`RPC error: ${json.error.message}`);
    }
    return (json.result as T) ?? null;
  }
}

/** Raw task format from RPC */
interface RawPendingTask {
  taskId: string;
  epoch: number | string;
  taskType: string;
  modelName: string;
  modelVersion: string;
  inputDataHash: string;
  requiredTier?: string;
  requiredMemoryMb?: number;
  deadline: number | string;
  reward?: string;
}

function parsePendingTask(raw: RawPendingTask): PendingTask | null {
  if (!raw.taskId || !raw.taskType) return null;

  const tier: GpuTier =
    raw.requiredTier === 'Hot' ? 'Hot' : raw.requiredTier === 'Warm' ? 'Warm' : 'Cold';

  return {
    taskId: raw.taskId,
    epoch: typeof raw.epoch === 'string' ? parseInt(raw.epoch, 10) : raw.epoch,
    taskType: raw.taskType as TaskType,
    modelName: raw.modelName,
    modelVersion: raw.modelVersion,
    inputDataHash: raw.inputDataHash,
    requiredTier: tier,
    requiredMemoryMb: raw.requiredMemoryMb ?? 0,
    deadline: typeof raw.deadline === 'string' ? parseInt(raw.deadline, 10) : raw.deadline,
    reward: raw.reward ?? '0',
    createdAt: Date.now(),
    isChallenge: false,
  };
}
