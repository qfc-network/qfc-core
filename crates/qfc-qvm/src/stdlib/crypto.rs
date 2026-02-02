//! Cryptographic standard library functions
//!
//! Provides hashing and signature verification for QuantumScript contracts.

use primitive_types::{H160, H256, U256};

use crate::executor::{ExecutionError, ExecutionResult};
use crate::value::Value;
use super::StdlibContext;

/// Keccak-256 hash (Ethereum compatible)
/// crypto::keccak256(data: bytes) -> bytes32
pub fn keccak256(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    let data = get_bytes(&args, 0, "keccak256")?;

    // Use tiny-keccak for Ethereum compatibility
    use tiny_keccak::{Hasher, Keccak};
    let mut hasher = Keccak::v256();
    hasher.update(&data);
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);

    Ok(Value::Bytes32(H256::from(output)))
}

/// SHA-256 hash
/// crypto::sha256(data: bytes) -> bytes32
pub fn sha256(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    let data = get_bytes(&args, 0, "sha256")?;

    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let result = hasher.finalize();

    Ok(Value::Bytes32(H256::from_slice(&result)))
}

/// Blake3 hash (QFC native)
/// crypto::blake3(data: bytes) -> bytes32
pub fn blake3(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    let data = get_bytes(&args, 0, "blake3")?;

    let hash = blake3::hash(&data);
    Ok(Value::Bytes32(H256::from_slice(hash.as_bytes())))
}

/// Recover signer address from signature (Ethereum compatible)
/// crypto::ecrecover(hash: bytes32, v: u256, r: bytes32, s: bytes32) -> address
pub fn ecrecover(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    if args.len() != 4 {
        return Err(ExecutionError::Internal(
            "ecrecover() expects 4 arguments".to_string()
        ));
    }

    let hash = get_h256(&args[0], "ecrecover")?;
    let v = get_u256(&args[1], "ecrecover")?;
    let r = get_h256(&args[2], "ecrecover")?;
    let s = get_h256(&args[3], "ecrecover")?;

    // Convert v to recovery id (0 or 1)
    let v_byte = if v == U256::from(27) {
        0u8
    } else if v == U256::from(28) {
        1u8
    } else if v == U256::zero() {
        0u8
    } else if v == U256::one() {
        1u8
    } else {
        return Ok(Value::Address(H160::zero())); // Invalid v
    };

    // Use k256 for secp256k1 ECDSA recovery
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    use k256::ecdsa::signature::hazmat::PrehashVerifier;

    // Construct signature from r and s
    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(r.as_bytes());
    sig_bytes[32..].copy_from_slice(s.as_bytes());

    let signature = match Signature::from_slice(&sig_bytes) {
        Ok(sig) => sig,
        Err(_) => return Ok(Value::Address(H160::zero())),
    };

    let recovery_id = match RecoveryId::try_from(v_byte) {
        Ok(id) => id,
        Err(_) => return Ok(Value::Address(H160::zero())),
    };

    // Recover public key
    let recovered_key = match VerifyingKey::recover_from_prehash(
        hash.as_bytes(),
        &signature,
        recovery_id,
    ) {
        Ok(key) => key,
        Err(_) => return Ok(Value::Address(H160::zero())),
    };

    // Convert public key to Ethereum address
    let public_key_bytes = recovered_key.to_encoded_point(false);
    let public_key_hash = {
        use tiny_keccak::{Hasher, Keccak};
        let mut hasher = Keccak::v256();
        // Skip the 0x04 prefix byte
        hasher.update(&public_key_bytes.as_bytes()[1..]);
        let mut output = [0u8; 32];
        hasher.finalize(&mut output);
        output
    };

    // Take last 20 bytes as address
    let address = H160::from_slice(&public_key_hash[12..32]);

    Ok(Value::Address(address))
}

/// Verify a signature (Ed25519 for QFC native)
/// crypto::verify(message: bytes, signature: bytes, pubkey: bytes) -> bool
pub fn verify(_ctx: &mut StdlibContext, args: Vec<Value>) -> ExecutionResult<Value> {
    if args.len() != 3 {
        return Err(ExecutionError::Internal(
            "verify() expects 3 arguments".to_string()
        ));
    }

    let message = get_bytes(&args, 0, "verify")?;
    let signature = get_bytes(&args, 1, "verify")?;
    let pubkey = get_bytes(&args, 2, "verify")?;

    // Ed25519 verification
    if signature.len() != 64 || pubkey.len() != 32 {
        return Ok(Value::Bool(false));
    }

    use ed25519_dalek::{Signature as Ed25519Sig, VerifyingKey, Verifier};

    let pubkey_bytes: [u8; 32] = match pubkey.try_into() {
        Ok(b) => b,
        Err(_) => return Ok(Value::Bool(false)),
    };

    let sig_bytes: [u8; 64] = match signature.try_into() {
        Ok(b) => b,
        Err(_) => return Ok(Value::Bool(false)),
    };

    let verifying_key = match VerifyingKey::from_bytes(&pubkey_bytes) {
        Ok(k) => k,
        Err(_) => return Ok(Value::Bool(false)),
    };

    let sig = Ed25519Sig::from_bytes(&sig_bytes);

    let result = verifying_key.verify(&message, &sig).is_ok();
    Ok(Value::Bool(result))
}

// Helper functions

fn get_bytes(args: &[Value], index: usize, func: &str) -> ExecutionResult<Vec<u8>> {
    if index >= args.len() {
        return Err(ExecutionError::Internal(format!(
            "{}() missing argument {}",
            func, index
        )));
    }

    match &args[index] {
        Value::Bytes(b) => Ok(b.clone()),
        Value::Bytes32(h) => Ok(h.as_bytes().to_vec()),
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        other => Err(ExecutionError::TypeError {
            expected: "bytes".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

fn get_h256(value: &Value, func: &str) -> ExecutionResult<H256> {
    match value {
        Value::Bytes32(h) => Ok(*h),
        Value::U256(n) => {
            let mut bytes = [0u8; 32];
            n.to_big_endian(&mut bytes);
            Ok(H256::from(bytes))
        }
        other => Err(ExecutionError::TypeError {
            expected: "bytes32".to_string(),
            found: other.type_name().to_string(),
        }),
    }
}

fn get_u256(value: &Value, func: &str) -> ExecutionResult<U256> {
    value.as_u256().ok_or_else(|| {
        ExecutionError::TypeError {
            expected: "u256".to_string(),
            found: value.type_name().to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> StdlibContext<'static> {
        static mut MEM: Vec<u8> = Vec::new();
        StdlibContext {
            address: H160::zero(),
            caller: H160::zero(),
            value: U256::zero(),
            block_number: 0,
            timestamp: 0,
            memory: unsafe { &mut MEM },
        }
    }

    #[test]
    fn test_keccak256() {
        let mut c = ctx();
        let result = keccak256(&mut c, vec![Value::Bytes(b"hello".to_vec())]).unwrap();

        // Expected: keccak256("hello")
        if let Value::Bytes32(hash) = result {
            assert!(!hash.is_zero());
        } else {
            panic!("Expected Bytes32");
        }
    }

    #[test]
    fn test_sha256() {
        let mut c = ctx();
        let result = sha256(&mut c, vec![Value::Bytes(b"hello".to_vec())]).unwrap();

        if let Value::Bytes32(hash) = result {
            // SHA256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
            let expected = hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap();
            assert_eq!(hash.as_bytes(), expected.as_slice());
        } else {
            panic!("Expected Bytes32");
        }
    }

    #[test]
    fn test_blake3() {
        let mut c = ctx();
        let result = blake3(&mut c, vec![Value::Bytes(b"hello".to_vec())]).unwrap();

        if let Value::Bytes32(hash) = result {
            assert!(!hash.is_zero());
        } else {
            panic!("Expected Bytes32");
        }
    }
}
