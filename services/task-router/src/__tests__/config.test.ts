import { describe, it, expect } from 'vitest';
import { loadConfig } from '../config.js';

describe('loadConfig', () => {
  it('should return default values when env vars are not set', () => {
    const config = loadConfig();
    expect(config.rpcWsUrl).toBe('ws://127.0.0.1:8546');
    expect(config.rpcHttpUrl).toBe('http://127.0.0.1:8545');
    expect(config.redisUrl).toBe('redis://127.0.0.1:6379');
    expect(config.challengeIntervalMs).toBe(300000);
    expect(config.taskTimeoutMs).toBe(120000);
    expect(config.logLevel).toBe('info');
    expect(config.maxTasksPerMiner).toBe(3);
    expect(config.pollIntervalMs).toBe(5000);
  });
});
