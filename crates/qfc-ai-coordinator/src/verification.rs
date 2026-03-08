//! Spot-check verification logic for inference proofs
//!
//! Follows the industry pattern (Bittensor, Ritual, Gensyn) of using
//! task-scoped deadlines instead of global epoch checks. Epochs are
//! retained for reward attribution only — not for rejecting proofs.

use qfc_inference::proof::InferenceProof;
use qfc_inference::{InferenceEngine, InferenceTask};
use qfc_types::Hash;
use thiserror::Error;

/// Maximum age of a proof in seconds (2 minutes).
/// Proofs older than this are rejected to prevent replay attacks.
/// This is deliberately generous — inference can take a while on slow hardware.
const MAX_PROOF_AGE_SECS: u64 = 120;

/// Maximum clock skew tolerance in seconds (10 seconds into the future)
const MAX_FUTURE_SECS: u64 = 10;

/// Verification errors
#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("Proof expired: submitted at {submitted}, but current time is {now} ({age_secs}s old, max {max_age_secs}s)")]
    ProofExpired {
        submitted: u64,
        now: u64,
        age_secs: u64,
        max_age_secs: u64,
    },

    #[error("Proof timestamp is in the future: {submitted} > {now} + {tolerance}s")]
    ProofFromFuture {
        submitted: u64,
        now: u64,
        tolerance: u64,
    },

    #[error("Model not in approved registry: {0}")]
    UnapprovedModel(String),

    #[error("Output hash mismatch: expected {expected}, got {got}")]
    OutputHashMismatch { expected: Hash, got: Hash },

    #[error("Unreasonable FLOPS claim: claimed {claimed}, expected ~{expected}")]
    UnreasonableFlops { claimed: u64, expected: u64 },

    #[error("Inference re-execution failed: {0}")]
    ReexecutionFailed(String),
}

/// Verification result
#[derive(Clone, Debug)]
pub struct VerificationResult {
    /// Whether the proof passed verification
    pub passed: bool,
    /// Was this a spot-check (re-execution) or just basic validation?
    pub spot_checked: bool,
    /// Details
    pub details: String,
}

/// Spot-check percentage (5% of proofs are re-executed)
const SPOT_CHECK_RATE: f64 = 0.05;

/// Verify an inference proof (basic checks, no re-execution).
///
/// Uses timestamp-based validation instead of epoch matching:
/// - Proof must not be older than MAX_PROOF_AGE_SECS (prevents replay)
/// - Proof must not be from the future (prevents clock manipulation)
/// - Epoch is NOT checked here — it's only used for reward attribution
pub fn verify_basic(
    proof: &InferenceProof,
    now_secs: u64,
    approved_models: &qfc_inference::model::ModelRegistry,
) -> Result<VerificationResult, VerificationError> {
    // 1. Check proof timestamp freshness (replaces epoch check)
    if proof.timestamp > now_secs + MAX_FUTURE_SECS {
        return Err(VerificationError::ProofFromFuture {
            submitted: proof.timestamp,
            now: now_secs,
            tolerance: MAX_FUTURE_SECS,
        });
    }
    if now_secs > proof.timestamp {
        let age = now_secs - proof.timestamp;
        if age > MAX_PROOF_AGE_SECS {
            return Err(VerificationError::ProofExpired {
                submitted: proof.timestamp,
                now: now_secs,
                age_secs: age,
                max_age_secs: MAX_PROOF_AGE_SECS,
            });
        }
    }

    // 2. Verify model is approved
    if let Some(model_id) = proof.task_type.model_id() {
        if !approved_models.is_approved(model_id) {
            return Err(VerificationError::UnapprovedModel(model_id.to_string()));
        }
    }

    // 3. Check FLOPS are reasonable (within 10x of expected)
    let expected_flops = crate::task_types::task_requirements(&proof.task_type).estimated_flops;
    if expected_flops > 0 {
        let ratio = proof.flops_estimated as f64 / expected_flops as f64;
        if ratio > 10.0 || ratio < 0.01 {
            return Err(VerificationError::UnreasonableFlops {
                claimed: proof.flops_estimated,
                expected: expected_flops,
            });
        }
    }

    Ok(VerificationResult {
        passed: true,
        spot_checked: false,
        details: "Basic validation passed".to_string(),
    })
}

/// Determine if a proof should be spot-checked (probabilistic)
pub fn should_spot_check(proof: &InferenceProof) -> bool {
    // Use proof hash as deterministic randomness source
    let hash_byte = proof.output_hash.as_bytes()[0];
    let threshold = (SPOT_CHECK_RATE * 256.0) as u8;
    hash_byte < threshold
}

/// Perform a full spot-check by re-executing the inference task
pub async fn verify_spot_check(
    proof: &InferenceProof,
    task: &InferenceTask,
    engine: &dyn InferenceEngine,
) -> Result<VerificationResult, VerificationError> {
    // Re-run inference with same inputs
    let result = engine
        .run_inference(task)
        .await
        .map_err(|e| VerificationError::ReexecutionFailed(e.to_string()))?;

    // Compare output hash
    if result.output_hash != proof.output_hash {
        return Err(VerificationError::OutputHashMismatch {
            expected: proof.output_hash,
            got: result.output_hash,
        });
    }

    Ok(VerificationResult {
        passed: true,
        spot_checked: true,
        details: "Spot-check re-execution matched".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_inference::model::ModelRegistry;
    use qfc_inference::{BackendType, ComputeTaskType, InferenceTask, ModelId};
    use qfc_types::Address;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn test_verify_basic_pass() {
        let registry = ModelRegistry::default_v2();
        let now = now_secs();
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000, // 1 GFLOPS — reasonable for embedding
            BackendType::Cpu,
            now,
        );

        let result = verify_basic(&proof, now, &registry).unwrap();
        assert!(result.passed);
        assert!(!result.spot_checked);
    }

    #[test]
    fn test_verify_basic_expired_proof() {
        let registry = ModelRegistry::default_v2();
        let now = now_secs();
        let old_timestamp = now - 200; // 200 seconds ago, > MAX_PROOF_AGE_SECS (120)

        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            old_timestamp,
        );

        let result = verify_basic(&proof, now, &registry);
        assert!(matches!(
            result,
            Err(VerificationError::ProofExpired { .. })
        ));

        // 60 seconds ago — within 120s window — should pass
        let recent_proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            now - 60,
        );
        let result_recent = verify_basic(&recent_proof, now, &registry);
        assert!(result_recent.is_ok());
    }

    #[test]
    fn test_verify_basic_future_proof() {
        let registry = ModelRegistry::default_v2();
        let now = now_secs();
        let future_timestamp = now + 30; // 30 seconds in the future, > MAX_FUTURE_SECS (10)

        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            future_timestamp,
        );

        let result = verify_basic(&proof, now, &registry);
        assert!(matches!(
            result,
            Err(VerificationError::ProofFromFuture { .. })
        ));
    }

    #[test]
    fn test_verify_basic_unapproved_model() {
        let registry = ModelRegistry::default_v2();
        let now = now_secs();
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("malicious-model", "v666"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            now,
        );

        let result = verify_basic(&proof, now, &registry);
        assert!(matches!(result, Err(VerificationError::UnapprovedModel(_))));
    }

    #[test]
    fn test_spot_check_determinism() {
        let now = now_secs();
        // The same proof should always get the same spot-check decision
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0x05; 32]), // 0x05 < 0.05*256=12.8 → should be spot-checked
            150,
            1_000_000_000,
            BackendType::Cpu,
            now,
        );

        let decision1 = should_spot_check(&proof);
        let decision2 = should_spot_check(&proof);
        assert_eq!(decision1, decision2);
    }

    #[tokio::test]
    async fn test_verify_spot_check_pass() {
        use qfc_inference::backend::cpu::CpuEngine;

        let mut engine = CpuEngine::new();

        // Load model first so inference doesn't reject with ModelNotLoaded
        let model_id = ModelId::new("qfc-embed-small", "v1.0");
        engine.load_model(&model_id).await.unwrap();

        // Build a task
        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: model_id.clone(),
                input_hash: Hash::ZERO,
            },
            vec![],
            1234567890,
            1234597890,
        );

        // Run inference to get the correct output hash
        let result = engine.run_inference(&task).await.unwrap();
        let now = now_secs();

        // Build a proof with the correct output hash
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::new([0x42; 32]),
            result.output_hash,
            150,
            1_000_000_000,
            BackendType::Cpu,
            now,
        );

        // Spot-check should pass
        let verification = verify_spot_check(&proof, &task, &engine).await.unwrap();
        assert!(verification.passed);
        assert!(verification.spot_checked);
    }

    #[tokio::test]
    async fn test_verify_spot_check_mismatch() {
        use qfc_inference::backend::cpu::CpuEngine;

        let mut engine = CpuEngine::new();

        // Load model first
        let model_id = ModelId::new("qfc-embed-small", "v1.0");
        engine.load_model(&model_id).await.unwrap();

        // Build a task
        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: model_id.clone(),
                input_hash: Hash::ZERO,
            },
            vec![],
            1234567890,
            1234597890,
        );

        let now = now_secs();

        // Build a proof with a TAMPERED output hash
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::new([0x42; 32]),
            Hash::new([0xff; 32]), // fraudulent output hash
            150,
            1_000_000_000,
            BackendType::Cpu,
            now,
        );

        // Spot-check should detect mismatch
        let result = verify_spot_check(&proof, &task, &engine).await;
        assert!(matches!(
            result,
            Err(VerificationError::OutputHashMismatch { .. })
        ));
    }
}
