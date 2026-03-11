import Redis from 'ioredis';
import type { Logger } from 'pino';
import type { MinerState, TaskAssignment, PendingTask, ChallengeTask } from './types.js';

const MINER_KEY_PREFIX = 'qfc:miner:';
const TASK_KEY_PREFIX = 'qfc:task:';
const ASSIGNMENT_KEY_PREFIX = 'qfc:assign:';
const CHALLENGE_KEY_PREFIX = 'qfc:challenge:';
const MINERS_SET = 'qfc:miners:online';
const PENDING_TASKS_SET = 'qfc:tasks:pending';

/** Redis-backed state store for miner availability and task assignments */
export class RedisStore {
  private redis: Redis;
  private log: Logger;

  constructor(redisUrl: string, log: Logger) {
    this.redis = new Redis(redisUrl, {
      maxRetriesPerRequest: 3,
      retryStrategy(times) {
        return Math.min(times * 200, 5000);
      },
    });
    this.log = log.child({ component: 'redis-store' });

    this.redis.on('error', (err) => {
      this.log.error({ err }, 'Redis connection error');
    });
    this.redis.on('connect', () => {
      this.log.info('Connected to Redis');
    });
  }

  /** Update a miner's availability state */
  async updateMiner(miner: MinerState): Promise<void> {
    const key = MINER_KEY_PREFIX + miner.address;
    await this.redis.set(key, JSON.stringify(miner), 'EX', 120); // 2 min TTL
    if (miner.online) {
      await this.redis.sadd(MINERS_SET, miner.address);
    } else {
      await this.redis.srem(MINERS_SET, miner.address);
    }
  }

  /** Get a miner's state */
  async getMiner(address: string): Promise<MinerState | null> {
    const data = await this.redis.get(MINER_KEY_PREFIX + address);
    return data ? (JSON.parse(data) as MinerState) : null;
  }

  /** Get all online miners */
  async getOnlineMiners(): Promise<MinerState[]> {
    const addresses = await this.redis.smembers(MINERS_SET);
    const pipeline = this.redis.pipeline();
    for (const addr of addresses) {
      pipeline.get(MINER_KEY_PREFIX + addr);
    }
    const results = await pipeline.exec();
    if (!results) return [];

    const miners: MinerState[] = [];
    for (const [err, data] of results) {
      if (!err && data) {
        try {
          miners.push(JSON.parse(data as string) as MinerState);
        } catch {
          // skip invalid entries
        }
      }
    }
    return miners;
  }

  /** Add a pending task */
  async addPendingTask(task: PendingTask): Promise<void> {
    const key = TASK_KEY_PREFIX + task.taskId;
    await this.redis.set(key, JSON.stringify(task), 'EX', 600); // 10 min TTL
    await this.redis.zadd(PENDING_TASKS_SET, task.deadline, task.taskId);
  }

  /** Get a pending task */
  async getPendingTask(taskId: string): Promise<PendingTask | null> {
    const data = await this.redis.get(TASK_KEY_PREFIX + taskId);
    return data ? (JSON.parse(data) as PendingTask) : null;
  }

  /** Get all pending task IDs sorted by deadline (earliest first) */
  async getPendingTaskIds(): Promise<string[]> {
    return this.redis.zrangebyscore(PENDING_TASKS_SET, '-inf', '+inf');
  }

  /** Remove a task from the pending set */
  async removePendingTask(taskId: string): Promise<void> {
    await this.redis.zrem(PENDING_TASKS_SET, taskId);
    await this.redis.del(TASK_KEY_PREFIX + taskId);
  }

  /** Record a task assignment */
  async setAssignment(assignment: TaskAssignment): Promise<void> {
    const key = ASSIGNMENT_KEY_PREFIX + assignment.taskId;
    await this.redis.set(key, JSON.stringify(assignment), 'EX', 300); // 5 min TTL
  }

  /** Get a task assignment */
  async getAssignment(taskId: string): Promise<TaskAssignment | null> {
    const data = await this.redis.get(ASSIGNMENT_KEY_PREFIX + taskId);
    return data ? (JSON.parse(data) as TaskAssignment) : null;
  }

  /** Remove a task assignment */
  async removeAssignment(taskId: string): Promise<void> {
    await this.redis.del(ASSIGNMENT_KEY_PREFIX + taskId);
  }

  /** Store a challenge task for later verification */
  async addChallenge(challenge: ChallengeTask): Promise<void> {
    const key = CHALLENGE_KEY_PREFIX + challenge.taskId;
    await this.redis.set(key, JSON.stringify(challenge), 'EX', 600);
  }

  /** Get a challenge task */
  async getChallenge(taskId: string): Promise<ChallengeTask | null> {
    const data = await this.redis.get(CHALLENGE_KEY_PREFIX + taskId);
    return data ? (JSON.parse(data) as ChallengeTask) : null;
  }

  /** Increment a miner's active task count */
  async incrementMinerTasks(address: string): Promise<void> {
    const miner = await this.getMiner(address);
    if (miner) {
      miner.activeTasks++;
      await this.updateMiner(miner);
    }
  }

  /** Decrement a miner's active task count */
  async decrementMinerTasks(address: string): Promise<void> {
    const miner = await this.getMiner(address);
    if (miner && miner.activeTasks > 0) {
      miner.activeTasks--;
      await this.updateMiner(miner);
    }
  }

  /** Close Redis connection */
  async close(): Promise<void> {
    await this.redis.quit();
  }
}
