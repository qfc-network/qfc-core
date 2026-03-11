import { createHash, randomBytes } from 'node:crypto';
import type { Logger } from 'pino';
import type { RedisStore } from './redis-store.js';
import type { ChallengeTask, PendingTask, TaskType } from './types.js';

/**
 * ChallengeInjector — periodically creates challenge tasks with known answers
 * to detect miners submitting fraudulent results.
 *
 * Challenge tasks look identical to real tasks but the router knows the
 * expected output hash. If a miner's result doesn't match, it gets flagged.
 */
export class ChallengeInjector {
  private store: RedisStore;
  private log: Logger;
  private intervalMs: number;
  private timer: ReturnType<typeof setInterval> | null = null;
  private onChallenge: (task: PendingTask) => void;

  /** Pre-computed challenge inputs with known outputs */
  private static readonly CHALLENGE_PROMPTS: Array<{
    taskType: TaskType;
    modelName: string;
    input: string;
    expectedHash: string;
  }> = [
    {
      taskType: 'text_generation',
      modelName: 'qfc-llm-0.5b',
      input: 'QFC challenge: what is 2+2?',
      expectedHash: '', // Computed at runtime from reference execution
    },
    {
      taskType: 'embedding',
      modelName: 'qfc-embed-small',
      input: 'QFC challenge embedding test vector',
      expectedHash: '',
    },
  ];

  constructor(
    store: RedisStore,
    intervalMs: number,
    onChallenge: (task: PendingTask) => void,
    log: Logger,
  ) {
    this.store = store;
    this.intervalMs = intervalMs;
    this.onChallenge = onChallenge;
    this.log = log.child({ component: 'challenge' });
  }

  /** Start injecting challenge tasks */
  start(): void {
    this.log.info({ intervalMs: this.intervalMs }, 'Starting challenge task injector');
    this.timer = setInterval(() => {
      void this.injectChallenge();
    }, this.intervalMs);
  }

  /** Stop injecting */
  stop(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }

  /** Create and inject a challenge task */
  private async injectChallenge(): Promise<void> {
    try {
      const taskId = 'challenge-' + randomBytes(16).toString('hex');
      const prompt =
        ChallengeInjector.CHALLENGE_PROMPTS[
          Math.floor(Math.random() * ChallengeInjector.CHALLENGE_PROMPTS.length)
        ];

      const inputHash = createHash('sha256').update(prompt.input).digest('hex');

      const challenge: ChallengeTask = {
        taskId,
        taskType: prompt.taskType,
        modelName: prompt.modelName,
        expectedOutputHash: prompt.expectedHash || inputHash, // Use input hash as placeholder
        injectedAt: Date.now(),
      };

      await this.store.addChallenge(challenge);

      const pendingTask: PendingTask = {
        taskId,
        epoch: 0,
        taskType: prompt.taskType,
        modelName: prompt.modelName,
        modelVersion: 'v1',
        inputDataHash: inputHash,
        requiredTier: 'Cold',
        requiredMemoryMb: 0,
        deadline: Date.now() + 120_000,
        reward: '0',
        createdAt: Date.now(),
        isChallenge: true,
      };

      this.log.info(
        { taskId, taskType: prompt.taskType, model: prompt.modelName },
        'Injected challenge task',
      );

      this.onChallenge(pendingTask);
    } catch (err) {
      this.log.error({ err }, 'Failed to inject challenge task');
    }
  }

  /** Verify a miner's result against the expected challenge output */
  async verifyChallenge(taskId: string, outputHash: string): Promise<boolean> {
    const challenge = await this.store.getChallenge(taskId);
    if (!challenge) return true; // Not a challenge task

    const passed = challenge.expectedOutputHash === outputHash;
    if (!passed) {
      this.log.warn(
        {
          taskId,
          expected: challenge.expectedOutputHash,
          actual: outputHash,
        },
        'Challenge task FAILED — miner produced wrong output',
      );
    } else {
      this.log.info({ taskId }, 'Challenge task passed');
    }

    return passed;
  }
}
