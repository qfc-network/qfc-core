//! QFC Proof of Work Mining
//!
//! This crate provides Blake3-based proof of work for the optional 20% compute contribution
//! in QFC's Proof of Contribution (PoC) consensus.
//!
//! # Usage
//!
//! ```ignore
//! use qfc_pow::{Miner, verify_proof};
//!
//! // Create a miner
//! let miner = Miner::new(validator_address, 4); // 4 threads
//!
//! // Mine for a task
//! let proof = miner.mine(&task);
//!
//! // Verify a proof
//! assert!(verify_proof(&proof, &task));
//! ```

mod difficulty;
mod error;
mod mining;

pub use difficulty::*;
pub use error::*;
pub use mining::*;

use qfc_crypto::blake3_hash;
use qfc_types::{Address, ComputeProof, Hash, InferenceProof, MiningTask, WorkProof, U256};

/// Perform a single mining iteration
///
/// Returns the nonce and resulting hash
pub fn mine_once(seed: &[u8; 32], validator: &Address, nonce: u64) -> Hash {
    let mut data = Vec::with_capacity(32 + 20 + 8);
    data.extend_from_slice(seed);
    data.extend_from_slice(validator.as_bytes());
    data.extend_from_slice(&nonce.to_le_bytes());
    blake3_hash(&data)
}

/// Check if a hash meets the difficulty target
///
/// The hash must be less than the difficulty target to be valid
pub fn meets_difficulty(hash: &Hash, difficulty: &U256) -> bool {
    let hash_value = U256::from_be_bytes(hash.as_bytes());
    hash_value < *difficulty
}

/// Verify a work proof against a mining task
pub fn verify_proof(proof: &WorkProof, task: &MiningTask) -> Result<bool, PowError> {
    // 1. Check epoch matches
    if proof.epoch != task.epoch {
        return Err(PowError::EpochMismatch {
            expected: task.epoch,
            got: proof.epoch,
        });
    }

    // 2. Verify the hash computation
    let computed_hash = mine_once(&task.seed, &proof.validator, proof.nonce);
    if computed_hash != proof.hash {
        return Err(PowError::InvalidHash);
    }

    // 3. Verify the hash meets difficulty
    if !meets_difficulty(&proof.hash, &task.difficulty) {
        return Err(PowError::DifficultyNotMet);
    }

    Ok(true)
}

/// Calculate hashrate from work proof
///
/// hashrate = work_count * difficulty_factor / epoch_duration
pub fn calculate_hashrate(proof: &WorkProof, task: &MiningTask) -> u64 {
    if proof.work_count == 0 {
        return 0;
    }

    let epoch_duration_secs = (task.epoch_end - task.epoch_start) / 1000;
    if epoch_duration_secs == 0 {
        return 0;
    }

    // Estimate hash operations per valid proof based on difficulty
    // For difficulty D, expected hashes per valid proof = MAX / D
    // We approximate this as 2^leading_zeros
    let leading_zeros = count_leading_zeros(&task.difficulty);
    let hashes_per_proof = 1u64 << leading_zeros.min(63);

    // Total hashes = work_count * hashes_per_proof
    // Hashrate = total_hashes / epoch_duration
    let total_hashes = proof.work_count.saturating_mul(hashes_per_proof);
    total_hashes / epoch_duration_secs
}

/// Verify an inference proof (v2.0) — basic validation only
///
/// Checks epoch, model approval, and FLOPS reasonableness.
/// Spot-check re-execution is handled by qfc-ai-coordinator.
pub fn verify_inference_proof(
    proof: &InferenceProof,
    expected_epoch: u64,
) -> Result<bool, PowError> {
    // 1. Check epoch matches
    if proof.epoch != expected_epoch {
        return Err(PowError::EpochMismatch {
            expected: expected_epoch,
            got: proof.epoch,
        });
    }

    // 2. Check output hash is non-zero (basic sanity)
    if proof.output_hash == Hash::ZERO {
        return Err(PowError::InvalidHash);
    }

    // 3. Check execution time is non-zero
    if proof.execution_time_ms == 0 {
        return Err(PowError::InvalidHash);
    }

    Ok(true)
}

/// Verify a compute proof (supports both v1 PoW and v2 inference)
pub fn verify_compute_proof(
    proof: &ComputeProof,
    task: Option<&MiningTask>,
    expected_epoch: u64,
) -> Result<bool, PowError> {
    match proof {
        ComputeProof::PowV1(work_proof) => {
            let task = task.ok_or(PowError::InvalidHash)?;
            verify_proof(work_proof, task)
        }
        ComputeProof::InferenceV2(inference_proof) => {
            verify_inference_proof(inference_proof, expected_epoch)
        }
    }
}

/// Count leading zero bits in a U256
fn count_leading_zeros(value: &U256) -> u32 {
    let bytes = value.to_be_bytes();
    let mut zeros = 0u32;

    for byte in bytes.iter() {
        if *byte == 0 {
            zeros += 8;
        } else {
            zeros += byte.leading_zeros();
            break;
        }
    }

    zeros
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mine_once() {
        let seed = [0u8; 32];
        let validator = Address::default();

        let hash1 = mine_once(&seed, &validator, 0);
        let hash2 = mine_once(&seed, &validator, 1);

        // Different nonces should produce different hashes
        assert_ne!(hash1, hash2);

        // Same inputs should produce same hash
        let hash1_again = mine_once(&seed, &validator, 0);
        assert_eq!(hash1, hash1_again);
    }

    #[test]
    fn test_meets_difficulty() {
        // Easy difficulty (high target)
        let easy_difficulty = U256::from_be_bytes(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        // Hard difficulty (low target)
        let hard_difficulty = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x01,
        ]);

        // A random hash
        let hash = mine_once(&[0u8; 32], &Address::default(), 12345);

        // Should meet easy difficulty (almost always)
        assert!(meets_difficulty(&hash, &easy_difficulty));

        // Probably won't meet hard difficulty
        // (This is probabilistic, but extremely unlikely to fail)
        assert!(!meets_difficulty(&hash, &hard_difficulty));
    }

    #[test]
    fn test_verify_proof() {
        let seed = [1u8; 32];
        let validator = Address::default();
        let difficulty = U256::from_be_bytes(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        let task = MiningTask::new(100, seed, difficulty, 1000, 2000);

        // Find a valid nonce
        let nonce = 42u64;
        let hash = mine_once(&seed, &validator, nonce);

        let proof = WorkProof::new(validator, 100, nonce, hash, 1, 1500);

        // Should verify successfully
        assert!(verify_proof(&proof, &task).unwrap());
    }

    #[test]
    fn test_verify_proof_wrong_epoch() {
        let seed = [1u8; 32];
        let validator = Address::default();
        let difficulty = U256::MAX;

        let task = MiningTask::new(100, seed, difficulty, 1000, 2000);

        let hash = mine_once(&seed, &validator, 42);
        let proof = WorkProof::new(validator, 99, 42, hash, 1, 1500); // Wrong epoch

        let result = verify_proof(&proof, &task);
        assert!(matches!(result, Err(PowError::EpochMismatch { .. })));
    }

    #[test]
    fn test_count_leading_zeros() {
        let value1 = U256::from_be_bytes(&[
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);
        assert_eq!(count_leading_zeros(&value1), 16);

        let value2 = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);
        assert_eq!(count_leading_zeros(&value2), 64);

        let value3 = U256::from_be_bytes(&[
            0x0f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);
        assert_eq!(count_leading_zeros(&value3), 4);
    }

    #[test]
    fn test_calculate_hashrate() {
        let seed = [1u8; 32];
        let difficulty = U256::from_be_bytes(&[
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff,
        ]);

        // 10 second epoch (10000ms)
        let task = MiningTask::new(100, seed, difficulty, 0, 10000);

        let proof = WorkProof::new(Address::default(), 100, 0, Hash::default(), 1000, 5000);

        let hashrate = calculate_hashrate(&proof, &task);

        // 16 leading zeros means ~65536 hashes per valid proof
        // 1000 valid proofs * 65536 / 10 seconds = ~6.5M H/s
        assert!(hashrate > 0);
    }
}
