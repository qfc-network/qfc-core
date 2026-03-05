//! Inference result and deterministic output hashing

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::{Address, Hash, Signature};
use serde::{Deserialize, Serialize};

use crate::runtime::BackendType;
use crate::task::ComputeTaskType;

/// Result of an inference execution
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceResult {
    /// Raw output bytes (tensor data)
    pub output_data: Vec<u8>,
    /// Hash of the output: blake3(output_data)
    pub output_hash: Hash,
    /// Execution time in milliseconds
    pub execution_time_ms: u64,
    /// Estimated FLOPS for this computation
    pub flops_estimated: u64,
}

impl InferenceResult {
    /// Create a new inference result, computing the output hash
    pub fn new(output_data: Vec<u8>, execution_time_ms: u64, flops_estimated: u64) -> Self {
        let output_hash = hash_output(&output_data);
        Self {
            output_data,
            output_hash,
            execution_time_ms,
            flops_estimated,
        }
    }
}

/// Inference proof submitted to the network for consensus
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct InferenceProof {
    /// Validator/miner address
    pub validator: Address,
    /// Epoch number
    pub epoch: u64,
    /// The task type executed
    pub task_type: ComputeTaskType,
    /// Hash of input data
    pub input_hash: Hash,
    /// Hash of inference output: blake3(output_tensor_bytes)
    pub output_hash: Hash,
    /// Execution time in milliseconds
    pub execution_time_ms: u64,
    /// Estimated FLOPS of computation
    pub flops_estimated: u64,
    /// Backend used (CUDA / Metal / CPU)
    pub backend: BackendType,
    /// Timestamp
    pub timestamp: u64,
    /// Signature over the proof
    pub signature: Signature,
}

impl InferenceProof {
    /// Create a new inference proof (unsigned)
    pub fn new(
        validator: Address,
        epoch: u64,
        task_type: ComputeTaskType,
        input_hash: Hash,
        output_hash: Hash,
        execution_time_ms: u64,
        flops_estimated: u64,
        backend: BackendType,
        timestamp: u64,
    ) -> Self {
        Self {
            validator,
            epoch,
            task_type,
            input_hash,
            output_hash,
            execution_time_ms,
            flops_estimated,
            backend,
            timestamp,
            signature: Signature::default(),
        }
    }

    /// Set the signature
    pub fn set_signature(&mut self, signature: Signature) {
        self.signature = signature;
    }

    /// Get bytes for signing (excludes signature field)
    pub fn to_bytes_without_signature(&self) -> Vec<u8> {
        let unsigned = UnsignedInferenceProof {
            validator: self.validator,
            epoch: self.epoch,
            task_type: self.task_type.clone(),
            input_hash: self.input_hash,
            output_hash: self.output_hash,
            execution_time_ms: self.execution_time_ms,
            flops_estimated: self.flops_estimated,
            backend: self.backend,
            timestamp: self.timestamp,
        };
        borsh::to_vec(&unsigned).expect("InferenceProof serialization should not fail")
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("InferenceProof serialization should not fail")
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
struct UnsignedInferenceProof {
    pub validator: Address,
    pub epoch: u64,
    pub task_type: ComputeTaskType,
    pub input_hash: Hash,
    pub output_hash: Hash,
    pub execution_time_ms: u64,
    pub flops_estimated: u64,
    pub backend: BackendType,
    pub timestamp: u64,
}

/// Versioned compute proof (supports both v1 PoW and v2 inference)
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
#[allow(dead_code)]
pub enum ComputeProof {
    /// v1 legacy (Blake3 PoW)
    PowV1(qfc_types::WorkProof),
    /// v2 AI inference
    InferenceV2(InferenceProof),
}

/// Hash output data deterministically using Blake3
pub fn hash_output(data: &[u8]) -> Hash {
    qfc_crypto::blake3_hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::ModelId;

    #[test]
    fn test_inference_result_hashing() {
        let data = vec![1u8, 2, 3, 4, 5];
        let result = InferenceResult::new(data.clone(), 100, 1000);

        // Same data should produce same hash
        let result2 = InferenceResult::new(data, 200, 2000);
        assert_eq!(result.output_hash, result2.output_hash);

        // Different data should produce different hash
        let result3 = InferenceResult::new(vec![5, 4, 3, 2, 1], 100, 1000);
        assert_ne!(result.output_hash, result3.output_hash);
    }

    #[test]
    fn test_inference_proof_serialization() {
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("bert-base", "v1"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            5000,
            BackendType::Cpu,
            1234567890,
        );

        let bytes = proof.to_bytes();
        let decoded = InferenceProof::from_bytes(&bytes).unwrap();
        assert_eq!(proof, decoded);
    }

    #[test]
    fn test_inference_proof_bytes_without_signature() {
        let proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("bert-base", "v1"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            5000,
            BackendType::Cpu,
            1234567890,
        );

        let bytes = proof.to_bytes_without_signature();
        // Should not include signature (64 bytes)
        assert!(bytes.len() < proof.to_bytes().len());
    }

    #[test]
    fn test_compute_proof_enum() {
        let pow_proof = qfc_types::WorkProof::new(
            Address::default(),
            1,
            42,
            Hash::ZERO,
            100,
            1234567890,
        );
        let compute = ComputeProof::PowV1(pow_proof);
        assert!(matches!(compute, ComputeProof::PowV1(_)));

        let inf_proof = InferenceProof::new(
            Address::default(),
            1,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("bert-base", "v1"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::new([0xab; 32]),
            150,
            5000,
            BackendType::Cpu,
            1234567890,
        );
        let compute = ComputeProof::InferenceV2(inf_proof);
        assert!(matches!(compute, ComputeProof::InferenceV2(_)));
    }
}
