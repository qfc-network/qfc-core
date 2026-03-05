//! Network constants and configuration

use crate::U256;

/// Chain ID for QFC Testnet
pub const TESTNET_CHAIN_ID: u64 = 9000;

/// Chain ID for QFC Mainnet
pub const MAINNET_CHAIN_ID: u64 = 9001;

/// Default chain ID (testnet)
pub const DEFAULT_CHAIN_ID: u64 = TESTNET_CHAIN_ID;

/// Block version
pub const BLOCK_VERSION: u32 = 1;

/// Maximum extra data size in bytes
pub const MAX_EXTRA_DATA_SIZE: usize = 32;

/// Minimum gas for a transaction
pub const MINIMUM_GAS: u64 = 21000;

/// Gas limit for transfer
pub const TRANSFER_GAS: u64 = 21000;

/// Gas limit for contract creation base cost
pub const CONTRACT_CREATE_GAS: u64 = 53000;

/// Gas per byte of data
pub const GAS_PER_BYTE: u64 = 16;

/// Gas per zero byte of data
pub const GAS_PER_ZERO_BYTE: u64 = 4;

/// Default block gas limit
pub const DEFAULT_BLOCK_GAS_LIMIT: u64 = 30_000_000;

/// Maximum transactions per block
pub const MAX_TRANSACTIONS_PER_BLOCK: usize = 10000;

/// Maximum inference proofs per block (v2.0)
pub const MAX_INFERENCE_PROOFS_PER_BLOCK: usize = 500;

/// Inference fee distribution: miner (70%)
pub const INFERENCE_FEE_MINER_PERCENT: u64 = 70;

/// Inference fee distribution: validators (10%)
pub const INFERENCE_FEE_VALIDATORS_PERCENT: u64 = 10;

/// Inference fee distribution: burn (20%)
pub const INFERENCE_FEE_BURN_PERCENT: u64 = 20;

/// Epoch duration in seconds
pub const EPOCH_DURATION_SECS: u64 = 10;

/// Blocks per epoch
pub const BLOCKS_PER_EPOCH: u64 = 3;

/// Block time in milliseconds (approximately)
pub const BLOCK_TIME_MS: u64 = 3333;

/// Minimum stake for validators (10,000 QFC)
pub const MIN_VALIDATOR_STAKE: u128 = 10_000_000_000_000_000_000_000; // 10^22 wei

/// Maximum number of active validators
pub const MAX_ACTIVE_VALIDATORS: usize = 1000;

/// Finality threshold (2/3 of total weight)
pub const FINALITY_THRESHOLD: f64 = 0.67;

/// Vote timeout in seconds
pub const VOTE_TIMEOUT_SECS: u64 = 5;

/// Block reward in wei (10 QFC)
pub const BLOCK_REWARD: u128 = 10_000_000_000_000_000_000; // 10^19 wei

/// Producer reward percentage (70%)
pub const PRODUCER_REWARD_PERCENT: u64 = 70;

/// Voters reward percentage (30%)
pub const VOTERS_REWARD_PERCENT: u64 = 30;

/// Fee distribution: producer (50%)
pub const FEE_PRODUCER_PERCENT: u64 = 50;

/// Fee distribution: voters (30%)
pub const FEE_VOTERS_PERCENT: u64 = 30;

/// Fee distribution: burn (20%)
pub const FEE_BURN_PERCENT: u64 = 20;

/// Contribution weight: stake (30%)
pub const WEIGHT_STAKE: f64 = 0.30;

/// Contribution weight: compute (20%)
pub const WEIGHT_COMPUTE: f64 = 0.20;

/// Contribution weight: uptime (15%)
pub const WEIGHT_UPTIME: f64 = 0.15;

/// Contribution weight: accuracy (15%)
pub const WEIGHT_ACCURACY: f64 = 0.15;

/// Contribution weight: network (10%)
pub const WEIGHT_NETWORK: f64 = 0.10;

/// Contribution weight: storage (5%)
pub const WEIGHT_STORAGE: f64 = 0.05;

/// Contribution weight: reputation (5%)
pub const WEIGHT_REPUTATION: f64 = 0.05;

/// Slash percentage for double signing
pub const SLASH_DOUBLE_SIGN_PERCENT: u64 = 50;

/// Slash percentage for invalid block
pub const SLASH_INVALID_BLOCK_PERCENT: u64 = 10;

/// Slash percentage for censorship
pub const SLASH_CENSORSHIP_PERCENT: u64 = 5;

/// Slash percentage for offline
pub const SLASH_OFFLINE_PERCENT: u64 = 1;

/// Slash percentage for false vote
pub const SLASH_FALSE_VOTE_PERCENT: u64 = 2;

/// One QFC in wei (10^18)
pub const ONE_QFC: u128 = 1_000_000_000_000_000_000;

/// One Gwei in wei (10^9)
pub const ONE_GWEI: u64 = 1_000_000_000;

// ============ Tokenomics ============

/// Initial total supply (1 billion QFC)
pub const INITIAL_SUPPLY: u128 = 1_000_000_000 * ONE_QFC;

/// Maximum supply cap (2 billion QFC)
pub const MAX_SUPPLY: u128 = 2_000_000_000 * ONE_QFC;

/// Block reward halving period in years
pub const HALVING_PERIOD_YEARS: u64 = 1;

/// Minimum block reward after all halvings (0.625 QFC)
pub const MIN_BLOCK_REWARD: u128 = 625_000_000_000_000_000;

/// Unstaking delay in seconds (7 days)
pub const UNSTAKE_DELAY_SECS: u64 = 7 * 24 * 60 * 60;

/// Minimum delegation amount (100 QFC)
pub const MIN_DELEGATION: u128 = 100 * ONE_QFC;

/// Maximum stake percentage per validator (1%)
pub const MAX_VALIDATOR_STAKE_PERCENT: u64 = 1;

/// Contract creator fee rebate percentage (5%)
pub const CONTRACT_CREATOR_FEE_PERCENT: u64 = 5;

/// Minimum gas price (1 Gwei)
pub const MIN_GAS_PRICE: u64 = ONE_GWEI;

/// Transaction pool size
pub const MEMPOOL_MAX_SIZE: usize = 10000;

/// Maximum pending transactions per account
pub const MEMPOOL_MAX_PER_ACCOUNT: usize = 64;

/// Transaction lifetime in seconds
pub const TX_LIFETIME_SECS: u64 = 3600; // 1 hour

/// Default block cache size in MB
pub const DEFAULT_BLOCK_CACHE_MB: usize = 512;

/// Default write buffer size in MB
pub const DEFAULT_WRITE_BUFFER_MB: usize = 64;

/// State pruning depth
pub const DEFAULT_PRUNING_DEPTH: u64 = 1000;

/// P2P default port
pub const DEFAULT_P2P_PORT: u16 = 30303;

/// RPC default HTTP port
pub const DEFAULT_RPC_HTTP_PORT: u16 = 8545;

/// RPC default WebSocket port
pub const DEFAULT_RPC_WS_PORT: u16 = 8546;

/// Maximum inbound peers
pub const DEFAULT_MAX_INBOUND_PEERS: u32 = 50;

/// Maximum outbound peers
pub const DEFAULT_MAX_OUTBOUND_PEERS: u32 = 25;

/// Get default block reward as U256
pub fn default_block_reward() -> U256 {
    U256::from_u128(BLOCK_REWARD)
}

/// Get minimum validator stake as U256
pub fn min_validator_stake() -> U256 {
    U256::from_u128(MIN_VALIDATOR_STAKE)
}

/// Get one QFC as U256
pub fn one_qfc() -> U256 {
    U256::from_u128(ONE_QFC)
}

/// Get initial supply as U256
pub fn initial_supply() -> U256 {
    U256::from_u128(INITIAL_SUPPLY)
}

/// Get max supply as U256
pub fn max_supply() -> U256 {
    U256::from_u128(MAX_SUPPLY)
}

/// Calculate block reward for a given year (0-indexed)
/// Reward halves each year until minimum is reached
pub fn block_reward_for_year(year: u64) -> U256 {
    let halvings = year.min(4); // Max 4 halvings
    let reward = BLOCK_REWARD >> halvings;
    let final_reward = reward.max(MIN_BLOCK_REWARD);
    U256::from_u128(final_reward)
}

/// Get minimum delegation as U256
pub fn min_delegation() -> U256 {
    U256::from_u128(MIN_DELEGATION)
}
