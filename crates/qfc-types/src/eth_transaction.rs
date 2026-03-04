//! Ethereum transaction format support
//!
//! This module provides support for decoding Ethereum-formatted transactions
//! (RLP-encoded with secp256k1 signatures) and converting them to QFC's native format.

use crate::{Address, Hash, PublicKey, Signature, Transaction, TransactionType, U256};
use k256::ecdsa::{RecoveryId, Signature as K256Signature, VerifyingKey};
use rlp::Rlp;
use sha3::{Digest, Keccak256};
use thiserror::Error;

/// Errors that can occur when processing Ethereum transactions
#[derive(Debug, Error)]
pub enum EthTxError {
    #[error("Invalid RLP encoding: {0}")]
    InvalidRlp(String),
    #[error("Invalid signature: {0}")]
    InvalidSignature(String),
    #[error("Failed to recover sender: {0}")]
    RecoveryFailed(String),
    #[error("Invalid transaction type: {0}")]
    InvalidTxType(u8),
    #[error("Invalid chain ID")]
    InvalidChainId,
}

/// Decoded Ethereum transaction with recovered sender
#[derive(Debug, Clone)]
pub struct EthTransaction {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_price: U256,
    pub max_priority_fee_per_gas: Option<U256>,
    pub max_fee_per_gas: Option<U256>,
    pub gas_limit: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub data: Vec<u8>,
    pub v: u64,
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// Recovered sender address
    pub sender: Address,
    /// Transaction hash (keccak256 of raw bytes)
    pub hash: Hash,
    /// Whether this is an EIP-1559 transaction
    pub is_eip1559: bool,
}

impl EthTransaction {
    /// Decode an Ethereum transaction from raw RLP bytes
    pub fn decode(raw: &[u8]) -> Result<Self, EthTxError> {
        if raw.is_empty() {
            return Err(EthTxError::InvalidRlp("empty input".to_string()));
        }

        // Check for typed transaction (EIP-2718)
        // First byte < 0x80 means it's a transaction type prefix
        if raw[0] < 0x80 {
            match raw[0] {
                0x02 => Self::decode_eip1559(&raw[1..], raw),
                0x01 => Self::decode_eip2930(&raw[1..], raw),
                ty => Err(EthTxError::InvalidTxType(ty)),
            }
        } else {
            // Legacy transaction
            Self::decode_legacy(raw)
        }
    }

    /// Decode a legacy (pre-EIP-155 or EIP-155) transaction
    fn decode_legacy(raw: &[u8]) -> Result<Self, EthTxError> {
        let rlp = Rlp::new(raw);
        if !rlp.is_list() {
            return Err(EthTxError::InvalidRlp("expected RLP list".to_string()));
        }

        let item_count = rlp
            .item_count()
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        if item_count != 9 {
            return Err(EthTxError::InvalidRlp(format!(
                "expected 9 items, got {}",
                item_count
            )));
        }

        let nonce: u64 = rlp
            .val_at(0)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let gas_price = decode_u256(&rlp, 1)?;
        let gas_limit: u64 = rlp
            .val_at(2)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let to = decode_address(&rlp, 3)?;
        let value = decode_u256(&rlp, 4)?;
        let data: Vec<u8> = rlp
            .val_at(5)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let v: u64 = rlp
            .val_at(6)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let r = decode_bytes32(&rlp, 7)?;
        let s = decode_bytes32(&rlp, 8)?;

        // Extract chain_id from v (EIP-155)
        // v = chain_id * 2 + 35 + recovery_id (0 or 1)
        // For legacy (pre-EIP-155): v = 27 or 28
        let (chain_id, recovery_id) = if v >= 35 {
            // EIP-155
            let chain_id = (v - 35) / 2;
            let recovery_id = ((v - 35) % 2) as u8;
            (chain_id, recovery_id)
        } else if v == 27 || v == 28 {
            // Legacy (pre-EIP-155), assume chain_id 1
            (1, (v - 27) as u8)
        } else {
            return Err(EthTxError::InvalidSignature(format!(
                "invalid v value: {}",
                v
            )));
        };

        // Build signing hash for legacy transaction
        let signing_hash = if v >= 35 {
            // EIP-155: include chain_id in hash
            legacy_signing_hash_eip155(nonce, &gas_price, gas_limit, &to, &value, &data, chain_id)
        } else {
            // Pre-EIP-155
            legacy_signing_hash_pre155(nonce, &gas_price, gas_limit, &to, &value, &data)
        };

        // Recover sender
        let sender = recover_sender(&signing_hash, &r, &s, recovery_id)?;

        // Transaction hash is keccak256 of raw bytes
        let hash = keccak256(raw);

        Ok(Self {
            chain_id,
            nonce,
            gas_price,
            max_priority_fee_per_gas: None,
            max_fee_per_gas: None,
            gas_limit,
            to,
            value,
            data,
            v,
            r,
            s,
            sender,
            hash: Hash::new(hash),
            is_eip1559: false,
        })
    }

    /// Decode an EIP-1559 (type 2) transaction
    fn decode_eip1559(payload: &[u8], raw: &[u8]) -> Result<Self, EthTxError> {
        let rlp = Rlp::new(payload);
        if !rlp.is_list() {
            return Err(EthTxError::InvalidRlp("expected RLP list".to_string()));
        }

        let item_count = rlp
            .item_count()
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        if item_count != 12 {
            return Err(EthTxError::InvalidRlp(format!(
                "expected 12 items for EIP-1559, got {}",
                item_count
            )));
        }

        let chain_id: u64 = rlp
            .val_at(0)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let nonce: u64 = rlp
            .val_at(1)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let max_priority_fee_per_gas = decode_u256(&rlp, 2)?;
        let max_fee_per_gas = decode_u256(&rlp, 3)?;
        let gas_limit: u64 = rlp
            .val_at(4)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let to = decode_address(&rlp, 5)?;
        let value = decode_u256(&rlp, 6)?;
        let data: Vec<u8> = rlp
            .val_at(7)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        // Access list at index 8 - we skip it for now
        let v: u64 = rlp
            .val_at(9)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let r = decode_bytes32(&rlp, 10)?;
        let s = decode_bytes32(&rlp, 11)?;

        // For EIP-1559, v is just 0 or 1 (recovery_id)
        let recovery_id = v as u8;
        if recovery_id > 1 {
            return Err(EthTxError::InvalidSignature(format!(
                "invalid recovery id: {}",
                recovery_id
            )));
        }

        // Build signing hash for EIP-1559
        let signing_hash = eip1559_signing_hash(
            chain_id,
            nonce,
            &max_priority_fee_per_gas,
            &max_fee_per_gas,
            gas_limit,
            &to,
            &value,
            &data,
        );

        // Recover sender
        let sender = recover_sender(&signing_hash, &r, &s, recovery_id)?;

        // Transaction hash is keccak256 of raw bytes (including type prefix)
        let hash = keccak256(raw);

        Ok(Self {
            chain_id,
            nonce,
            gas_price: max_fee_per_gas, // Use max_fee as gas_price
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas_limit,
            to,
            value,
            data,
            v,
            r,
            s,
            sender,
            hash: Hash::new(hash),
            is_eip1559: true,
        })
    }

    /// Decode an EIP-2930 (type 1) transaction
    fn decode_eip2930(payload: &[u8], raw: &[u8]) -> Result<Self, EthTxError> {
        let rlp = Rlp::new(payload);
        if !rlp.is_list() {
            return Err(EthTxError::InvalidRlp("expected RLP list".to_string()));
        }

        let item_count = rlp
            .item_count()
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        if item_count != 11 {
            return Err(EthTxError::InvalidRlp(format!(
                "expected 11 items for EIP-2930, got {}",
                item_count
            )));
        }

        let chain_id: u64 = rlp
            .val_at(0)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let nonce: u64 = rlp
            .val_at(1)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let gas_price = decode_u256(&rlp, 2)?;
        let gas_limit: u64 = rlp
            .val_at(3)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let to = decode_address(&rlp, 4)?;
        let value = decode_u256(&rlp, 5)?;
        let data: Vec<u8> = rlp
            .val_at(6)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        // Access list at index 7 - we skip it
        let v: u64 = rlp
            .val_at(8)
            .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
        let r = decode_bytes32(&rlp, 9)?;
        let s = decode_bytes32(&rlp, 10)?;

        let recovery_id = v as u8;
        if recovery_id > 1 {
            return Err(EthTxError::InvalidSignature(format!(
                "invalid recovery id: {}",
                recovery_id
            )));
        }

        // Build signing hash for EIP-2930
        let signing_hash =
            eip2930_signing_hash(chain_id, nonce, &gas_price, gas_limit, &to, &value, &data);

        let sender = recover_sender(&signing_hash, &r, &s, recovery_id)?;
        let hash = keccak256(raw);

        Ok(Self {
            chain_id,
            nonce,
            gas_price,
            max_priority_fee_per_gas: None,
            max_fee_per_gas: None,
            gas_limit,
            to,
            value,
            data,
            v,
            r,
            s,
            sender,
            hash: Hash::new(hash),
            is_eip1559: false,
        })
    }

    /// Convert to QFC's native Transaction format
    ///
    /// Note: The signature fields (public_key, signature) will be set to
    /// placeholder values since QFC uses Ed25519 while Ethereum uses secp256k1.
    /// The sender is already recovered and stored separately.
    pub fn to_qfc_transaction(&self) -> Transaction {
        let tx_type = if self.to.is_none() && !self.data.is_empty() {
            TransactionType::ContractCreate
        } else if self.to.is_some() && !self.data.is_empty() {
            TransactionType::ContractCall
        } else {
            TransactionType::Transfer
        };

        Transaction {
            tx_type,
            chain_id: self.chain_id,
            nonce: self.nonce,
            to: self.to,
            value: self.value,
            data: self.data.clone(),
            gas_limit: self.gas_limit,
            gas_price: self.gas_price,
            // These are placeholders - the actual verification uses secp256k1
            public_key: PublicKey::ZERO,
            signature: Signature::ZERO,
        }
    }
}

// Helper functions

fn decode_u256(rlp: &Rlp, index: usize) -> Result<U256, EthTxError> {
    let bytes: Vec<u8> = rlp
        .val_at(index)
        .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
    if bytes.is_empty() {
        return Ok(U256::ZERO);
    }
    if bytes.len() > 32 {
        return Err(EthTxError::InvalidRlp("U256 overflow".to_string()));
    }
    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(U256::from_be_bytes(&padded))
}

fn decode_address(rlp: &Rlp, index: usize) -> Result<Option<Address>, EthTxError> {
    let bytes: Vec<u8> = rlp
        .val_at(index)
        .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() != 20 {
        return Err(EthTxError::InvalidRlp(format!(
            "invalid address length: {}",
            bytes.len()
        )));
    }
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes);
    Ok(Some(Address::new(addr)))
}

fn decode_bytes32(rlp: &Rlp, index: usize) -> Result<[u8; 32], EthTxError> {
    let bytes: Vec<u8> = rlp
        .val_at(index)
        .map_err(|e| EthTxError::InvalidRlp(e.to_string()))?;
    if bytes.len() > 32 {
        return Err(EthTxError::InvalidRlp("bytes32 overflow".to_string()));
    }
    let mut result = [0u8; 32];
    // Right-align the bytes (for r and s in signatures)
    result[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(result)
}

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

fn recover_sender(
    message_hash: &[u8; 32],
    r: &[u8; 32],
    s: &[u8; 32],
    recovery_id: u8,
) -> Result<Address, EthTxError> {
    // Construct signature from r and s
    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(r);
    sig_bytes[32..].copy_from_slice(s);

    let signature = K256Signature::from_slice(&sig_bytes)
        .map_err(|e| EthTxError::InvalidSignature(e.to_string()))?;

    let recid = RecoveryId::try_from(recovery_id)
        .map_err(|_| EthTxError::InvalidSignature("invalid recovery id".to_string()))?;

    // Recover the public key
    let recovered_key = VerifyingKey::recover_from_prehash(message_hash, &signature, recid)
        .map_err(|e| EthTxError::RecoveryFailed(e.to_string()))?;

    // Get uncompressed public key (65 bytes: 0x04 || x || y)
    let pubkey_bytes = recovered_key.to_encoded_point(false);
    let pubkey_uncompressed = pubkey_bytes.as_bytes();

    // Address is keccak256(pubkey[1..65])[12..32]
    // Skip the 0x04 prefix
    let pubkey_hash = keccak256(&pubkey_uncompressed[1..]);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&pubkey_hash[12..]);

    Ok(Address::new(addr))
}

/// Build signing hash for legacy transaction with EIP-155
fn legacy_signing_hash_eip155(
    nonce: u64,
    gas_price: &U256,
    gas_limit: u64,
    to: &Option<Address>,
    value: &U256,
    data: &[u8],
    chain_id: u64,
) -> [u8; 32] {
    use rlp::RlpStream;

    let mut stream = RlpStream::new_list(9);
    stream.append(&nonce);
    append_u256(&mut stream, gas_price);
    stream.append(&gas_limit);
    if let Some(addr) = to {
        stream.append(&addr.0.as_slice());
    } else {
        stream.append(&"");
    }
    append_u256(&mut stream, value);
    stream.append(&data);
    stream.append(&chain_id);
    stream.append(&0u8);
    stream.append(&0u8);

    keccak256(&stream.out())
}

/// Build signing hash for legacy transaction (pre-EIP-155)
fn legacy_signing_hash_pre155(
    nonce: u64,
    gas_price: &U256,
    gas_limit: u64,
    to: &Option<Address>,
    value: &U256,
    data: &[u8],
) -> [u8; 32] {
    use rlp::RlpStream;

    let mut stream = RlpStream::new_list(6);
    stream.append(&nonce);
    append_u256(&mut stream, gas_price);
    stream.append(&gas_limit);
    if let Some(addr) = to {
        stream.append(&addr.0.as_slice());
    } else {
        stream.append(&"");
    }
    append_u256(&mut stream, value);
    stream.append(&data);

    keccak256(&stream.out())
}

/// Build signing hash for EIP-1559 transaction
fn eip1559_signing_hash(
    chain_id: u64,
    nonce: u64,
    max_priority_fee_per_gas: &U256,
    max_fee_per_gas: &U256,
    gas_limit: u64,
    to: &Option<Address>,
    value: &U256,
    data: &[u8],
) -> [u8; 32] {
    use rlp::RlpStream;

    let mut stream = RlpStream::new_list(9);
    stream.append(&chain_id);
    stream.append(&nonce);
    append_u256(&mut stream, max_priority_fee_per_gas);
    append_u256(&mut stream, max_fee_per_gas);
    stream.append(&gas_limit);
    if let Some(addr) = to {
        stream.append(&addr.0.as_slice());
    } else {
        stream.append(&"");
    }
    append_u256(&mut stream, value);
    stream.append(&data);
    // Empty access list
    stream.begin_list(0);

    // Type 2 prefix + RLP
    let mut payload = vec![0x02];
    payload.extend_from_slice(&stream.out());
    keccak256(&payload)
}

/// Build signing hash for EIP-2930 transaction
fn eip2930_signing_hash(
    chain_id: u64,
    nonce: u64,
    gas_price: &U256,
    gas_limit: u64,
    to: &Option<Address>,
    value: &U256,
    data: &[u8],
) -> [u8; 32] {
    use rlp::RlpStream;

    let mut stream = RlpStream::new_list(8);
    stream.append(&chain_id);
    stream.append(&nonce);
    append_u256(&mut stream, gas_price);
    stream.append(&gas_limit);
    if let Some(addr) = to {
        stream.append(&addr.0.as_slice());
    } else {
        stream.append(&"");
    }
    append_u256(&mut stream, value);
    stream.append(&data);
    // Empty access list
    stream.begin_list(0);

    // Type 1 prefix + RLP
    let mut payload = vec![0x01];
    payload.extend_from_slice(&stream.out());
    keccak256(&payload)
}

fn append_u256(stream: &mut rlp::RlpStream, value: &U256) {
    let bytes = value.to_be_bytes();
    // Find first non-zero byte
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(32);
    stream.append(&bytes[start..].to_vec());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_legacy_transaction() {
        // A real legacy transaction from Ethereum mainnet
        // This is a simple ETH transfer
        let raw = hex::decode(
            "f86c098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a76400008025a028ef61340bd939bc2195fe537567866003e1a15d3c71ff63e1590620aa636276a067cbe9d8997f761aecb703304b3800ccf555c9f3dc64214b297fb1966a3b6d83"
        ).unwrap();

        let tx = EthTransaction::decode(&raw).unwrap();

        assert_eq!(tx.nonce, 9);
        assert_eq!(tx.gas_limit, 21000);
        assert!(!tx.is_eip1559);
        assert!(tx.to.is_some());
    }

    #[test]
    fn test_keccak256() {
        let input = b"hello";
        let hash = keccak256(input);
        let expected =
            hex::decode("1c8aff950685c2ed4bc3174f3472287b56d9517b9c948127319a09a7a36deac8")
                .unwrap();
        assert_eq!(hash[..], expected[..]);
    }
}
