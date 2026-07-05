//! RPC types for JSON serialization

use qfc_types::{Address, Block, EthTxMeta, Hash, Receipt, Transaction, U256};
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
    /// Render a block.
    ///
    /// `eth_metas` is parallel to `block.transactions`: entry `i` holds the
    /// render-only Ethereum metadata for tx `i` when it was submitted as an
    /// Ethereum tx (keccak hash / full `v` / envelope / EIP-1559 fees), or
    /// `None` for a native QFC tx. `logs_bloom` is the pre-computed block-level
    /// bloom (OR of all receipt blooms) as a `0x`-prefixed 256-byte hex string;
    /// `None` falls back to an all-zero bloom.
    pub fn from_block(
        block: Block,
        block_hash: Hash,
        full_tx: bool,
        eth_metas: &[Option<EthTxMeta>],
        logs_bloom: Option<String>,
    ) -> Self {
        // Internal BLAKE3 hashes (used as the fallback rendering + as the key
        // the Ethereum metadata was recorded against).
        let internal_hashes: Vec<Hash> = block
            .transactions
            .iter()
            .map(|tx| qfc_crypto::blake3_hash(&tx.to_bytes_without_signature()))
            .collect();

        // The hash shown to Ethereum tooling: the canonical keccak hash when a
        // mapping exists, else the internal hash (native QFC tx).
        let display_hashes: Vec<Hash> = internal_hashes
            .iter()
            .enumerate()
            .map(|(i, internal)| {
                eth_metas
                    .get(i)
                    .and_then(|m| m.as_ref())
                    .map(|m| m.eth_hash)
                    .unwrap_or(*internal)
            })
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
            // Header timestamp is milliseconds (consensus slot clock); the
            // Ethereum JSON-RPC convention (and every explorer/tool) expects
            // Unix seconds, matching the EVM `block.timestamp`. Convert here.
            timestamp: format!("0x{:x}", block.timestamp() / 1000),
            gas_limit: format!("0x{:x}", block.gas_limit()),
            gas_used: format!("0x{:x}", block.gas_used()),
            extra_data: format!("0x{}", hex::encode(&block.header.extra_data)),
            // Fields for ethers.js compatibility (PoC doesn't use these)
            difficulty: "0x0".to_string(),
            total_difficulty: "0x0".to_string(),
            nonce: "0x0000000000000000".to_string(),
            sha3_uncles: "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
                .to_string(),
            logs_bloom: logs_bloom.unwrap_or(empty_bloom),
            size: "0x0".to_string(),
            base_fee_per_gas: Some("0x0".to_string()),
            transactions: Some(if full_tx {
                serde_json::to_value(
                    block
                        .transactions
                        .iter()
                        .zip(internal_hashes.iter())
                        .enumerate()
                        .map(|(i, (tx, internal))| {
                            RpcTransaction::from_tx(
                                tx.clone(),
                                *internal,
                                block_hash,
                                block.number(),
                                i as u32,
                                eth_metas.get(i).and_then(|m| m.as_ref()),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Array(vec![]))
            } else {
                serde_json::to_value(
                    display_hashes
                        .iter()
                        .map(|h| h.to_string())
                        .collect::<Vec<_>>(),
                )
                .unwrap_or(serde_json::Value::Array(vec![]))
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
    // EIP-1559 fee fields (present only for type-0x2 transactions)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_fee_per_gas: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_priority_fee_per_gas: Option<String>,
    pub input: String,
    // Ethereum-compatible signature fields (required by ethers.js)
    pub r: String,
    pub s: String,
    pub v: String,
    #[serde(rename = "type")]
    pub tx_type: String,
    pub chain_id: String,
}

impl RpcTransaction {
    /// Extract sender, r, s, v from a transaction, handling both Ethereum and Ed25519 formats.
    ///
    /// The `v` returned here is a best-effort fallback. When render metadata is
    /// available (see [`EthTxMeta`]) it is overridden with the full-width `v`,
    /// because the Ethereum marker only stashes a single (truncated) byte in
    /// `public_key[1]`.
    fn extract_signature_fields(tx: &Transaction) -> (Address, String, String, String) {
        if tx.public_key.0[0] == 0xEE {
            // Ethereum transaction: r/s stored in signature, v in public_key[1], sender in public_key[2..22]
            let sender = Address::from_slice(&tx.public_key.0[2..22]).unwrap_or(Address::ZERO);
            let r = format!("0x{}", hex::encode(&tx.signature.0[..32]));
            let s = format!("0x{}", hex::encode(&tx.signature.0[32..]));
            let v = format!("0x{:x}", tx.public_key.0[1]);
            (sender, r, s, v)
        } else {
            // Ed25519 native: split 64-byte signature into r/s halves, use v=0x1b
            let sender = qfc_crypto::address_from_public_key(&tx.public_key);
            let r = format!("0x{}", hex::encode(&tx.signature.0[..32]));
            let s = format!("0x{}", hex::encode(&tx.signature.0[32..]));
            let v = "0x1b".to_string();
            (sender, r, s, v)
        }
    }

    /// Resolve the Ethereum-facing fields (hash, v, envelope type, fee fields)
    /// from optional render metadata, given the fallback internal hash / v and
    /// the tx's native gas price.
    fn eth_fields(
        eth_meta: Option<&EthTxMeta>,
        internal_hash: Hash,
        fallback_v: String,
        gas_price: U256,
    ) -> (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    ) {
        match eth_meta {
            Some(m) => {
                let tx_type = format!("0x{:x}", m.tx_type);
                let v = format!("0x{:x}", m.v);
                // EIP-1559 (type 0x2): surface the fee cap / tip and use the
                // max fee as the effective gasPrice (matching decode).
                let (gas_price_str, max_fee, max_prio) = if m.tx_type == 2 {
                    let max_fee = m
                        .max_fee_per_gas
                        .map(|f| format!("0x{:x}", f.0))
                        .unwrap_or_else(|| format!("0x{:x}", gas_price.0));
                    let max_prio = m
                        .max_priority_fee_per_gas
                        .map(|f| format!("0x{:x}", f.0))
                        .unwrap_or_else(|| "0x0".to_string());
                    (max_fee.clone(), Some(max_fee), Some(max_prio))
                } else {
                    (format!("0x{:x}", gas_price.0), None, None)
                };
                (
                    m.eth_hash.to_string(),
                    v,
                    tx_type,
                    gas_price_str,
                    max_fee,
                    max_prio,
                )
            }
            None => (
                // Native QFC tx: internal hash, legacy envelope (0x0).
                internal_hash.to_string(),
                fallback_v,
                "0x0".to_string(),
                format!("0x{:x}", gas_price.0),
                None,
                None,
            ),
        }
    }

    pub fn from_tx(
        tx: Transaction,
        internal_hash: Hash,
        block_hash: Hash,
        block_number: u64,
        tx_index: u32,
        eth_meta: Option<&EthTxMeta>,
    ) -> Self {
        let (sender, r, s, fallback_v) = Self::extract_signature_fields(&tx);
        let (hash, v, tx_type, gas_price, max_fee_per_gas, max_priority_fee_per_gas) =
            Self::eth_fields(eth_meta, internal_hash, fallback_v, tx.gas_price);
        let chain_id = format!("0x{:x}", tx.chain_id);

        Self {
            hash,
            nonce: format!("0x{:x}", tx.nonce),
            block_hash: Some(block_hash.to_string()),
            block_number: Some(format!("0x{:x}", block_number)),
            transaction_index: Some(format!("0x{:x}", tx_index)),
            from: sender.to_string(),
            to: tx.to.map(|a| a.to_string()),
            value: format!("0x{:x}", tx.value.0),
            gas: format!("0x{:x}", tx.gas_limit),
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            input: format!("0x{}", hex::encode(&tx.data)),
            r,
            s,
            v,
            tx_type,
            chain_id,
        }
    }

    pub fn from_pending(
        tx: Transaction,
        internal_hash: Hash,
        _sender: Address,
        eth_meta: Option<&EthTxMeta>,
    ) -> Self {
        let (sender, r, s, fallback_v) = Self::extract_signature_fields(&tx);
        let (hash, v, tx_type, gas_price, max_fee_per_gas, max_priority_fee_per_gas) =
            Self::eth_fields(eth_meta, internal_hash, fallback_v, tx.gas_price);
        let chain_id = format!("0x{:x}", tx.chain_id);

        Self {
            hash,
            nonce: format!("0x{:x}", tx.nonce),
            block_hash: None,
            block_number: None,
            transaction_index: None,
            from: sender.to_string(),
            to: tx.to.map(|a| a.to_string()),
            value: format!("0x{:x}", tx.value.0),
            gas: format!("0x{:x}", tx.gas_limit),
            gas_price,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            input: format!("0x{}", hex::encode(&tx.data)),
            r,
            s,
            v,
            tx_type,
            chain_id,
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
    /// Actual gas price paid (EIP-1559 tooling requires this field).
    pub effective_gas_price: String,
    /// EIP-2718 envelope type (0 legacy, 1 EIP-2930, 2 EIP-1559).
    #[serde(rename = "type")]
    pub tx_type: String,
}

impl RpcReceipt {
    pub fn from_receipt(
        receipt: Receipt,
        from: Address,
        to: Option<Address>,
        block_hash: Option<Hash>,
        block_number: Option<u64>,
        effective_gas_price: U256,
        tx_type: u8,
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
            logs: receipt
                .logs
                .iter()
                .enumerate()
                .map(|(i, log)| {
                    let mut rpc_log = RpcLog::from_log(log);
                    rpc_log.block_number = block_number.map(|n| format!("0x{:x}", n));
                    rpc_log.block_hash = block_hash.map(|h| h.to_string());
                    rpc_log.transaction_hash = Some(receipt.tx_hash.to_string());
                    rpc_log.transaction_index = Some(format!("0x{:x}", receipt.tx_index));
                    rpc_log.log_index = Some(format!("0x{:x}", i));
                    rpc_log
                })
                .collect(),
            logs_bloom: format!("0x{}", hex::encode(&receipt.logs_bloom.0)),
            status: format!("0x{}", if receipt.is_success() { "1" } else { "0" }),
            effective_gas_price: format!("0x{:x}", effective_gas_price.0),
            tx_type: format!("0x{:x}", tx_type),
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

    pub fn from_log_with_meta(
        log: &qfc_types::Log,
        block_number: u64,
        block_hash: Hash,
        tx_hash: Hash,
        tx_index: u32,
        log_index: u32,
    ) -> Self {
        Self {
            address: log.address.to_string(),
            topics: log.topics.iter().map(|t| t.to_string()).collect(),
            data: format!("0x{}", hex::encode(&log.data)),
            block_number: Some(format!("0x{:x}", block_number)),
            block_hash: Some(block_hash.to_string()),
            transaction_hash: Some(tx_hash.to_string()),
            transaction_index: Some(format!("0x{:x}", tx_index)),
            log_index: Some(format!("0x{:x}", log_index)),
        }
    }
}

/// Log filter for eth_getLogs
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogFilter {
    /// From block (default: latest)
    #[serde(default)]
    pub from_block: Option<BlockNumber>,
    /// To block (default: latest)
    #[serde(default)]
    pub to_block: Option<BlockNumber>,
    /// Contract address or list of addresses
    #[serde(default)]
    pub address: Option<AddressFilter>,
    /// Topic filters (position-based, up to 4 topics)
    #[serde(default)]
    pub topics: Option<Vec<Option<TopicFilter>>>,
    /// Block hash (overrides fromBlock/toBlock)
    #[serde(default)]
    pub block_hash: Option<String>,
}

/// Address filter: single address or array of addresses
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AddressFilter {
    Single(String),
    Multiple(Vec<String>),
}

/// Topic filter: single topic or array of topics (OR logic)
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TopicFilter {
    Single(String),
    Multiple(Vec<String>),
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

/// Fee history response for `eth_feeHistory` (EIP-1559 fee estimation).
///
/// QFC runs a flat gas model with a zero base fee (post-#139), so every
/// base-fee/reward entry is `0x0`; the field *shapes* are what MetaMask,
/// ethers and hardhat require to build a fee suggestion.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcFeeHistory {
    /// Lowest block number in the returned range.
    pub oldest_block: String,
    /// Base fee per gas for each block in `[oldestBlock, newestBlock + 1]`
    /// (length `blockCount + 1`).
    pub base_fee_per_gas: Vec<String>,
    /// Ratio of gas used to gas limit for each block (length `blockCount`).
    pub gas_used_ratio: Vec<f64>,
    /// Per-block priority-fee percentiles, present only when reward
    /// percentiles were requested (outer length `blockCount`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reward: Option<Vec<Vec<String>>>,
}

impl RpcFeeHistory {
    /// Build a well-formed fee history for `block_count` blocks ending at
    /// `newest_block` (clamped to the available range). All fee/reward values
    /// are `0x0` (QFC has a zero base fee and no priority fee), but the array
    /// lengths follow the spec so fee-estimation tooling works.
    pub fn build(block_count: u64, newest_block: u64, reward_percentiles: Option<&[f64]>) -> Self {
        // Clamp: at most one entry per block up to and including newest_block,
        // and always at least one.
        let max_count = newest_block.saturating_add(1);
        let count = block_count.clamp(1, max_count);
        let oldest = newest_block.saturating_sub(count.saturating_sub(1));

        // baseFeePerGas has count + 1 entries (includes the next block).
        let base_fee_per_gas = vec!["0x0".to_string(); (count + 1) as usize];
        let gas_used_ratio = vec![0.0f64; count as usize];
        let reward = reward_percentiles.map(|p| {
            let row = vec!["0x0".to_string(); p.len()];
            vec![row; count as usize]
        });

        Self {
            oldest_block: format!("0x{:x}", oldest),
            base_fee_per_gas,
            gas_used_ratio,
            reward,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_number_parse_hex() {
        let json = "\"0x10\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Number(n) => assert_eq!(n, 16),
            _ => panic!("Expected Number"),
        }
    }

    #[test]
    fn test_block_number_parse_latest() {
        let json = "\"latest\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Latest) => {}
            _ => panic!("Expected Latest tag"),
        }
    }

    #[test]
    fn test_block_number_parse_earliest() {
        let json = "\"earliest\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Earliest) => {}
            _ => panic!("Expected Earliest tag"),
        }
    }

    #[test]
    fn test_block_number_parse_pending() {
        let json = "\"pending\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Pending) => {}
            _ => panic!("Expected Pending tag"),
        }
    }

    #[test]
    fn test_block_number_parse_finalized() {
        let json = "\"finalized\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Finalized) => {}
            _ => panic!("Expected Finalized tag"),
        }
    }

    #[test]
    fn test_block_number_parse_safe() {
        let json = "\"safe\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Safe) => {}
            _ => panic!("Expected Safe tag"),
        }
    }

    #[test]
    fn test_block_number_parse_case_insensitive() {
        let json = "\"LATEST\"";
        let bn: BlockNumber = serde_json::from_str(json).unwrap();
        match bn {
            BlockNumber::Tag(BlockTag::Latest) => {}
            _ => panic!("Expected Latest tag"),
        }
    }

    #[test]
    fn test_block_number_serialize_number() {
        let bn = BlockNumber::Number(255);
        let json = serde_json::to_string(&bn).unwrap();
        assert_eq!(json, "\"0xff\"");
    }

    #[test]
    fn test_block_number_serialize_tag() {
        let bn = BlockNumber::Tag(BlockTag::Latest);
        let json = serde_json::to_string(&bn).unwrap();
        assert_eq!(json, "\"latest\"");
    }

    #[test]
    fn test_block_number_default() {
        let bn = BlockNumber::default();
        match bn {
            BlockNumber::Tag(BlockTag::Latest) => {}
            _ => panic!("Expected default to be Latest"),
        }
    }

    #[test]
    fn test_call_request_serialization() {
        let req = CallRequest {
            from: Some("0x1234".to_string()),
            to: Some("0x5678".to_string()),
            gas: Some("0x5208".to_string()),
            gas_price: Some("0x3b9aca00".to_string()),
            value: Some("0x0".to_string()),
            data: Some("0x".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        let parsed: CallRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.from, req.from);
        assert_eq!(parsed.to, req.to);
        assert_eq!(parsed.gas, req.gas);
    }

    #[test]
    fn test_rpc_log_from_log() {
        let log = qfc_types::Log {
            address: Address::default(),
            topics: vec![Hash::default()],
            data: vec![1, 2, 3, 4],
        };

        let rpc_log = RpcLog::from_log(&log);
        assert_eq!(rpc_log.data, "0x01020304");
        assert_eq!(rpc_log.topics.len(), 1);
    }

    #[test]
    fn test_rpc_receipt_status_success() {
        let receipt = Receipt {
            tx_hash: Hash::default(),
            tx_index: 0,
            cumulative_gas_used: 21000,
            gas_used: 21000,
            status: qfc_types::ReceiptStatus::Success,
            contract_address: None,
            logs: vec![],
            logs_bloom: qfc_types::Bloom::default(),
        };

        let rpc_receipt = RpcReceipt::from_receipt(
            receipt,
            Address::default(),
            Some(Address::default()),
            Some(Hash::default()),
            Some(100),
            U256::from_u64(1_000_000_000),
            2,
        );

        assert_eq!(rpc_receipt.status, "0x1");
        assert_eq!(rpc_receipt.gas_used, "0x5208");
        assert_eq!(rpc_receipt.effective_gas_price, "0x3b9aca00");
        assert_eq!(rpc_receipt.tx_type, "0x2");
    }

    #[test]
    fn test_rpc_receipt_status_failure() {
        let receipt = Receipt {
            tx_hash: Hash::default(),
            tx_index: 0,
            cumulative_gas_used: 21000,
            gas_used: 21000,
            status: qfc_types::ReceiptStatus::Failure("test error".to_string()),
            contract_address: None,
            logs: vec![],
            logs_bloom: qfc_types::Bloom::default(),
        };

        let rpc_receipt =
            RpcReceipt::from_receipt(receipt, Address::default(), None, None, None, U256::ZERO, 0);

        assert_eq!(rpc_receipt.status, "0x0");
        assert!(rpc_receipt.block_hash.is_none());
        assert!(rpc_receipt.block_number.is_none());
        assert_eq!(rpc_receipt.tx_type, "0x0");
    }

    fn dummy_eth_tx() -> Transaction {
        // A Transaction carrying the 0xEE Ethereum marker in public_key. The
        // marker's v byte is intentionally truncated (0x9b) to prove the meta
        // record's full-width v wins.
        let mut pk = [0u8; 32];
        pk[0] = 0xEE;
        pk[1] = 0x9b;
        Transaction {
            gas_price: U256::from_u64(1_000_000_000),
            public_key: qfc_types::PublicKey::new(pk),
            ..Default::default()
        }
    }

    #[test]
    fn test_tx_v_full_width_from_meta() {
        // Legacy chain-9000 v = 9000*2 + 35 + 0 = 18035 = 0x4673.
        let meta = EthTxMeta {
            eth_hash: Hash::new([0x11u8; 32]),
            v: 0x4673,
            tx_type: 0,
            max_priority_fee_per_gas: None,
            max_fee_per_gas: None,
        };
        let tx = RpcTransaction::from_tx(
            dummy_eth_tx(),
            Hash::new([0x22u8; 32]),
            Hash::default(),
            1,
            0,
            Some(&meta),
        );
        // Full-width v, not the truncated 0x9b byte.
        assert_eq!(tx.v, "0x4673");
        // Envelope type from meta (legacy), not QFC TransactionType.
        assert_eq!(tx.tx_type, "0x0");
        // Canonical keccak hash from meta, not the internal blake3 hash.
        assert_eq!(tx.hash, meta.eth_hash.to_string());
    }

    #[test]
    fn test_tx_type_eip1559_and_fees() {
        let meta = EthTxMeta {
            eth_hash: Hash::new([0xabu8; 32]),
            v: 1,
            tx_type: 2,
            max_priority_fee_per_gas: Some(U256::from_u64(0)),
            max_fee_per_gas: Some(U256::from_u64(2_000_000_000)),
        };
        let tx = RpcTransaction::from_tx(
            dummy_eth_tx(),
            Hash::new([0x22u8; 32]),
            Hash::default(),
            1,
            0,
            Some(&meta),
        );
        assert_eq!(tx.tx_type, "0x2");
        assert_eq!(tx.max_fee_per_gas.as_deref(), Some("0x77359400"));
        assert_eq!(tx.max_priority_fee_per_gas.as_deref(), Some("0x0"));
        // gasPrice mirrors maxFeePerGas for type-2 txs.
        assert_eq!(tx.gas_price, "0x77359400");
    }

    #[test]
    fn test_tx_native_fallback_no_meta() {
        // No meta: internal hash rendered, legacy envelope, no fee fields.
        let internal = Hash::new([0x33u8; 32]);
        let tx = RpcTransaction::from_tx(
            Transaction::default(),
            internal,
            Hash::default(),
            1,
            0,
            None,
        );
        assert_eq!(tx.hash, internal.to_string());
        assert_eq!(tx.tx_type, "0x0");
        assert!(tx.max_fee_per_gas.is_none());
        assert!(tx.max_priority_fee_per_gas.is_none());
    }

    #[test]
    fn test_block_renders_eth_hash_with_fallback() {
        // Two txs: index 0 has an eth mapping, index 1 does not.
        let block = Block {
            transactions: vec![Transaction::default(), Transaction::default()],
            ..Block::default()
        };

        let eth_hash = Hash::new([0x44u8; 32]);
        let metas = vec![
            Some(EthTxMeta {
                eth_hash,
                v: 0x4673,
                tx_type: 0,
                max_priority_fee_per_gas: None,
                max_fee_per_gas: None,
            }),
            None,
        ];

        let internal1 =
            qfc_crypto::blake3_hash(&block.transactions[1].to_bytes_without_signature());

        let rpc = RpcBlock::from_block(block, Hash::default(), false, &metas, None);
        let hashes: Vec<String> = serde_json::from_value(rpc.transactions.unwrap()).unwrap();
        // Mapped tx renders keccak; unmapped tx falls back to internal blake3.
        assert_eq!(hashes[0], eth_hash.to_string());
        assert_eq!(hashes[1], internal1.to_string());
    }

    #[test]
    fn test_block_logs_bloom_passthrough() {
        let block = Block::default();
        let bloom = format!("0x{}", "ff".repeat(256));
        let rpc = RpcBlock::from_block(block, Hash::default(), false, &[], Some(bloom.clone()));
        assert_eq!(rpc.logs_bloom, bloom);
        // base fee field must always be present for EIP-1559 tooling.
        assert_eq!(rpc.base_fee_per_gas.as_deref(), Some("0x0"));
    }

    #[test]
    fn test_fee_history_shape() {
        let fh = RpcFeeHistory::build(5, 100, Some(&[25.0, 50.0, 75.0]));
        assert_eq!(fh.oldest_block, "0x60"); // 100 - 5 + 1 = 96 = 0x60
        assert_eq!(fh.base_fee_per_gas.len(), 6); // blockCount + 1
        assert_eq!(fh.gas_used_ratio.len(), 5);
        let reward = fh.reward.unwrap();
        assert_eq!(reward.len(), 5);
        assert_eq!(reward[0].len(), 3);
        assert!(fh.base_fee_per_gas.iter().all(|b| b == "0x0"));
    }

    #[test]
    fn test_fee_history_clamps_to_range() {
        // Requesting more blocks than exist clamps to genesis.
        let fh = RpcFeeHistory::build(100, 3, None);
        assert_eq!(fh.oldest_block, "0x0");
        assert_eq!(fh.base_fee_per_gas.len(), 5); // 4 blocks (0..=3) + 1
        assert_eq!(fh.gas_used_ratio.len(), 4);
        assert!(fh.reward.is_none());
    }
}
