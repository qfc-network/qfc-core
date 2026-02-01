//! Ed25519 signature operations

use crate::error::{CryptoError, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use qfc_types::{Hash, PublicKey, Signature};
use rand::rngs::OsRng;

/// Keypair for signing operations
#[derive(Clone)]
pub struct Keypair {
    signing_key: SigningKey,
}

impl Keypair {
    /// Generate a new random keypair
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self { signing_key }
    }

    /// Create keypair from secret bytes (32 bytes)
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(bytes);
        Ok(Self { signing_key })
    }

    /// Get the secret key bytes
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    /// Get the public key
    pub fn public_key(&self) -> PublicKey {
        let verifying_key = self.signing_key.verifying_key();
        PublicKey::new(verifying_key.to_bytes())
    }

    /// Sign a message
    pub fn sign(&self, message: &[u8]) -> Signature {
        let sig = self.signing_key.sign(message);
        Signature::new(sig.to_bytes())
    }

    /// Sign a hash
    pub fn sign_hash(&self, hash: &Hash) -> Signature {
        self.sign(hash.as_bytes())
    }
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair")
            .field("public_key", &self.public_key())
            .finish()
    }
}

/// Verify a signature against a public key and message
pub fn verify_signature(public_key: &PublicKey, message: &[u8], signature: &Signature) -> Result<()> {
    let verifying_key = VerifyingKey::from_bytes(public_key.as_bytes())
        .map_err(|_| CryptoError::InvalidPublicKey)?;

    let sig = ed25519_dalek::Signature::from_bytes(signature.as_bytes());

    verifying_key
        .verify(message, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Verify a signature against a public key and hash
pub fn verify_hash_signature(public_key: &PublicKey, hash: &Hash, signature: &Signature) -> Result<()> {
    verify_signature(public_key, hash.as_bytes(), signature)
}

/// Check if a signature is valid (returns bool instead of Result)
pub fn is_valid_signature(public_key: &PublicKey, message: &[u8], signature: &Signature) -> bool {
    verify_signature(public_key, message, signature).is_ok()
}

/// Check if a hash signature is valid
pub fn is_valid_hash_signature(public_key: &PublicKey, hash: &Hash, signature: &Signature) -> bool {
    verify_hash_signature(public_key, hash, signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_generation() {
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();

        // Different keypairs should have different public keys
        assert_ne!(kp1.public_key(), kp2.public_key());
    }

    #[test]
    fn test_keypair_from_secret() {
        let kp = Keypair::generate();
        let secret = kp.secret_bytes();

        let kp2 = Keypair::from_secret_bytes(&secret).unwrap();
        assert_eq!(kp.public_key(), kp2.public_key());
    }

    #[test]
    fn test_sign_and_verify() {
        let kp = Keypair::generate();
        let message = b"hello world";

        let signature = kp.sign(message);

        // Valid signature should verify
        assert!(verify_signature(&kp.public_key(), message, &signature).is_ok());

        // Wrong message should fail
        assert!(verify_signature(&kp.public_key(), b"wrong message", &signature).is_err());

        // Wrong public key should fail
        let other_kp = Keypair::generate();
        assert!(verify_signature(&other_kp.public_key(), message, &signature).is_err());
    }

    #[test]
    fn test_sign_hash() {
        let kp = Keypair::generate();
        let hash = crate::blake3_hash(b"test data");

        let signature = kp.sign_hash(&hash);
        assert!(verify_hash_signature(&kp.public_key(), &hash, &signature).is_ok());
    }

    #[test]
    fn test_is_valid_signature() {
        let kp = Keypair::generate();
        let message = b"test";
        let signature = kp.sign(message);

        assert!(is_valid_signature(&kp.public_key(), message, &signature));
        assert!(!is_valid_signature(&kp.public_key(), b"wrong", &signature));
    }
}
