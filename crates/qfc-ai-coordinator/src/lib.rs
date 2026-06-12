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
pub mod challenge;
pub mod cost;
pub mod governance;
pub mod ipfs;
pub mod param_governance;
pub mod proof_pool;
pub mod quota;
pub mod redundant;
pub mod registry;
pub mod router;
pub mod task_pool;
pub mod task_types;
pub mod training;
pub mod treasury;
pub mod verification;

pub use assignment::{MinerCapability, MinerRegistry};
pub use challenge::{
    ArbitrationManager, ArbitrationOutcome, ArbitrationPanel, ArbitrationVote, ChallengeGenerator,
    ChallengePenalty, ChallengeVerdict,
};
pub use cost::{CostEntry, CostMeter, CostReport, LoggingTreasuryHook, TreasuryHook};
pub use governance::{GovernanceError, ModelGovernance, ModelProposal, ProposalStatus};
pub use param_governance::{
    ParamGovernanceError, ParamProposalStatus, ParameterGovernance, ParameterKey, ParameterProposal,
};
pub use proof_pool::ProofPool;
pub use quota::{QuotaConfig, QuotaConfigError, QuotaError, TierQuota};
pub use task_pool::{AiQuotaMetrics, PublicTaskFilter, TaskPool};
pub use task_types::estimate_base_fee;
pub use training::{
    TrainingAssignment, TrainingError, TrainingJobSpec, TrainingJobStatus, TrainingPool,
};
pub use treasury::{SpendProposal, SpendStatus, Treasury, TreasuryError};
pub use verification::{
    should_spot_check, verify_basic, verify_spot_check, VerificationError, VerificationResult,
};
