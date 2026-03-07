//! Buffer for verified inference proofs awaiting inclusion in a block

use std::collections::{HashSet, VecDeque};

use qfc_types::{Hash, InferenceProof};

/// Pool of verified inference proofs waiting to be packed into blocks
pub struct ProofPool {
    /// Pending proofs in FIFO order
    pending: VecDeque<InferenceProof>,
    /// Proof hashes already seen (dedup)
    seen: HashSet<Hash>,
    /// Maximum pool size
    max_size: usize,
    /// Total proofs accepted (for metrics)
    total_accepted: u64,
    /// Total add() calls (for metrics)
    total_submissions: u64,
}

impl ProofPool {
    pub fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            seen: HashSet::new(),
            max_size: 2000,
            total_accepted: 0,
            total_submissions: 0,
        }
    }

    /// Add a proof to the pool. Returns true if accepted (new), false if duplicate or full.
    pub fn add(&mut self, proof: InferenceProof) -> bool {
        self.total_submissions += 1;
        let proof_hash = qfc_crypto::blake3_hash(&proof.to_bytes_without_signature());
        if self.seen.contains(&proof_hash) {
            return false;
        }
        if self.pending.len() >= self.max_size {
            return false;
        }
        self.seen.insert(proof_hash);
        self.pending.push_back(proof);
        self.total_accepted += 1;
        true
    }

    /// Drain up to `max` proofs from the pool for block inclusion.
    pub fn drain(&mut self, max: usize) -> Vec<InferenceProof> {
        let count = max.min(self.pending.len());
        let proofs: Vec<InferenceProof> = self.pending.drain(..count).collect();
        // Clean seen set for drained proofs (they'll be in a block now)
        for proof in &proofs {
            let proof_hash = qfc_crypto::blake3_hash(&proof.to_bytes_without_signature());
            self.seen.remove(&proof_hash);
        }
        proofs
    }

    /// Number of pending proofs
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Total proofs accepted since start
    pub fn total_accepted(&self) -> u64 {
        self.total_accepted
    }

    /// Total add() calls since start
    pub fn total_submissions(&self) -> u64 {
        self.total_submissions
    }
}

impl Default for ProofPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_types::{Address, BackendType, ComputeTaskType, ModelId};

    fn make_proof(epoch: u64) -> InferenceProof {
        InferenceProof::new(
            Address::new([epoch as u8; 20]),
            epoch,
            ComputeTaskType::Embedding {
                model_id: ModelId::new("test", "v1"),
                input_hash: Hash::ZERO,
            },
            Hash::ZERO,
            Hash::ZERO,
            100,
            1000,
            BackendType::Cpu,
            0,
        )
    }

    #[test]
    fn test_add_and_drain() {
        let mut pool = ProofPool::new();
        assert_eq!(pool.pending_count(), 0);

        pool.add(make_proof(1));
        pool.add(make_proof(2));
        assert_eq!(pool.pending_count(), 2);

        let drained = pool.drain(1);
        assert_eq!(drained.len(), 1);
        assert_eq!(pool.pending_count(), 1);

        let drained = pool.drain(100);
        assert_eq!(drained.len(), 1);
        assert_eq!(pool.pending_count(), 0);
    }

    #[test]
    fn test_dedup() {
        let mut pool = ProofPool::new();
        let proof = make_proof(1);
        assert!(pool.add(proof.clone()));
        assert!(!pool.add(proof)); // duplicate
        assert_eq!(pool.pending_count(), 1);
    }
}
