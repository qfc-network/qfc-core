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

/// Canonical block production interval in milliseconds.
///
/// SINGLE SOURCE for the consensus slot length: engine, producer, miner and
/// block validation must all consume this constant (never a per-node config
/// value — a per-node slot length is a silent consensus fork; see
/// docs/adr/0012-consensus-convergence-fixes.md).
pub const BLOCK_INTERVAL_MS: u64 = 5000;

/// Canonical epoch duration in milliseconds (single source, see
/// [`BLOCK_INTERVAL_MS`]). Must be a multiple of `BLOCK_INTERVAL_MS` so a
/// slot never straddles an epoch boundary.
pub const EPOCH_DURATION_MS: u64 = 10_000;

/// Epoch duration in seconds (derived from [`EPOCH_DURATION_MS`])
pub const EPOCH_DURATION_SECS: u64 = EPOCH_DURATION_MS / 1000;

/// Maximum allowed clock drift for block timestamps in milliseconds.
/// A block's timestamp may be at most this far in the validator's future;
/// it also bounds the slot-boundary tolerance in producer enforcement.
pub const MAX_TIMESTAMP_DRIFT_MS: u64 = 1500;

/// Blocks per epoch
pub const BLOCKS_PER_EPOCH: u64 = 3;

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

/// Producer reward percentage (60%)
pub const PRODUCER_REWARD_PERCENT: u64 = 60;

/// Voters reward percentage (25%)
pub const VOTERS_REWARD_PERCENT: u64 = 25;

/// Inference miners reward percentage (15%)
/// Distributed to miners who submitted proofs in this block, proportional to FLOPS.
/// If no inference proofs, this share goes back to producer + voters at 70/30 ratio.
pub const INFERENCE_MINERS_REWARD_PERCENT: u64 = 15;

/// Fee distribution: producer (47%)
pub const FEE_PRODUCER_PERCENT: u64 = 47;

/// Fee distribution: voters (28%)
pub const FEE_VOTERS_PERCENT: u64 = 28;

/// Fee distribution: burn (20%)
pub const FEE_BURN_PERCENT: u64 = 20;

/// Fee distribution: treasury (5%)
pub const FEE_TREASURY_PERCENT: u64 = 5;

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

// ============ Treasury ============

/// Treasury address — deterministic address derived from "qfc-treasury"
/// keccak256("qfc-treasury")[12..] = 0x5146...
/// This is a contract-like address with no private key.
pub const TREASURY_ADDRESS_BYTES: [u8; 20] = [
    0x51, 0x46, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
];

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

// ============ State Rent ============

/// Storage deposit per account creation (0.01 QFC — refundable on deletion)
pub const STORAGE_DEPOSIT_PER_ACCOUNT: u128 = ONE_QFC / 100;

/// Storage deposit per contract byte (0.00001 QFC per byte — refundable)
pub const STORAGE_DEPOSIT_PER_BYTE: u128 = ONE_QFC / 100_000;

/// Storage rent per slot per epoch (0.000001 QFC per storage slot per epoch)
pub const STORAGE_RENT_PER_SLOT_PER_EPOCH: u128 = ONE_QFC / 1_000_000;

/// Number of epochs of inactivity before an account becomes dormant
pub const DORMANT_THRESHOLD_EPOCHS: u64 = 262_800; // ~1 year at 10s epochs

/// Reactivation fee (0.1 QFC — non-refundable)
pub const REACTIVATION_FEE: u128 = ONE_QFC / 10;

/// Minimum storage deposit balance before account is flagged for rent collection
pub const MIN_STORAGE_DEPOSIT: u128 = ONE_QFC / 1_000; // 0.001 QFC
