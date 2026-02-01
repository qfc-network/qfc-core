//! VRF (Verifiable Random Function) implementation
//!
//! This is a simplified VRF using Ed25519 signatures as the base.
//! The VRF output is derived from signing the input with a secret key,
//! and anyone can verify the output using the public key.

use crate::error::{CryptoError, Result};
use crate::signature::Keypair;
use qfc_types::{Hash, PublicKey, VrfProof};

/// VRF keypair wrapper
pub struct VrfKeypair {
    keypair: Keypair,
}

impl VrfKeypair {
    /// Generate a new VRF keypair
    pub fn generate() -> Self {
        Self {
            keypair: Keypair::generate(),
        }
    }

    /// Create from existing keypair
    pub fn from_keypair(keypair: Keypair) -> Self {
        Self { keypair }
    }

    /// Create from secret bytes
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Result<Self> {
        Ok(Self {
            keypair: Keypair::from_secret_bytes(bytes)?,
        })
    }

    /// Get the public key
    pub fn public_key(&self) -> PublicKey {
        self.keypair.public_key()
    }

    /// Generate VRF proof for an input
    ///
    /// Returns (output, proof) where:
    /// - output is a 32-byte hash derived from the proof
    /// - proof is the VRF proof that can be verified
    pub fn prove(&self, input: &[u8]) -> VrfProof {
        // Sign the input to create the proof
        let signature = self.keypair.sign(input);

        // Derive the output by hashing the signature
        let output_hash = blake3::hash(signature.as_bytes());

        VrfProof {
            output: *output_hash.as_bytes(),
            proof: *signature.as_bytes(),
        }
    }

    /// Generate VRF proof with epoch seed
    pub fn prove_with_seed(&self, seed: &[u8; 32]) -> VrfProof {
        self.prove(seed)
    }
}

/// Verify a VRF proof
///
/// Returns the VRF output if the proof is valid
pub fn vrf_verify(public_key: &PublicKey, input: &[u8], vrf_proof: &VrfProof) -> Result<[u8; 32]> {
    // Reconstruct the signature from the proof
    let signature = qfc_types::Signature::new(vrf_proof.proof);

    // Verify the signature
    crate::verify_signature(public_key, input, &signature)?;

    // Recompute the output from the proof
    let computed_output = blake3::hash(&vrf_proof.proof);

    // Verify the output matches
    if computed_output.as_bytes() != &vrf_proof.output {
        return Err(CryptoError::InvalidVrfProof);
    }

    Ok(vrf_proof.output)
}

/// Verify a VRF proof with epoch seed
pub fn vrf_verify_with_seed(
    public_key: &PublicKey,
    seed: &[u8; 32],
    vrf_proof: &VrfProof,
) -> Result<[u8; 32]> {
    vrf_verify(public_key, seed, vrf_proof)
}

/// Convert VRF output to a random value in range [0, 1)
pub fn vrf_output_to_f64(output: &[u8; 32]) -> f64 {
    // Use first 8 bytes as u64
    let value = u64::from_le_bytes(output[0..8].try_into().unwrap());
    value as f64 / u64::MAX as f64
}

/// Convert VRF output to a value in range [0, max)
pub fn vrf_output_to_range(output: &[u8; 32], max: u64) -> u64 {
    let value = u64::from_le_bytes(output[0..8].try_into().unwrap());
    value % max
}

/// Check if VRF output is below threshold (for block producer selection)
pub fn vrf_output_below_threshold(output: &[u8; 32], threshold: f64) -> bool {
    let random_value = vrf_output_to_f64(output);
    random_value < threshold
}

/// Compute VRF-based hash for block selection
pub fn compute_selection_hash(vrf_output: &[u8; 32], stake: u64, total_stake: u64) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(vrf_output);
    hasher.update(&stake.to_le_bytes());
    hasher.update(&total_stake.to_le_bytes());
    Hash::new(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vrf_prove_and_verify() {
        let vrf_kp = VrfKeypair::generate();
        let input = b"test input";

        let proof = vrf_kp.prove(input);

        // Verify should succeed
        let output = vrf_verify(&vrf_kp.public_key(), input, &proof).unwrap();
        assert_eq!(output, proof.output);
    }

    #[test]
    fn test_vrf_deterministic() {
        let vrf_kp = VrfKeypair::generate();
        let input = b"same input";

        let proof1 = vrf_kp.prove(input);
        let proof2 = vrf_kp.prove(input);

        // Same input should produce same output
        assert_eq!(proof1.output, proof2.output);
    }

    #[test]
    fn test_vrf_different_inputs() {
        let vrf_kp = VrfKeypair::generate();

        let proof1 = vrf_kp.prove(b"input 1");
        let proof2 = vrf_kp.prove(b"input 2");

        // Different inputs should produce different outputs
        assert_ne!(proof1.output, proof2.output);
    }

    #[test]
    fn test_vrf_wrong_public_key() {
        let vrf_kp1 = VrfKeypair::generate();
        let vrf_kp2 = VrfKeypair::generate();
        let input = b"test";

        let proof = vrf_kp1.prove(input);

        // Verification with wrong public key should fail
        assert!(vrf_verify(&vrf_kp2.public_key(), input, &proof).is_err());
    }

    #[test]
    fn test_vrf_wrong_input() {
        let vrf_kp = VrfKeypair::generate();
        let input = b"correct input";

        let proof = vrf_kp.prove(input);

        // Verification with wrong input should fail
        assert!(vrf_verify(&vrf_kp.public_key(), b"wrong input", &proof).is_err());
    }

    #[test]
    fn test_vrf_output_to_f64() {
        let output = [0xff; 32];
        let value = vrf_output_to_f64(&output);
        assert!(value > 0.99);

        let output = [0x00; 32];
        let value = vrf_output_to_f64(&output);
        assert!(value < 0.01);
    }

    #[test]
    fn test_vrf_output_to_range() {
        let mut output = [0u8; 32];
        output[0] = 0x10;
        let value = vrf_output_to_range(&output, 100);
        assert!(value < 100);
    }

    #[test]
    fn test_vrf_with_seed() {
        let vrf_kp = VrfKeypair::generate();
        let seed = [0xab; 32];

        let proof = vrf_kp.prove_with_seed(&seed);
        let output = vrf_verify_with_seed(&vrf_kp.public_key(), &seed, &proof).unwrap();
        assert_eq!(output, proof.output);
    }
}
