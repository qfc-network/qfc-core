import type { Logger } from 'pino';
import type { RouterConfig } from './config.js';
import { ChainWatcher } from './chain-watcher.js';
import { RedisStore } from './redis-store.js';
import { TaskMatcher } from './matcher.js';
import { TimeoutMonitor } from './timeout-monitor.js';
import { ChallengeInjector } from './challenge.js';
import type { PendingTask, TaskAssignment } from './types.js';

/**
 * TaskRouter — the main orchestrator.
 *
 * Responsibilities:
 * 1. Watch TaskRegistry for new pending tasks (via ChainWatcher)
 * 2. Match tasks to miners by tier + availability (via TaskMatcher)
 * 3. Call claimTask() on behalf of matched miner (via chain RPC)
 * 4. Monitor for timeouts → trigger reassignment (via TimeoutMonitor)
 * 5. Inject challenge tasks periodically (via ChallengeInjector)
 */
export class TaskRouter {
  private config: RouterConfig;
  private log: Logger;
  private store: RedisStore;
  private watcher: ChainWatcher;
  private matcher: TaskMatcher;
  private timeoutMonitor: TimeoutMonitor;
  private challengeInjector: ChallengeInjector;

  /** Tasks currently being processed (dedup) */
  private processingTasks = new Set<string>();

  /** Stats */
  private stats = {
    tasksRouted: 0,
    tasksTimedOut: 0,
    challengesInjected: 0,
    challengesFailed: 0,
    startedAt: Date.now(),
  };

  constructor(config: RouterConfig, log: Logger) {
    this.config = config;
    this.log = log.child({ component: 'router' });

    // Initialize components
    this.store = new RedisStore(config.redisUrl, log);

    this.watcher = new ChainWatcher(
      config.rpcWsUrl,
      config.rpcHttpUrl,
      config.pollIntervalMs,
      (task) => void this.handleNewTask(task),
      log,
    );

    this.matcher = new TaskMatcher(config.maxTasksPerMiner, log);

    this.timeoutMonitor = new TimeoutMonitor(
      this.store,
      config.taskTimeoutMs,
      (taskId, minerAddress) => void this.handleTimeout(taskId, minerAddress),
      log,
    );

    this.challengeInjector = new ChallengeInjector(
      this.store,
      config.challengeIntervalMs,
      (task) => void this.handleNewTask(task),
      log,
    );
  }

  /** Start the router */
  async start(): Promise<void> {
    this.log.info('Starting QFC Task Router');

    // Start all components
    this.watcher.start();
    this.timeoutMonitor.start();
    this.challengeInjector.start();

    // Log periodic stats
    setInterval(() => {
      const uptime = Math.floor((Date.now() - this.stats.startedAt) / 1000);
      this.log.info(
        {
          routed: this.stats.tasksRouted,
          timedOut: this.stats.tasksTimedOut,
          challenges: this.stats.challengesInjected,
          challengesFailed: this.stats.challengesFailed,
          uptimeSeconds: uptime,
        },
        'Router stats',
      );
    }, 60_000);

    this.log.info('Task Router started');
  }

  /** Stop the router gracefully */
  async stop(): Promise<void> {
    this.log.info('Stopping Task Router');
    this.watcher.stop();
    this.timeoutMonitor.stop();
    this.challengeInjector.stop();
    await this.store.close();
    this.log.info('Task Router stopped');
  }

  /** Handle a new pending task (from chain or challenge injection) */
  private async handleNewTask(task: PendingTask): Promise<void> {
    // Dedup: skip if already processing
    if (this.processingTasks.has(task.taskId)) return;

    // Skip expired tasks
    if (task.deadline > 0 && Date.now() > task.deadline) {
      this.log.debug({ taskId: task.taskId }, 'Skipping expired task');
      return;
    }

    this.processingTasks.add(task.taskId);

    try {
      // Store task
      await this.store.addPendingTask(task);

      if (task.isChallenge) {
        this.stats.challengesInjected++;
      }

      // Find best miner
      const miners = await this.store.getOnlineMiners();
      const match = this.matcher.match(task, miners);

      if (!match) {
        this.log.debug(
          { taskId: task.taskId, onlineMiners: miners.length },
          'No miner available, task stays pending',
        );
        return;
      }

      // Create assignment
      const assignment: TaskAssignment = {
        taskId: task.taskId,
        minerAddress: match.minerAddress,
        assignedAt: Date.now(),
        deadline: task.deadline,
        attempts: 1,
      };

      await this.store.setAssignment(assignment);
      await this.store.incrementMinerTasks(match.minerAddress);

      // Claim task on-chain on behalf of the miner
      await this.claimTask(task.taskId, match.minerAddress);

      this.stats.tasksRouted++;
      this.log.info(
        {
          taskId: task.taskId,
          miner: match.minerAddress,
          taskType: task.taskType,
          model: task.modelName,
        },
        'Task routed to miner',
      );
    } catch (err) {
      this.log.error({ err, taskId: task.taskId }, 'Error routing task');
    } finally {
      this.processingTasks.delete(task.taskId);
    }
  }

  /** Handle a timed-out task — reassign to another miner */
  private async handleTimeout(taskId: string, previousMiner: string): Promise<void> {
    this.stats.tasksTimedOut++;

    const task = await this.store.getPendingTask(taskId);
    if (!task) {
      this.log.debug({ taskId }, 'Timed-out task no longer pending');
      return;
    }

    // Skip if past deadline
    if (task.deadline > 0 && Date.now() > task.deadline) {
      this.log.info({ taskId }, 'Timed-out task past deadline, removing');
      await this.store.removePendingTask(taskId);
      return;
    }

    // Find a different miner
    const miners = await this.store.getOnlineMiners();
    const availableMiners = miners.filter((m) => m.address !== previousMiner);
    const match = this.matcher.match(task, availableMiners);

    if (!match) {
      this.log.warn({ taskId }, 'No alternative miner for reassignment');
      return;
    }

    const assignment: TaskAssignment = {
      taskId,
      minerAddress: match.minerAddress,
      assignedAt: Date.now(),
      deadline: task.deadline,
      attempts: 2,
    };

    await this.store.setAssignment(assignment);
    await this.store.incrementMinerTasks(match.minerAddress);
    await this.claimTask(taskId, match.minerAddress);

    this.log.info(
      { taskId, newMiner: match.minerAddress, previousMiner },
      'Task reassigned after timeout',
    );
  }

  /** Call claimTask RPC on behalf of the miner */
  private async claimTask(taskId: string, minerAddress: string): Promise<void> {
    try {
      await this.watcher.rpcCall('qfc_claimInferenceTask', [
        { taskId, minerAddress },
      ]);
    } catch (err) {
      this.log.error({ err, taskId, minerAddress }, 'Failed to claim task on-chain');
      throw err;
    }
  }
}
