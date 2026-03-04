//! Multi-threaded mining implementation

use crate::{meets_difficulty, mine_once};
use qfc_types::{Address, Hash, MiningTask, WorkProof};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Mining result from a mining session
#[derive(Clone, Debug)]
pub struct MiningResult {
    /// Best nonce found (lowest hash)
    pub best_nonce: u64,
    /// Best hash found
    pub best_hash: Hash,
    /// Number of valid hashes found
    pub work_count: u64,
    /// Total hashes computed
    pub total_hashes: u64,
    /// Mining duration
    pub duration: Duration,
}

impl MiningResult {
    /// Calculate hashrate (hashes per second)
    pub fn hashrate(&self) -> f64 {
        let secs = self.duration.as_secs_f64();
        if secs > 0.0 {
            self.total_hashes as f64 / secs
        } else {
            0.0
        }
    }
}

/// Multi-threaded miner
pub struct Miner {
    /// Validator address
    validator: Address,
    /// Number of mining threads
    threads: usize,
}

impl Miner {
    /// Create a new miner
    pub fn new(validator: Address, threads: usize) -> Self {
        Self {
            validator,
            threads: threads.max(1),
        }
    }

    /// Mine for a specific duration
    ///
    /// This is useful for epoch-based mining where you mine until the epoch ends
    pub fn mine_for_duration(&self, task: &MiningTask, duration: Duration) -> MiningResult {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let best_hash = Arc::new(parking_lot::RwLock::new(Hash::new([0xff; 32])));
        let best_nonce = Arc::new(AtomicU64::new(0));
        let work_count = Arc::new(AtomicU64::new(0));
        let total_hashes = Arc::new(AtomicU64::new(0));

        let start = Instant::now();
        let num_threads = self.threads;

        // Spawn mining threads
        let handles: Vec<_> = (0..self.threads)
            .map(|thread_id| {
                let task = task.clone();
                let validator = self.validator;
                let stop_flag = Arc::clone(&stop_flag);
                let best_hash = Arc::clone(&best_hash);
                let best_nonce = Arc::clone(&best_nonce);
                let work_count = Arc::clone(&work_count);
                let total_hashes = Arc::clone(&total_hashes);

                thread::spawn(move || {
                    let mut nonce = thread_id as u64;
                    let mut local_work_count = 0u64;
                    let mut local_total = 0u64;

                    while !stop_flag.load(Ordering::Relaxed) {
                        let hash = mine_once(&task.seed, &validator, nonce);
                        local_total += 1;

                        if meets_difficulty(&hash, &task.difficulty) {
                            local_work_count += 1;

                            // Check if this is the best hash
                            let current_best = *best_hash.read();
                            if hash < current_best {
                                *best_hash.write() = hash;
                                best_nonce.store(nonce, Ordering::Relaxed);
                            }
                        }

                        // Use thread_id to ensure threads don't overlap
                        nonce = nonce.wrapping_add(num_threads as u64);
                    }

                    // Add local counts to global
                    work_count.fetch_add(local_work_count, Ordering::Relaxed);
                    total_hashes.fetch_add(local_total, Ordering::Relaxed);
                })
            })
            .collect();

        // Wait for duration
        thread::sleep(duration);
        stop_flag.store(true, Ordering::Relaxed);

        // Wait for all threads
        for handle in handles {
            let _ = handle.join();
        }

        let elapsed = start.elapsed();
        let final_hash = *best_hash.read();

        MiningResult {
            best_nonce: best_nonce.load(Ordering::Relaxed),
            best_hash: final_hash,
            work_count: work_count.load(Ordering::Relaxed),
            total_hashes: total_hashes.load(Ordering::Relaxed),
            duration: elapsed,
        }
    }

    /// Mine until a certain number of valid proofs are found
    pub fn mine_until_count(&self, task: &MiningTask, target_count: u64) -> MiningResult {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let best_hash = Arc::new(parking_lot::RwLock::new(Hash::new([0xff; 32])));
        let best_nonce = Arc::new(AtomicU64::new(0));
        let work_count = Arc::new(AtomicU64::new(0));
        let total_hashes = Arc::new(AtomicU64::new(0));

        let start = Instant::now();
        let num_threads = self.threads;

        // Spawn mining threads
        let handles: Vec<_> = (0..self.threads)
            .map(|thread_id| {
                let task = task.clone();
                let validator = self.validator;
                let stop_flag = Arc::clone(&stop_flag);
                let best_hash = Arc::clone(&best_hash);
                let best_nonce = Arc::clone(&best_nonce);
                let work_count = Arc::clone(&work_count);
                let total_hashes = Arc::clone(&total_hashes);

                thread::spawn(move || {
                    let mut nonce = thread_id as u64;
                    let mut local_total = 0u64;

                    while !stop_flag.load(Ordering::Relaxed) {
                        let hash = mine_once(&task.seed, &validator, nonce);
                        local_total += 1;

                        if meets_difficulty(&hash, &task.difficulty) {
                            let current_count = work_count.fetch_add(1, Ordering::Relaxed);

                            // Check if this is the best hash
                            let current_best = *best_hash.read();
                            if hash < current_best {
                                *best_hash.write() = hash;
                                best_nonce.store(nonce, Ordering::Relaxed);
                            }

                            // Check if we've reached target
                            if current_count + 1 >= target_count {
                                stop_flag.store(true, Ordering::Relaxed);
                                break;
                            }
                        }

                        nonce = nonce.wrapping_add(num_threads as u64);
                    }

                    total_hashes.fetch_add(local_total, Ordering::Relaxed);
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            let _ = handle.join();
        }

        let elapsed = start.elapsed();
        let final_hash = *best_hash.read();

        MiningResult {
            best_nonce: best_nonce.load(Ordering::Relaxed),
            best_hash: final_hash,
            work_count: work_count.load(Ordering::Relaxed),
            total_hashes: total_hashes.load(Ordering::Relaxed),
            duration: elapsed,
        }
    }

    /// Create a work proof from mining result
    pub fn create_proof(&self, task: &MiningTask, result: &MiningResult) -> WorkProof {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        WorkProof::new(
            self.validator,
            task.epoch,
            result.best_nonce,
            result.best_hash,
            result.work_count,
            now,
        )
    }
}

/// Simple single-threaded mining for testing
pub fn mine_simple(
    task: &MiningTask,
    validator: &Address,
    max_attempts: u64,
) -> Option<(u64, Hash)> {
    for nonce in 0..max_attempts {
        let hash = mine_once(&task.seed, validator, nonce);
        if meets_difficulty(&hash, &task.difficulty) {
            return Some((nonce, hash));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_types::U256;

    fn easy_task() -> MiningTask {
        MiningTask::new(
            1,
            [0u8; 32],
            // Very easy difficulty - almost any hash will pass
            U256::from_be_bytes(&[
                0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff,
            ]),
            0,
            10000,
        )
    }

    #[test]
    fn test_mine_simple() {
        let task = easy_task();
        let validator = Address::default();

        let result = mine_simple(&task, &validator, 100);
        assert!(result.is_some());

        let (nonce, hash) = result.unwrap();
        assert!(meets_difficulty(&hash, &task.difficulty));

        // Verify the hash
        let computed = mine_once(&task.seed, &validator, nonce);
        assert_eq!(computed, hash);
    }

    #[test]
    fn test_miner_duration() {
        let task = easy_task();
        let validator = Address::default();
        let miner = Miner::new(validator, 2);

        let result = miner.mine_for_duration(&task, Duration::from_millis(100));

        assert!(result.work_count > 0);
        assert!(result.total_hashes > 0);
        assert!(result.hashrate() > 0.0);
    }

    #[test]
    fn test_miner_until_count() {
        let task = easy_task();
        let validator = Address::default();
        let miner = Miner::new(validator, 2);

        let result = miner.mine_until_count(&task, 10);

        assert!(result.work_count >= 10);
    }

    #[test]
    fn test_create_proof() {
        let task = easy_task();
        let validator = Address::default();
        let miner = Miner::new(validator, 1);

        let result = miner.mine_until_count(&task, 5);
        let proof = miner.create_proof(&task, &result);

        assert_eq!(proof.validator, validator);
        assert_eq!(proof.epoch, task.epoch);
        assert_eq!(proof.work_count, result.work_count);
    }

    #[test]
    fn test_mining_result_hashrate() {
        let result = MiningResult {
            best_nonce: 0,
            best_hash: Hash::default(),
            work_count: 100,
            total_hashes: 1_000_000,
            duration: Duration::from_secs(1),
        };

        assert_eq!(result.hashrate(), 1_000_000.0);
    }
}
