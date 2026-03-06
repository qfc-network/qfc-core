//! Spot-check verification logic for inference proofs

use qfc_inference::proof::InferenceProof;
use qfc_inference::{InferenceEngine, InferenceTask};
use qfc_types::Hash;
use thiserror::Error;

/// Verification errors
#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("Epoch mismatch: expected {expected}, got {got}")]
    EpochMismatch { expected: u64, got: u64 },

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

/// Verify an inference proof (basic checks, no re-execution)
pub fn verify_basic(
    proof: &InferenceProof,
    expected_epoch: u64,
    approved_models: &qfc_inference::model::ModelRegistry,
) -> Result<VerificationResult, VerificationError> {
    // 1. Check epoch matches (allow ±1 tolerance for clock skew between nodes)
    let diff = if proof.epoch >= expected_epoch {
        proof.epoch - expected_epoch
    } else {
        expected_epoch - proof.epoch
    };
    if diff > 1 {
        return Err(VerificationError::EpochMismatch {
            expected: expected_epoch,
            got: proof.epoch,
        });
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

    #[test]
    fn test_verify_basic_pass() {
        let registry = ModelRegistry::default_v2();
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
            1234567890,
        );

        let result = verify_basic(&proof, 1, &registry).unwrap();
        assert!(result.passed);
        assert!(!result.spot_checked);
    }

    #[test]
    fn test_verify_basic_wrong_epoch() {
        let registry = ModelRegistry::default_v2();
        let proof = InferenceProof::new(
            Address::default(),
            5, // wrong epoch (>1 difference from expected)
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            1234567890,
        );

        // Epoch 5 vs expected 1 → diff > 1 → error
        let result = verify_basic(&proof, 1, &registry);
        assert!(matches!(
            result,
            Err(VerificationError::EpochMismatch { .. })
        ));

        // Epoch 2 vs expected 1 → diff = 1 → OK (±1 tolerance)
        let proof_close = InferenceProof::new(
            Address::default(),
            2,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            1_000_000_000,
            BackendType::Cpu,
            1234567890,
        );
        let result_close = verify_basic(&proof_close, 1, &registry);
        assert!(result_close.is_ok());
    }

    #[test]
    fn test_verify_basic_unapproved_model() {
        let registry = ModelRegistry::default_v2();
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
            1234567890,
        );

        let result = verify_basic(&proof, 1, &registry);
        assert!(matches!(result, Err(VerificationError::UnapprovedModel(_))));
    }

    #[test]
    fn test_spot_check_determinism() {
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
            1234567890,
        );

        let decision1 = should_spot_check(&proof);
        let decision2 = should_spot_check(&proof);
        assert_eq!(decision1, decision2);
    }

    #[tokio::test]
    async fn test_verify_spot_check_pass() {
        use qfc_inference::backend::cpu::CpuEngine;

        let engine = CpuEngine::new();

        // Build a task
        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            vec![],
            1234567890,
            1234597890,
        );

        // Run inference to get the correct output hash
        let result = engine.run_inference(&task).await.unwrap();

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
            1234567890,
        );

        // Spot-check should pass
        let verification = verify_spot_check(&proof, &task, &engine).await.unwrap();
        assert!(verification.passed);
        assert!(verification.spot_checked);
    }

    #[tokio::test]
    async fn test_verify_spot_check_mismatch() {
        use qfc_inference::backend::cpu::CpuEngine;

        let engine = CpuEngine::new();

        // Build a task
        let task = InferenceTask::new(
            Hash::new([0x42; 32]),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("qfc-embed-small", "v1.0"),
                input_hash: Hash::ZERO,
            },
            vec![],
            1234567890,
            1234597890,
        );

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
            1234567890,
        );

        // Spot-check should detect mismatch
        let result = verify_spot_check(&proof, &task, &engine).await;
        assert!(matches!(
            result,
            Err(VerificationError::OutputHashMismatch { .. })
        ));
    }
}
