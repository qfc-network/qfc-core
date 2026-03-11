import { describe, it, expect } from 'vitest';
import { TaskMatcher } from '../matcher.js';
import type { PendingTask, MinerState } from '../types.js';
import pino from 'pino';

const log = pino({ level: 'silent' });

function makeMiner(overrides: Partial<MinerState> = {}): MinerState {
  return {
    address: '0x' + 'a'.repeat(40),
    gpuTier: 'Warm',
    availableMemoryMb: 16000,
    backend: 'Metal',
    loadedModels: [],
    activeTasks: 0,
    maxTasks: 3,
    lastHeartbeat: Date.now(),
    reputation: 5000,
    online: true,
    ...overrides,
  };
}

function makeTask(overrides: Partial<PendingTask> = {}): PendingTask {
  return {
    taskId: 'task-001',
    epoch: 1,
    taskType: 'text_generation',
    modelName: 'llama3-8b',
    modelVersion: 'v1',
    inputDataHash: '0x1234',
    requiredTier: 'Warm',
    requiredMemoryMb: 8000,
    deadline: Date.now() + 120000,
    reward: '1000',
    createdAt: Date.now(),
    isChallenge: false,
    ...overrides,
  };
}

describe('TaskMatcher', () => {
  it('should match a task to the best miner', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask();
    const miners = [
      makeMiner({ address: '0x' + '1'.repeat(40), reputation: 3000 }),
      makeMiner({ address: '0x' + '2'.repeat(40), reputation: 8000 }),
      makeMiner({ address: '0x' + '3'.repeat(40), reputation: 5000 }),
    ];

    const result = matcher.match(task, miners);
    expect(result).not.toBeNull();
    // Highest reputation miner should win
    expect(result!.minerAddress).toBe('0x' + '2'.repeat(40));
  });

  it('should prefer miner with model already loaded', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask({ modelName: 'llama3-8b' });
    const miners = [
      makeMiner({
        address: '0x' + '1'.repeat(40),
        reputation: 3000,
        loadedModels: ['llama3-8b'],
      }),
      makeMiner({ address: '0x' + '2'.repeat(40), reputation: 8000, loadedModels: [] }),
    ];

    const result = matcher.match(task, miners);
    expect(result).not.toBeNull();
    // Model-loaded bonus (+50) should outweigh reputation difference
    expect(result!.minerAddress).toBe('0x' + '1'.repeat(40));
  });

  it('should reject miners with insufficient tier', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask({ requiredTier: 'Hot' });
    const miners = [
      makeMiner({ address: '0x' + '1'.repeat(40), gpuTier: 'Warm' }),
      makeMiner({ address: '0x' + '2'.repeat(40), gpuTier: 'Cold' }),
    ];

    const result = matcher.match(task, miners);
    expect(result).toBeNull();
  });

  it('should reject miners with insufficient memory', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask({ requiredMemoryMb: 24000 });
    const miners = [makeMiner({ availableMemoryMb: 16000 })];

    const result = matcher.match(task, miners);
    expect(result).toBeNull();
  });

  it('should reject offline miners', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask();
    const miners = [makeMiner({ online: false })];

    const result = matcher.match(task, miners);
    expect(result).toBeNull();
  });

  it('should reject miners at max task capacity', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask();
    const miners = [makeMiner({ activeTasks: 3 })];

    const result = matcher.match(task, miners);
    expect(result).toBeNull();
  });

  it('should return null when no miners available', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask();

    const result = matcher.match(task, []);
    expect(result).toBeNull();
  });

  it('should prefer lower-load miners when all else is equal', () => {
    const matcher = new TaskMatcher(3, log);
    const task = makeTask({ requiredTier: 'Cold' });
    const miners = [
      makeMiner({
        address: '0x' + '1'.repeat(40),
        gpuTier: 'Cold',
        reputation: 5000,
        activeTasks: 2,
      }),
      makeMiner({
        address: '0x' + '2'.repeat(40),
        gpuTier: 'Cold',
        reputation: 5000,
        activeTasks: 0,
      }),
    ];

    const result = matcher.match(task, miners);
    expect(result).not.toBeNull();
    expect(result!.minerAddress).toBe('0x' + '2'.repeat(40));
  });
});
