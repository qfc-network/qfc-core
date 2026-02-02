//! RPC types for JSON serialization

use qfc_types::{Address, Block, Hash, Receipt, Transaction};
use serde::{Deserialize, Deserializer, Serialize};

/// Block number parameter - handles both hex strings ("0x0") and tags ("latest")
#[derive(Clone, Debug)]
pub enum BlockNumber {
    /// Specific block number
    Number(u64),
    /// Block tag
    Tag(BlockTag),
}

impl Serialize for BlockNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            BlockNumber::Number(n) => serializer.serialize_str(&format!("0x{:x}", n)),
            BlockNumber::Tag(tag) => tag.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for BlockNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        // Try parsing as a tag first
        match s.to_lowercase().as_str() {
            "latest" => return Ok(BlockNumber::Tag(BlockTag::Latest)),
            "earliest" => return Ok(BlockNumber::Tag(BlockTag::Earliest)),
            "pending" => return Ok(BlockNumber::Tag(BlockTag::Pending)),
            "safe" => return Ok(BlockNumber::Tag(BlockTag::Safe)),
            "finalized" => return Ok(BlockNumber::Tag(BlockTag::Finalized)),
            _ => {}
        }

        // Try parsing as hex number
        let s = s.strip_prefix("0x").unwrap_or(&s);
        u64::from_str_radix(s, 16)
            .map(BlockNumber::Number)
            .map_err(|_| serde::de::Error::custom("invalid block number"))
    }
}

/// Block tag
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockTag {
    Latest,
    Earliest,
    Pending,
    Safe,
    Finalized,
}

impl Default for BlockNumber {
    fn default() -> Self {
        Self::Tag(BlockTag::Latest)
    }
}

/// RPC block representation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcBlock {
    pub number: String,
    pub hash: String,
    pub parent_hash: String,
    pub state_root: String,
    pub transactions_root: String,
    pub receipts_root: String,
    pub miner: String,
    pub timestamp: String,
    pub gas_limit: String,
    pub gas_used: String,
    pub extra_data: String,
    // Fields required by ethers.js
    pub difficulty: String,
    pub total_difficulty: String,
    pub nonce: String,
    pub sha3_uncles: String,
    pub logs_bloom: String,
    pub size: String,
    pub base_fee_per_gas: Option<String>,
    // When full_tx=true, this contains full transaction objects
    // When full_tx=false, this contains transaction hashes as strings
    // ethers.js expects this field to always be named "transactions"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transactions: Option<serde_json::Value>,
}

impl RpcBlock {
    pub fn from_block(block: Block, block_hash: Hash, full_tx: bool) -> Self {
        let tx_hashes: Vec<Hash> = block
            .transactions
            .iter()
            .map(|tx| qfc_crypto::blake3_hash(&tx.to_bytes_without_signature()))
            .collect();

        // Empty bloom filter (256 bytes of zeros)
        let empty_bloom = "0x".to_string() + &"00".repeat(256);

        Self {
            number: format!("0x{:x}", block.number()),
            hash: block_hash.to_string(),
            parent_hash: block.parent_hash().to_string(),
            state_root: block.state_root().to_string(),
            transactions_root: block.header.transactions_root.to_string(),
            receipts_root: block.header.receipts_root.to_string(),
            miner: block.producer().to_string(),
            timestamp: format!("0x{:x}", block.timestamp()),
            gas_limit: format!("0x{:x}", block.gas_limit()),
            gas_used: format!("0x{:x}", block.gas_used()),
            extra_data: format!("0x{}", hex::encode(&block.header.extra_data)),
            // Fields for ethers.js compatibility (PoC doesn't use these)
            difficulty: "0x0".to_string(),
            total_difficulty: "0x0".to_string(),
            nonce: "0x0000000000000000".to_string(),
            sha3_uncles: "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347".to_string(),
            logs_bloom: empty_bloom,
            size: "0x0".to_string(),
            base_fee_per_gas: Some("0x0".to_string()),
            transactions: Some(if full_tx {
                serde_json::to_value(
                    block
                        .transactions
                        .iter()
                        .zip(tx_hashes.iter())
                        .enumerate()
                        .map(|(i, (tx, hash))| {
                            RpcTransaction::from_tx(tx.clone(), *hash, block_hash, block.number(), i as u32)
                        })
                        .collect::<Vec<_>>()
                ).unwrap_or(serde_json::Value::Array(vec![]))
            } else {
                serde_json::to_value(
                    tx_hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>()
                ).unwrap_or(serde_json::Value::Array(vec![]))
            }),
        }
    }
}

/// RPC transaction representation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTransaction {
    pub hash: String,
    pub nonce: String,
    pub block_hash: Option<String>,
    pub block_number: Option<String>,
    pub transaction_index: Option<String>,
    pub from: String,
    pub to: Option<String>,
    pub value: String,
    pub gas: String,
    pub gas_price: String,
    pub input: String,
}

impl RpcTransaction {
    pub fn from_tx(
        tx: Transaction,
        hash: Hash,
        block_hash: Hash,
        block_number: u64,
        tx_index: u32,
    ) -> Self {
        // Derive sender from public key (Ed25519)
        let sender = qfc_crypto::address_from_public_key(&tx.public_key);

        Self {
            hash: hash.to_string(),
            nonce: format!("0x{:x}", tx.nonce),
            block_hash: Some(block_hash.to_string()),
            block_number: Some(format!("0x{:x}", block_number)),
            transaction_index: Some(format!("0x{:x}", tx_index)),
            from: sender.to_string(),
            to: tx.to.map(|a| a.to_string()),
            value: format!("0x{:x}", tx.value.0),
            gas: format!("0x{:x}", tx.gas_limit),
            gas_price: format!("0x{:x}", tx.gas_price.0),
            input: format!("0x{}", hex::encode(&tx.data)),
        }
    }

    pub fn from_pending(tx: Transaction, hash: Hash, sender: Address) -> Self {
        Self {
            hash: hash.to_string(),
            nonce: format!("0x{:x}", tx.nonce),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            from: sender.to_string(),
            to: tx.to.map(|a| a.to_string()),
            value: format!("0x{:x}", tx.value.0),
            gas: format!("0x{:x}", tx.gas_limit),
            gas_price: format!("0x{:x}", tx.gas_price.0),
            input: format!("0x{}", hex::encode(&tx.data)),
        }
    }
}

/// RPC receipt representation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcReceipt {
    pub transaction_hash: String,
    pub transaction_index: String,
    pub block_hash: Option<String>,
    pub block_number: Option<String>,
    pub from: String,
    pub to: Option<String>,
    pub cumulative_gas_used: String,
    pub gas_used: String,
    pub contract_address: Option<String>,
    pub logs: Vec<RpcLog>,
    pub logs_bloom: String,
    pub status: String,
}

impl RpcReceipt {
    pub fn from_receipt(
        receipt: Receipt,
        from: Address,
        to: Option<Address>,
        block_hash: Option<Hash>,
        block_number: Option<u64>,
    ) -> Self {
        Self {
            transaction_hash: receipt.tx_hash.to_string(),
            transaction_index: format!("0x{:x}", receipt.tx_index),
            block_hash: block_hash.map(|h| h.to_string()),
            block_number: block_number.map(|n| format!("0x{:x}", n)),
            from: from.to_string(),
            to: to.map(|a| a.to_string()),
            cumulative_gas_used: format!("0x{:x}", receipt.cumulative_gas_used),
            gas_used: format!("0x{:x}", receipt.gas_used),
            contract_address: receipt.contract_address.map(|a| a.to_string()),
            logs: receipt.logs.iter().map(RpcLog::from_log).collect(),
            logs_bloom: format!("0x{}", hex::encode(&receipt.logs_bloom.0)),
            status: format!("0x{}", if receipt.is_success() { "1" } else { "0" }),
        }
    }
}

/// RPC log representation
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcLog {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub block_number: Option<String>,
    pub block_hash: Option<String>,
    pub transaction_hash: Option<String>,
    pub transaction_index: Option<String>,
    pub log_index: Option<String>,
}

impl RpcLog {
    pub fn from_log(log: &qfc_types::Log) -> Self {
        Self {
            address: log.address.to_string(),
            topics: log.topics.iter().map(|t| t.to_string()).collect(),
            data: format!("0x{}", hex::encode(&log.data)),
            block_number: None,
            block_hash: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
        }
    }
}

/// Call request
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRequest {
    pub from: Option<String>,
    pub to: Option<String>,
    pub gas: Option<String>,
    pub gas_price: Option<String>,
    pub value: Option<String>,
    pub data: Option<String>,
}
