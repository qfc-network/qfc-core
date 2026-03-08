//! QFC Cross-Chain Bridge
//!
//! Lock-and-mint bridge between Ethereum and QFC.
//!
//! # Architecture
//!
//! - **Deposits (ETH→QFC)**: Users lock ETH/ERC20 on Ethereum. Bridge validators
//!   observe the lock event and collectively sign a mint request. Once threshold
//!   signatures are reached, wrapped tokens are minted on QFC.
//!
//! - **Withdrawals (QFC→ETH)**: Users burn wrapped tokens on QFC. Bridge validators
//!   observe the burn and collectively sign an unlock transaction on Ethereum.
//!
//! - **Security**: Multi-sig threshold (e.g., 5-of-7) prevents any single validator
//!   from minting or unlocking tokens unilaterally.

pub mod relayer;
pub mod types;
pub mod validator;

pub use relayer::{BridgeRelayer, RelayerConfig};
pub use types::{
    BridgeDeposit, BridgeError, BridgeStatus, BridgeWithdrawal, DepositStatus, WithdrawalStatus,
};
pub use validator::{BridgeValidatorSet, ValidatorSignature};
