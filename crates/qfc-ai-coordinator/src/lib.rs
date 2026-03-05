//! QFC AI Compute Coordinator
//!
//! Manages the task pool, miner assignment, and verification for
//! QFC v2.0's AI inference compute contribution.
//!
//! # Architecture
//!
//! - **Task Pool**: Queue of pending inference tasks (real or synthetic)
//! - **Assignment**: Match tasks to miners by capability (tier, memory, models)
//! - **Verification**: Spot-check re-execution of random proofs (~5%)
//! - **Registry**: Governance-approved model list

pub mod assignment;
pub mod governance;
pub mod proof_pool;
pub mod registry;
pub mod task_pool;
pub mod task_types;
pub mod verification;

pub use assignment::{MinerCapability, MinerRegistry};
pub use governance::{GovernanceError, ModelGovernance, ModelProposal, ProposalStatus};
pub use proof_pool::ProofPool;
pub use task_pool::TaskPool;
pub use verification::{
    should_spot_check, verify_basic, verify_spot_check, VerificationError, VerificationResult,
};
