//! Proof of Work types for compute contribution
//!
//! These types support the optional 20% compute contribution in PoC consensus.

use crate::{Address, Hash, Signature, U256};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Work proof submitted by a miner for an epoch
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct WorkProof {
    /// Validator/miner address
    pub validator: Address,
    /// Epoch number this proof is for
    pub epoch: u64,
    /// Best nonce found (corresponding to lowest hash)
    pub nonce: u64,
    /// Best hash found (must be < difficulty)
    pub hash: Hash,
    /// Number of valid hashes found this epoch
    pub work_count: u64,
    /// Timestamp when proof was created
    pub timestamp: u64,
    /// Signature over the proof
    pub signature: Signature,
}

impl WorkProof {
    /// Create a new work proof (unsigned)
    pub fn new(
        validator: Address,
        epoch: u64,
        nonce: u64,
        hash: Hash,
        work_count: u64,
        timestamp: u64,
    ) -> Self {
        Self {
            validator,
            epoch,
            nonce,
            hash,
            work_count,
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
        let mut bytes = Vec::new();
        bytes.extend_from_slice(self.validator.as_bytes());
        bytes.extend_from_slice(&self.epoch.to_le_bytes());
        bytes.extend_from_slice(&self.nonce.to_le_bytes());
        bytes.extend_from_slice(self.hash.as_bytes());
        bytes.extend_from_slice(&self.work_count.to_le_bytes());
        bytes.extend_from_slice(&self.timestamp.to_le_bytes());
        bytes
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("WorkProof serialization should not fail")
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

/// Mining task issued by the network for an epoch
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct MiningTask {
    /// Epoch number
    pub epoch: u64,
    /// Mining seed (derived from epoch + prev block hash)
    pub seed: [u8; 32],
    /// Current difficulty target (hash must be less than this)
    pub difficulty: U256,
    /// Start timestamp of the epoch
    pub epoch_start: u64,
    /// End timestamp of the epoch
    pub epoch_end: u64,
}

impl MiningTask {
    /// Create a new mining task
    pub fn new(
        epoch: u64,
        seed: [u8; 32],
        difficulty: U256,
        epoch_start: u64,
        epoch_end: u64,
    ) -> Self {
        Self {
            epoch,
            seed,
            difficulty,
            epoch_start,
            epoch_end,
        }
    }

    /// Check if the task is still active
    pub fn is_active(&self, current_time: u64) -> bool {
        current_time >= self.epoch_start && current_time < self.epoch_end
    }

    /// Serialize to bytes
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("MiningTask serialization should not fail")
    }

    /// Deserialize from bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

/// Difficulty adjustment parameters
#[derive(Clone, Debug)]
pub struct DifficultyConfig {
    /// Target number of proofs per epoch across all miners
    pub target_proofs_per_epoch: u64,
    /// Minimum difficulty (prevents difficulty from going too low)
    pub min_difficulty: U256,
    /// Maximum difficulty (prevents difficulty from going too high)
    pub max_difficulty: U256,
    /// Maximum adjustment per epoch (percentage, e.g., 10 = 10%)
    pub max_adjustment_percent: u64,
}

impl Default for DifficultyConfig {
    fn default() -> Self {
        Self {
            target_proofs_per_epoch: 10000,
            // Min difficulty: requires ~16 bits of leading zeros
            min_difficulty: U256::from_be_bytes(&[
                0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff,
            ]),
            // Max difficulty: requires ~64 bits of leading zeros
            max_difficulty: U256::from_be_bytes(&[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff,
            ]),
            max_adjustment_percent: 10,
        }
    }
}

/// Mining statistics for a validator
#[derive(Clone, Debug, Default, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct MiningStats {
    /// Total work proofs submitted
    pub total_proofs: u64,
    /// Total valid hashes found
    pub total_work_count: u64,
    /// Last epoch with a proof
    pub last_proof_epoch: u64,
    /// Current calculated hashrate
    pub hashrate: u64,
}

impl MiningStats {
    /// Update stats with a new work proof
    pub fn record_proof(&mut self, proof: &WorkProof) {
        self.total_proofs += 1;
        self.total_work_count += proof.work_count;
        self.last_proof_epoch = proof.epoch;
    }

    /// Update hashrate
    pub fn update_hashrate(&mut self, hashrate: u64) {
        self.hashrate = hashrate;
    }
}

// ============ v2.0: AI Inference Types ============

/// Backend type for AI inference execution (v2.0)
///
/// Mirrors qfc_inference::BackendType but defined here for type-level
/// use without pulling in the full inference crate.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    ::core::hash::Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum BackendType {
    /// NVIDIA CUDA GPU
    Cuda,
    /// Apple Metal GPU (Apple Silicon)
    Metal,
    /// CPU-only fallback
    Cpu,
}

impl std::fmt::Display for BackendType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendType::Cuda => write!(f, "CUDA"),
            BackendType::Metal => write!(f, "Metal"),
            BackendType::Cpu => write!(f, "CPU"),
        }
    }
}

/// Model identifier for AI inference (v2.0)
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    ::core::hash::Hash,
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct ModelId {
    /// Model name (e.g. "llama-7b", "bert-base")
    pub name: String,
    /// Model version hash (content-addressed)
    pub version: String,
}

impl ModelId {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
        }
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// Compute task types supported by the network (v2.0)
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum ComputeTaskType {
    /// Text generation (LLM inference)
    TextGeneration {
        model_id: ModelId,
        prompt_hash: Hash,
        max_tokens: u32,
        temperature_fp: u32,
        seed: u64,
    },
    /// Image classification
    ImageClassification { model_id: ModelId, input_hash: Hash },
    /// Embedding generation
    Embedding { model_id: ModelId, input_hash: Hash },
    /// Generic ONNX model execution
    OnnxInference { model_hash: Hash, input_hash: Hash },
}

/// Inference proof submitted to the network (v2.0)
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
        let mut bytes = Vec::new();
        bytes.extend_from_slice(self.validator.as_bytes());
        bytes.extend_from_slice(&self.epoch.to_le_bytes());
        bytes.extend_from_slice(self.input_hash.as_bytes());
        bytes.extend_from_slice(self.output_hash.as_bytes());
        bytes.extend_from_slice(&self.execution_time_ms.to_le_bytes());
        bytes.extend_from_slice(&self.flops_estimated.to_le_bytes());
        bytes.extend_from_slice(&self.timestamp.to_le_bytes());
        bytes
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

/// Versioned compute proof — supports both v1 PoW and v2 inference (v2.0)
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub enum ComputeProof {
    /// v1 legacy (Blake3 PoW)
    PowV1(WorkProof),
    /// v2 AI inference
    InferenceV2(InferenceProof),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_work_proof_serialization() {
        let proof = WorkProof::new(
            Address::default(),
            100,
            12345,
            Hash::default(),
            42,
            1234567890,
        );

        let bytes = proof.to_bytes();
        let decoded = WorkProof::from_bytes(&bytes).unwrap();

        assert_eq!(proof.epoch, decoded.epoch);
        assert_eq!(proof.nonce, decoded.nonce);
        assert_eq!(proof.work_count, decoded.work_count);
    }

    #[test]
    fn test_work_proof_bytes_without_signature() {
        let proof = WorkProof::new(
            Address::default(),
            100,
            12345,
            Hash::default(),
            42,
            1234567890,
        );

        let bytes = proof.to_bytes_without_signature();
        // Should not include signature (64 bytes)
        assert!(bytes.len() < proof.to_bytes().len());
    }

    #[test]
    fn test_mining_task_serialization() {
        let task = MiningTask::new(100, [0u8; 32], U256::from_u64(1000), 1000, 2000);

        let bytes = task.to_bytes();
        let decoded = MiningTask::from_bytes(&bytes).unwrap();

        assert_eq!(task.epoch, decoded.epoch);
        assert_eq!(task.seed, decoded.seed);
    }

    #[test]
    fn test_mining_task_is_active() {
        let task = MiningTask::new(100, [0u8; 32], U256::from_u64(1000), 1000, 2000);

        assert!(!task.is_active(999)); // Before start
        assert!(task.is_active(1000)); // At start
        assert!(task.is_active(1500)); // During
        assert!(!task.is_active(2000)); // At end (exclusive)
        assert!(!task.is_active(2001)); // After end
    }

    #[test]
    fn test_difficulty_config_default() {
        let config = DifficultyConfig::default();
        assert_eq!(config.target_proofs_per_epoch, 10000);
        // min_difficulty is easier (higher target), max_difficulty is harder (lower target)
        // This ensures difficulty stays within reasonable bounds
        assert!(config.min_difficulty > config.max_difficulty);
    }

    #[test]
    fn test_mining_stats() {
        let mut stats = MiningStats::default();

        let proof = WorkProof::new(
            Address::default(),
            100,
            12345,
            Hash::default(),
            42,
            1234567890,
        );

        stats.record_proof(&proof);

        assert_eq!(stats.total_proofs, 1);
        assert_eq!(stats.total_work_count, 42);
        assert_eq!(stats.last_proof_epoch, 100);
    }

    // ============ v2.0 Tests ============

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
    fn test_compute_proof_enum() {
        let pow = ComputeProof::PowV1(WorkProof::new(
            Address::default(),
            1,
            42,
            Hash::ZERO,
            100,
            1234567890,
        ));
        assert!(matches!(pow, ComputeProof::PowV1(_)));

        let inf = ComputeProof::InferenceV2(InferenceProof::new(
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
        ));
        assert!(matches!(inf, ComputeProof::InferenceV2(_)));
    }

    #[test]
    fn test_backend_type_display() {
        assert_eq!(format!("{}", BackendType::Cuda), "CUDA");
        assert_eq!(format!("{}", BackendType::Metal), "Metal");
        assert_eq!(format!("{}", BackendType::Cpu), "CPU");
    }

    #[test]
    fn test_model_id_display() {
        let id = ModelId::new("llama-7b", "v1.0");
        assert_eq!(format!("{}", id), "llama-7b@v1.0");
    }
}
