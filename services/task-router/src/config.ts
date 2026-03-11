import 'dotenv/config';

export interface RouterConfig {
  /** WebSocket RPC URL for chain subscriptions */
  rpcWsUrl: string;
  /** HTTP RPC URL for JSON-RPC calls */
  rpcHttpUrl: string;
  /** Redis connection URL */
  redisUrl: string;
  /** Private key for signing claim transactions */
  routerPrivateKey: string;
  /** Interval between challenge task injections (ms) */
  challengeIntervalMs: number;
  /** Task execution timeout before reassignment (ms) */
  taskTimeoutMs: number;
  /** Log level */
  logLevel: string;
  /** Maximum concurrent tasks per miner */
  maxTasksPerMiner: number;
  /** Task poll interval when WebSocket is unavailable (ms) */
  pollIntervalMs: number;
}

export function loadConfig(): RouterConfig {
  return {
    rpcWsUrl: process.env.QFC_RPC_URL ?? 'ws://127.0.0.1:8546',
    rpcHttpUrl: process.env.QFC_RPC_HTTP ?? 'http://127.0.0.1:8545',
    redisUrl: process.env.REDIS_URL ?? 'redis://127.0.0.1:6379',
    routerPrivateKey: process.env.ROUTER_PRIVATE_KEY ?? '',
    challengeIntervalMs: parseInt(process.env.CHALLENGE_TASK_INTERVAL_MS ?? '300000', 10),
    taskTimeoutMs: parseInt(process.env.TASK_TIMEOUT_MS ?? '120000', 10),
    logLevel: process.env.LOG_LEVEL ?? 'info',
    maxTasksPerMiner: parseInt(process.env.MAX_TASKS_PER_MINER ?? '3', 10),
    pollIntervalMs: parseInt(process.env.POLL_INTERVAL_MS ?? '5000', 10),
  };
}
