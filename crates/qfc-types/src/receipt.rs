//! Receipt and Log types

use crate::{Address, Hash};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Receipt execution status
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub enum ReceiptStatus {
    /// Transaction succeeded
    Success,
    /// Transaction failed with error message
    Failure(String),
    /// Transaction ran out of gas
    OutOfGas,
    /// Transaction was reverted
    Reverted,
}

impl Default for ReceiptStatus {
    fn default() -> Self {
        Self::Success
    }
}

impl ReceiptStatus {
    /// Check if execution was successful
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Get status code (1 = success, 0 = failure)
    pub fn status_code(&self) -> u8 {
        if self.is_success() {
            1
        } else {
            0
        }
    }
}

/// Bloom filter for logs (256 bytes = 2048 bits)
#[derive(Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Bloom(pub [u8; 256]);

impl Serialize for Bloom {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", hex::encode(&self.0)))
    }
}

impl<'de> Deserialize<'de> for Bloom {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        let s = s.strip_prefix("0x").unwrap_or(&s);
        let bytes = hex::decode(s).map_err(serde::de::Error::custom)?;
        if bytes.len() != 256 {
            return Err(serde::de::Error::custom("invalid bloom length"));
        }
        let mut arr = [0u8; 256];
        arr.copy_from_slice(&bytes);
        Ok(Bloom(arr))
    }
}

impl Default for Bloom {
    fn default() -> Self {
        Self([0u8; 256])
    }
}

impl std::fmt::Debug for Bloom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bloom(0x{}...)", hex::encode(&self.0[..8]))
    }
}

impl Bloom {
    pub const ZERO: Bloom = Bloom([0u8; 256]);

    /// Create a new empty bloom filter
    pub fn new() -> Self {
        Self::default()
    }

    /// Add data to bloom filter
    pub fn accrue(&mut self, data: &[u8]) {
        let hash = blake3::hash(data);
        let hash_bytes = hash.as_bytes();

        // Use first 6 bytes to set 3 bits
        for i in 0..3 {
            let bit_index =
                ((hash_bytes[i * 2] as usize) << 8 | hash_bytes[i * 2 + 1] as usize) & 2047;
            let byte_index = bit_index / 8;
            let bit_position = bit_index % 8;
            self.0[byte_index] |= 1 << bit_position;
        }
    }

    /// Check if bloom filter might contain data
    pub fn contains(&self, data: &[u8]) -> bool {
        let hash = blake3::hash(data);
        let hash_bytes = hash.as_bytes();

        for i in 0..3 {
            let bit_index =
                ((hash_bytes[i * 2] as usize) << 8 | hash_bytes[i * 2 + 1] as usize) & 2047;
            let byte_index = bit_index / 8;
            let bit_position = bit_index % 8;
            if self.0[byte_index] & (1 << bit_position) == 0 {
                return false;
            }
        }
        true
    }

    /// Combine with another bloom filter (OR)
    pub fn accrue_bloom(&mut self, other: &Bloom) {
        for i in 0..256 {
            self.0[i] |= other.0[i];
        }
    }

    /// Check if bloom is empty
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&b| b == 0)
    }
}

/// Event log
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Log {
    /// Contract address that emitted the log
    pub address: Address,

    /// Indexed topics (up to 4)
    pub topics: Vec<Hash>,

    /// Log data (non-indexed)
    pub data: Vec<u8>,
}

impl Log {
    /// Create a new log
    pub fn new(address: Address, topics: Vec<Hash>, data: Vec<u8>) -> Self {
        Self {
            address,
            topics,
            data,
        }
    }

    /// Create bloom filter for this log
    pub fn bloom(&self) -> Bloom {
        let mut bloom = Bloom::new();
        bloom.accrue(self.address.as_bytes());
        for topic in &self.topics {
            bloom.accrue(topic.as_bytes());
        }
        bloom
    }
}

/// Transaction receipt
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, BorshSerialize, BorshDeserialize)]
pub struct Receipt {
    /// Transaction hash
    pub tx_hash: Hash,

    /// Transaction index in block
    pub tx_index: u32,

    /// Execution status
    pub status: ReceiptStatus,

    /// Cumulative gas used (up to this tx in block)
    pub cumulative_gas_used: u64,

    /// Gas used by this transaction
    pub gas_used: u64,

    /// Logs emitted
    pub logs: Vec<Log>,

    /// Logs bloom filter
    pub logs_bloom: Bloom,

    /// Contract address (if contract creation)
    pub contract_address: Option<Address>,
}

impl Default for Receipt {
    fn default() -> Self {
        Self {
            tx_hash: Hash::ZERO,
            tx_index: 0,
            status: ReceiptStatus::Success,
            cumulative_gas_used: 0,
            gas_used: 0,
            logs: Vec::new(),
            logs_bloom: Bloom::default(),
            contract_address: None,
        }
    }
}

impl Receipt {
    /// Create a new successful receipt
    pub fn success(tx_hash: Hash, tx_index: u32, gas_used: u64, cumulative_gas_used: u64) -> Self {
        Self {
            tx_hash,
            tx_index,
            status: ReceiptStatus::Success,
            cumulative_gas_used,
            gas_used,
            logs: Vec::new(),
            logs_bloom: Bloom::default(),
            contract_address: None,
        }
    }

    /// Create a new failed receipt
    pub fn failure(
        tx_hash: Hash,
        tx_index: u32,
        gas_used: u64,
        cumulative_gas_used: u64,
        error: String,
    ) -> Self {
        Self {
            tx_hash,
            tx_index,
            status: ReceiptStatus::Failure(error),
            cumulative_gas_used,
            gas_used,
            logs: Vec::new(),
            logs_bloom: Bloom::default(),
            contract_address: None,
        }
    }

    /// Check if execution was successful
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Add a log
    pub fn add_log(&mut self, log: Log) {
        self.logs_bloom.accrue_bloom(&log.bloom());
        self.logs.push(log);
    }

    /// Set contract address
    pub fn set_contract_address(&mut self, address: Address) {
        self.contract_address = Some(address);
    }

    /// Compute bloom from logs
    pub fn compute_bloom(&mut self) {
        self.logs_bloom = Bloom::new();
        for log in &self.logs {
            self.logs_bloom.accrue_bloom(&log.bloom());
        }
    }

    /// Serialize receipt
    pub fn to_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("serialization should not fail")
    }

    /// Deserialize receipt
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, borsh::io::Error> {
        borsh::from_slice(bytes)
    }
}

/// Create bloom filter from multiple logs
pub fn create_bloom(logs: &[Log]) -> Bloom {
    let mut bloom = Bloom::new();
    for log in logs {
        bloom.accrue_bloom(&log.bloom());
    }
    bloom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_filter() {
        let mut bloom = Bloom::new();
        let data = b"test data";

        assert!(!bloom.contains(data));
        bloom.accrue(data);
        assert!(bloom.contains(data));
    }

    #[test]
    fn test_log_bloom() {
        let log = Log::new(
            Address::new([0x11; 20]),
            vec![Hash::new([0x22; 32])],
            vec![1, 2, 3],
        );

        let bloom = log.bloom();
        assert!(!bloom.is_empty());
        assert!(bloom.contains(&[0x11; 20]));
        assert!(bloom.contains(&[0x22; 32]));
    }

    #[test]
    fn test_receipt_serialization() {
        let receipt = Receipt::success(Hash::new([0x11; 32]), 0, 21000, 21000);

        let bytes = receipt.to_bytes();
        let decoded = Receipt::from_bytes(&bytes).unwrap();
        assert_eq!(receipt, decoded);
    }

    #[test]
    fn test_receipt_status() {
        let success = ReceiptStatus::Success;
        assert!(success.is_success());
        assert_eq!(success.status_code(), 1);

        let failure = ReceiptStatus::Failure("error".to_string());
        assert!(!failure.is_success());
        assert_eq!(failure.status_code(), 0);
    }
}
