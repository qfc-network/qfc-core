/** Task status in the routing pipeline */
export type TaskStatus = 'pending' | 'assigned' | 'executing' | 'completed' | 'failed' | 'timeout';

/** GPU tier classification */
export type GpuTier = 'Hot' | 'Warm' | 'Cold';

/** Compute task types supported by the network */
export type TaskType =
  | 'embedding'
  | 'text_generation'
  | 'image_classification'
  | 'speech_to_text'
  | 'image_generation';

/** A pending inference task from the chain */
export interface PendingTask {
  taskId: string;
  epoch: number;
  taskType: TaskType;
  modelName: string;
  modelVersion: string;
  inputDataHash: string;
  requiredTier: GpuTier;
  requiredMemoryMb: number;
  deadline: number;
  reward: string;
  createdAt: number;
  /** Whether this is a challenge task injected by the router */
  isChallenge: boolean;
}

/** A registered miner's availability state */
export interface MinerState {
  address: string;
  gpuTier: GpuTier;
  availableMemoryMb: number;
  backend: string;
  loadedModels: string[];
  activeTasks: number;
  maxTasks: number;
  lastHeartbeat: number;
  /** Reputation score (0-10000) */
  reputation: number;
  /** Whether the miner is online and accepting tasks */
  online: boolean;
}

/** Task assignment record */
export interface TaskAssignment {
  taskId: string;
  minerAddress: string;
  assignedAt: number;
  deadline: number;
  attempts: number;
}

/** Result of a task matching operation */
export interface MatchResult {
  taskId: string;
  minerAddress: string;
  score: number;
}

/** Challenge task for anti-fraud verification */
export interface ChallengeTask {
  taskId: string;
  taskType: TaskType;
  modelName: string;
  expectedOutputHash: string;
  injectedAt: number;
}
