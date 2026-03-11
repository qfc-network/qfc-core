import type { Logger } from 'pino';
import type { PendingTask, MinerState, MatchResult, GpuTier } from './types.js';

/** Tier priority ordering: Hot > Warm > Cold */
const TIER_RANK: Record<GpuTier, number> = { Hot: 3, Warm: 2, Cold: 1 };

/**
 * TaskMatcher — matches pending tasks to the best available miner
 * based on GPU tier, memory, model availability, reputation, and load.
 */
export class TaskMatcher {
  private log: Logger;
  private maxTasksPerMiner: number;

  constructor(maxTasksPerMiner: number, log: Logger) {
    this.maxTasksPerMiner = maxTasksPerMiner;
    this.log = log.child({ component: 'matcher' });
  }

  /**
   * Find the best miner for a given task.
   * Returns null if no suitable miner is available.
   */
  match(task: PendingTask, miners: MinerState[]): MatchResult | null {
    const candidates: MatchResult[] = [];

    for (const miner of miners) {
      if (!miner.online) continue;
      if (miner.activeTasks >= this.maxTasksPerMiner) continue;

      // Tier check: miner tier must meet or exceed task requirement
      if (TIER_RANK[miner.gpuTier] < TIER_RANK[task.requiredTier]) continue;

      // Memory check
      if (task.requiredMemoryMb > 0 && miner.availableMemoryMb < task.requiredMemoryMb) continue;

      // Score the candidate
      const score = this.scoreMiner(task, miner);
      candidates.push({
        taskId: task.taskId,
        minerAddress: miner.address,
        score,
      });
    }

    if (candidates.length === 0) {
      this.log.debug(
        { taskId: task.taskId, taskType: task.taskType, requiredTier: task.requiredTier },
        'No suitable miner found',
      );
      return null;
    }

    // Sort by score descending and pick the best
    candidates.sort((a, b) => b.score - a.score);

    const best = candidates[0];
    this.log.info(
      {
        taskId: task.taskId,
        miner: best.minerAddress,
        score: best.score,
        candidates: candidates.length,
      },
      'Task matched',
    );

    return best;
  }

  /**
   * Score a miner for a task. Higher is better.
   *
   * Scoring factors:
   * - Model already loaded: +50 (hot cache = faster execution)
   * - Reputation: +0-30 (reputation / 333)
   * - Available capacity: +0-10 (fewer active tasks = more capacity)
   * - Tier match: +0-10 (exact match preferred over overshoot)
   */
  private scoreMiner(task: PendingTask, miner: MinerState): number {
    let score = 0;

    // Model loaded bonus (biggest factor: avoids model load time)
    if (miner.loadedModels.includes(task.modelName)) {
      score += 50;
    }

    // Reputation score (0-10000 → 0-30)
    score += Math.floor(miner.reputation / 333);

    // Capacity score: fewer active tasks = higher score
    const capacityRatio = 1 - miner.activeTasks / this.maxTasksPerMiner;
    score += Math.floor(capacityRatio * 10);

    // Tier match: exact match gets +10, overshoot gets +5
    if (TIER_RANK[miner.gpuTier] === TIER_RANK[task.requiredTier]) {
      score += 10;
    } else {
      score += 5;
    }

    return score;
  }
}
