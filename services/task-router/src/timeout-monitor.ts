import type { Logger } from 'pino';
import type { RedisStore } from './redis-store.js';

/**
 * TimeoutMonitor — periodically checks for tasks that have exceeded their
 * deadline and triggers reassignment.
 */
export class TimeoutMonitor {
  private store: RedisStore;
  private log: Logger;
  private timeoutMs: number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private onTimeout: (taskId: string, minerAddress: string) => void;

  constructor(
    store: RedisStore,
    timeoutMs: number,
    onTimeout: (taskId: string, minerAddress: string) => void,
    log: Logger,
  ) {
    this.store = store;
    this.timeoutMs = timeoutMs;
    this.onTimeout = onTimeout;
    this.log = log.child({ component: 'timeout-monitor' });
  }

  /** Start monitoring */
  start(): void {
    // Check every 10% of timeout period, minimum 5s
    const checkInterval = Math.max(5000, Math.floor(this.timeoutMs / 10));
    this.log.info({ checkIntervalMs: checkInterval, timeoutMs: this.timeoutMs }, 'Starting timeout monitor');

    this.timer = setInterval(() => {
      void this.checkTimeouts();
    }, checkInterval);
  }

  /** Stop monitoring */
  stop(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Check for timed-out task assignments */
  private async checkTimeouts(): Promise<void> {
    try {
      const taskIds = await this.store.getPendingTaskIds();
      const now = Date.now();

      for (const taskId of taskIds) {
        const assignment = await this.store.getAssignment(taskId);
        if (!assignment) continue;

        const elapsed = now - assignment.assignedAt;
        if (elapsed > this.timeoutMs) {
          this.log.warn(
            {
              taskId,
              miner: assignment.minerAddress,
              elapsedMs: elapsed,
              attempts: assignment.attempts,
            },
            'Task assignment timed out',
          );

          // Decrement miner's active task count
          await this.store.decrementMinerTasks(assignment.minerAddress);
          // Remove stale assignment
          await this.store.removeAssignment(taskId);

          // Notify router for reassignment
          this.onTimeout(taskId, assignment.minerAddress);
        }
      }
    } catch (err) {
      this.log.error({ err }, 'Error checking timeouts');
    }
  }
}
