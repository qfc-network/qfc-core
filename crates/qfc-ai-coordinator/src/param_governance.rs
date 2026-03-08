//! Parameter governance — stake-weighted voting on protocol parameters
//!
//! Extends the governance system to allow validators to propose and vote
//! on changes to protocol parameters (block reward, gas limits, fee splits, etc.)
//! with stake-weighted voting and a timelock delay before execution.

use std::collections::HashMap;

use qfc_types::{Address, Hash};

/// Voting threshold: >2/3 of total stake must approve
const SUPERMAJORITY_NUMERATOR: u64 = 2;
const SUPERMAJORITY_DENOMINATOR: u64 = 3;

/// Default voting period: 3 days in milliseconds
const DEFAULT_VOTING_PERIOD_MS: u64 = 3 * 86_400_000;

/// Default timelock delay: 48 hours in milliseconds
const DEFAULT_TIMELOCK_MS: u64 = 48 * 3_600_000;

/// Minimum proposal stake: must hold at least 10,000 QFC to propose
const MIN_PROPOSAL_STAKE: u128 = 10_000_000_000_000_000_000_000; // 10^22 wei

/// Maximum parameter change per proposal (prevents extreme swings)
const MAX_CHANGE_PERCENT: u64 = 50;

/// Protocol parameters that can be governed
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ParameterKey {
    /// Block reward in wei
    BlockReward,
    /// Minimum validator stake in wei
    MinValidatorStake,
    /// Default block gas limit
    BlockGasLimit,
    /// Minimum gas price in wei
    MinGasPrice,
    /// Fee distribution: producer percent
    FeeProducerPercent,
    /// Fee distribution: voters percent
    FeeVotersPercent,
    /// Fee distribution: burn percent
    FeeBurnPercent,
    /// Block reward distribution: producer percent
    ProducerRewardPercent,
    /// Block reward distribution: voters percent
    VotersRewardPercent,
    /// Block reward distribution: inference miners percent
    InferenceMinersRewardPercent,
    /// Inference fee: miner percent
    InferenceFeeMinerPercent,
    /// Inference fee: validators percent
    InferenceFeeValidatorsPercent,
    /// Inference fee: burn percent
    InferenceFeeBurnPercent,
    /// Slash percent for double signing
    SlashDoubleSignPercent,
    /// Slash percent for going offline
    SlashOfflinePercent,
    /// Unstaking delay in seconds
    UnstakeDelaySecs,
    /// Minimum delegation amount in wei
    MinDelegation,
    /// Maximum transactions per block
    MaxTransactionsPerBlock,
}

impl std::fmt::Display for ParameterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlockReward => write!(f, "block_reward"),
            Self::MinValidatorStake => write!(f, "min_validator_stake"),
            Self::BlockGasLimit => write!(f, "block_gas_limit"),
            Self::MinGasPrice => write!(f, "min_gas_price"),
            Self::FeeProducerPercent => write!(f, "fee_producer_percent"),
            Self::FeeVotersPercent => write!(f, "fee_voters_percent"),
            Self::FeeBurnPercent => write!(f, "fee_burn_percent"),
            Self::ProducerRewardPercent => write!(f, "producer_reward_percent"),
            Self::VotersRewardPercent => write!(f, "voters_reward_percent"),
            Self::InferenceMinersRewardPercent => write!(f, "inference_miners_reward_percent"),
            Self::InferenceFeeMinerPercent => write!(f, "inference_fee_miner_percent"),
            Self::InferenceFeeValidatorsPercent => write!(f, "inference_fee_validators_percent"),
            Self::InferenceFeeBurnPercent => write!(f, "inference_fee_burn_percent"),
            Self::SlashDoubleSignPercent => write!(f, "slash_double_sign_percent"),
            Self::SlashOfflinePercent => write!(f, "slash_offline_percent"),
            Self::UnstakeDelaySecs => write!(f, "unstake_delay_secs"),
            Self::MinDelegation => write!(f, "min_delegation"),
            Self::MaxTransactionsPerBlock => write!(f, "max_transactions_per_block"),
        }
    }
}

/// Proposal lifecycle status
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamProposalStatus {
    /// Voting is ongoing
    Active,
    /// Passed vote, waiting for timelock to expire
    Queued { execute_after: u64 },
    /// Executed (parameter changed)
    Executed,
    /// Rejected by vote
    Rejected,
    /// Expired without reaching quorum
    Expired,
    /// Cancelled by proposer or emergency
    Cancelled,
}

/// Errors during parameter governance operations
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParamGovernanceError {
    #[error("Proposal not found: {0}")]
    ProposalNotFound(Hash),
    #[error("Proposal is not active")]
    ProposalNotActive,
    #[error("Voting period has expired")]
    VotingPeriodExpired,
    #[error("Voter {0} has already voted")]
    AlreadyVoted(Address),
    #[error("Insufficient stake: need {need}, have {have}")]
    InsufficientStake { need: String, have: String },
    #[error("Parameter change too large: {percent}% exceeds {max}% limit")]
    ChangeTooLarge { percent: u64, max: u64 },
    #[error("Timelock not expired: execute after {0}")]
    TimelockNotExpired(u64),
    #[error("Proposal not queued")]
    NotQueued,
    #[error("Fee percentages must sum to 100")]
    InvalidFeeSum,
    #[error("Governance is paused")]
    Paused,
}

/// A proposal to change a protocol parameter
#[derive(Clone, Debug)]
pub struct ParameterProposal {
    pub proposal_id: Hash,
    pub proposer: Address,
    pub parameter: ParameterKey,
    pub current_value: u128,
    pub proposed_value: u128,
    pub description: String,
    /// Voter address -> (approve, stake_weight)
    pub votes: HashMap<Address, (bool, u128)>,
    pub status: ParamProposalStatus,
    pub created_at: u64,
    pub voting_deadline: u64,
}

impl ParameterProposal {
    /// Total stake voting for
    pub fn stake_for(&self) -> u128 {
        self.votes
            .values()
            .filter(|(approve, _)| *approve)
            .map(|(_, stake)| stake)
            .sum()
    }

    /// Total stake voting against
    pub fn stake_against(&self) -> u128 {
        self.votes
            .values()
            .filter(|(approve, _)| !*approve)
            .map(|(_, stake)| stake)
            .sum()
    }

    /// Total stake that has voted
    pub fn total_voted_stake(&self) -> u128 {
        self.votes.values().map(|(_, stake)| stake).sum()
    }
}

/// Manages parameter governance proposals and voting
pub struct ParameterGovernance {
    proposals: HashMap<Hash, ParameterProposal>,
    /// Current override values (applied after execution)
    overrides: HashMap<ParameterKey, u128>,
    voting_period_ms: u64,
    timelock_ms: u64,
    proposal_counter: u64,
    /// Emergency pause flag
    paused: bool,
}

impl ParameterGovernance {
    pub fn new() -> Self {
        Self {
            proposals: HashMap::new(),
            overrides: HashMap::new(),
            voting_period_ms: DEFAULT_VOTING_PERIOD_MS,
            timelock_ms: DEFAULT_TIMELOCK_MS,
            proposal_counter: 0,
            paused: false,
        }
    }

    /// Create with custom voting period and timelock
    pub fn with_config(mut self, voting_period_ms: u64, timelock_ms: u64) -> Self {
        self.voting_period_ms = voting_period_ms;
        self.timelock_ms = timelock_ms;
        self
    }

    /// Submit a new parameter change proposal.
    /// `proposer_stake` is the proposer's current stake (must meet minimum).
    pub fn propose(
        &mut self,
        proposer: Address,
        parameter: ParameterKey,
        current_value: u128,
        proposed_value: u128,
        description: String,
        proposer_stake: u128,
        now: u64,
    ) -> Result<Hash, ParamGovernanceError> {
        if self.paused {
            return Err(ParamGovernanceError::Paused);
        }

        // Check minimum stake
        if proposer_stake < MIN_PROPOSAL_STAKE {
            return Err(ParamGovernanceError::InsufficientStake {
                need: MIN_PROPOSAL_STAKE.to_string(),
                have: proposer_stake.to_string(),
            });
        }

        // Check change magnitude
        if current_value > 0 {
            let change = if proposed_value > current_value {
                proposed_value - current_value
            } else {
                current_value - proposed_value
            };
            let percent = (change * 100) / current_value;
            if percent > MAX_CHANGE_PERCENT as u128 {
                return Err(ParamGovernanceError::ChangeTooLarge {
                    percent: percent as u64,
                    max: MAX_CHANGE_PERCENT,
                });
            }
        }

        self.proposal_counter += 1;
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(proposer.as_bytes());
        data.extend_from_slice(&self.proposal_counter.to_le_bytes());
        data.extend_from_slice(b"param");
        let proposal_id = qfc_crypto::blake3_hash(&data);

        let proposal = ParameterProposal {
            proposal_id,
            proposer,
            parameter,
            current_value,
            proposed_value,
            description,
            votes: HashMap::new(),
            status: ParamProposalStatus::Active,
            created_at: now,
            voting_deadline: now + self.voting_period_ms,
        };

        self.proposals.insert(proposal_id, proposal);
        Ok(proposal_id)
    }

    /// Cast a stake-weighted vote on a parameter proposal.
    pub fn vote(
        &mut self,
        proposal_id: Hash,
        voter: Address,
        approve: bool,
        voter_stake: u128,
        now: u64,
    ) -> Result<(), ParamGovernanceError> {
        if self.paused {
            return Err(ParamGovernanceError::Paused);
        }

        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(ParamGovernanceError::ProposalNotFound(proposal_id))?;

        if proposal.status != ParamProposalStatus::Active {
            return Err(ParamGovernanceError::ProposalNotActive);
        }

        if now > proposal.voting_deadline {
            return Err(ParamGovernanceError::VotingPeriodExpired);
        }

        if proposal.votes.contains_key(&voter) {
            return Err(ParamGovernanceError::AlreadyVoted(voter));
        }

        proposal.votes.insert(voter, (approve, voter_stake));
        Ok(())
    }

    /// Tally votes on all active proposals. Moves passing proposals to Queued.
    /// Returns list of proposal IDs that were newly queued.
    pub fn tally(&mut self, total_stake: u128, now: u64) -> Vec<Hash> {
        let mut queued = Vec::new();

        for proposal in self.proposals.values_mut() {
            if proposal.status != ParamProposalStatus::Active {
                continue;
            }

            let stake_for = proposal.stake_for();
            let stake_against = proposal.stake_against();
            let total_voted = stake_for + stake_against;

            let passed = stake_for * SUPERMAJORITY_DENOMINATOR as u128
                > total_stake * SUPERMAJORITY_NUMERATOR as u128;

            // Check if voting period expired
            if now > proposal.voting_deadline {
                if passed {
                    let execute_after = now + self.timelock_ms;
                    proposal.status = ParamProposalStatus::Queued { execute_after };
                    queued.push(proposal.proposal_id);
                } else {
                    proposal.status = ParamProposalStatus::Expired;
                }
                continue;
            }

            // Check if supermajority reached early
            if passed {
                let execute_after = now + self.timelock_ms;
                proposal.status = ParamProposalStatus::Queued { execute_after };
                queued.push(proposal.proposal_id);
                continue;
            }

            // Check if rejection is certain
            let remaining_stake = total_stake.saturating_sub(total_voted);
            if (stake_for + remaining_stake) * SUPERMAJORITY_DENOMINATOR as u128
                <= total_stake * SUPERMAJORITY_NUMERATOR as u128
            {
                proposal.status = ParamProposalStatus::Rejected;
            }
        }

        queued
    }

    /// Execute a queued proposal after timelock has expired.
    /// Returns the (parameter, new_value) pair on success.
    pub fn execute(
        &mut self,
        proposal_id: Hash,
        now: u64,
    ) -> Result<(ParameterKey, u128), ParamGovernanceError> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(ParamGovernanceError::ProposalNotFound(proposal_id))?;

        match &proposal.status {
            ParamProposalStatus::Queued { execute_after } => {
                if now < *execute_after {
                    return Err(ParamGovernanceError::TimelockNotExpired(*execute_after));
                }
            }
            _ => return Err(ParamGovernanceError::NotQueued),
        }

        let key = proposal.parameter.clone();
        let value = proposal.proposed_value;

        proposal.status = ParamProposalStatus::Executed;
        self.overrides.insert(key.clone(), value);

        Ok((key, value))
    }

    /// Execute all queued proposals whose timelock has expired.
    /// Returns list of (parameter, new_value) pairs that were executed.
    pub fn execute_mature(&mut self, now: u64) -> Vec<(ParameterKey, u128)> {
        let mature_ids: Vec<Hash> = self
            .proposals
            .values()
            .filter_map(|p| match &p.status {
                ParamProposalStatus::Queued { execute_after } if now >= *execute_after => {
                    Some(p.proposal_id)
                }
                _ => None,
            })
            .collect();

        let mut executed = Vec::new();
        for id in mature_ids {
            if let Ok(pair) = self.execute(id, now) {
                executed.push(pair);
            }
        }
        executed
    }

    /// Cancel a proposal (only proposer can cancel, and only while Active)
    pub fn cancel(
        &mut self,
        proposal_id: Hash,
        caller: Address,
    ) -> Result<(), ParamGovernanceError> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(ParamGovernanceError::ProposalNotFound(proposal_id))?;

        if proposal.proposer != caller {
            return Err(ParamGovernanceError::ProposalNotActive);
        }

        if proposal.status != ParamProposalStatus::Active {
            return Err(ParamGovernanceError::ProposalNotActive);
        }

        proposal.status = ParamProposalStatus::Cancelled;
        Ok(())
    }

    /// Emergency pause — stops all new proposals and votes
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Unpause governance
    pub fn unpause(&mut self) {
        self.paused = false;
    }

    /// Check if governance is paused
    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Get the current effective value for a parameter.
    /// Returns the override if one has been executed, otherwise None.
    pub fn get_override(&self, key: &ParameterKey) -> Option<u128> {
        self.overrides.get(key).copied()
    }

    /// Get all current overrides
    pub fn all_overrides(&self) -> &HashMap<ParameterKey, u128> {
        &self.overrides
    }

    /// Get a specific proposal
    pub fn get_proposal(&self, proposal_id: &Hash) -> Option<&ParameterProposal> {
        self.proposals.get(proposal_id)
    }

    /// Get all active proposals
    pub fn active_proposals(&self) -> Vec<&ParameterProposal> {
        self.proposals
            .values()
            .filter(|p| p.status == ParamProposalStatus::Active)
            .collect()
    }

    /// Get all queued proposals (waiting for timelock)
    pub fn queued_proposals(&self) -> Vec<&ParameterProposal> {
        self.proposals
            .values()
            .filter(|p| matches!(p.status, ParamProposalStatus::Queued { .. }))
            .collect()
    }

    /// Get all proposals
    pub fn all_proposals(&self) -> Vec<&ParameterProposal> {
        self.proposals.values().collect()
    }
}

impl Default for ParameterGovernance {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    const ENOUGH_STAKE: u128 = 20_000_000_000_000_000_000_000; // 20k QFC

    #[test]
    fn test_propose_parameter_change() {
        let mut gov = ParameterGovernance::new();
        let proposer = test_address(1);

        let id = gov
            .propose(
                proposer,
                ParameterKey::BlockReward,
                10_000_000_000_000_000_000, // 10 QFC
                8_000_000_000_000_000_000,  // 8 QFC (20% decrease)
                "Reduce block reward to slow inflation".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        let proposal = gov.get_proposal(&id).unwrap();
        assert_eq!(proposal.status, ParamProposalStatus::Active);
        assert_eq!(proposal.parameter, ParameterKey::BlockReward);
        assert_eq!(proposal.proposed_value, 8_000_000_000_000_000_000);
    }

    #[test]
    fn test_insufficient_stake_to_propose() {
        let mut gov = ParameterGovernance::new();
        let result = gov.propose(
            test_address(1),
            ParameterKey::BlockReward,
            10,
            8,
            "test".to_string(),
            1000, // way too little
            1000,
        );
        assert!(matches!(
            result,
            Err(ParamGovernanceError::InsufficientStake { .. })
        ));
    }

    #[test]
    fn test_change_too_large() {
        let mut gov = ParameterGovernance::new();
        let result = gov.propose(
            test_address(1),
            ParameterKey::BlockReward,
            100,
            10, // 90% decrease
            "test".to_string(),
            ENOUGH_STAKE,
            1000,
        );
        assert!(matches!(
            result,
            Err(ParamGovernanceError::ChangeTooLarge { .. })
        ));
    }

    #[test]
    fn test_stake_weighted_voting_and_pass() {
        let mut gov = ParameterGovernance::new().with_config(10_000, 5_000);
        let id = gov
            .propose(
                test_address(1),
                ParameterKey::MinGasPrice,
                1_000_000_000, // 1 Gwei
                1_500_000_000, // 1.5 Gwei
                "Increase min gas price".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        // Total stake: 100k. Need >66.7k to pass.
        let total_stake: u128 = 100_000_000_000_000_000_000_000; // 100k QFC

        // Validator with 40k stake votes for
        gov.vote(
            id,
            test_address(1),
            true,
            40_000_000_000_000_000_000_000,
            2000,
        )
        .unwrap();
        // Validator with 30k stake votes for (total for: 70k > 66.7k)
        gov.vote(
            id,
            test_address(2),
            true,
            30_000_000_000_000_000_000_000,
            3000,
        )
        .unwrap();

        let queued = gov.tally(total_stake, 4000);
        assert_eq!(queued.len(), 1);

        let proposal = gov.get_proposal(&id).unwrap();
        assert!(matches!(
            proposal.status,
            ParamProposalStatus::Queued { .. }
        ));
    }

    #[test]
    fn test_timelock_and_execute() {
        let mut gov = ParameterGovernance::new().with_config(10_000, 5_000);
        let id = gov
            .propose(
                test_address(1),
                ParameterKey::BlockGasLimit,
                30_000_000,
                35_000_000,
                "Increase gas limit".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        let total_stake: u128 = 100_000;
        gov.vote(id, test_address(1), true, 80_000, 2000).unwrap();

        gov.tally(total_stake, 3000);

        // Try to execute before timelock (execute_after = 3000 + 5000 = 8000)
        let err = gov.execute(id, 5000).unwrap_err();
        assert!(matches!(
            err,
            ParamGovernanceError::TimelockNotExpired(8000)
        ));

        // Execute after timelock
        let (key, value) = gov.execute(id, 9000).unwrap();
        assert_eq!(key, ParameterKey::BlockGasLimit);
        assert_eq!(value, 35_000_000);

        // Verify override is stored
        assert_eq!(
            gov.get_override(&ParameterKey::BlockGasLimit),
            Some(35_000_000)
        );
    }

    #[test]
    fn test_rejection() {
        let mut gov = ParameterGovernance::new().with_config(10_000, 5_000);
        let id = gov
            .propose(
                test_address(1),
                ParameterKey::BlockReward,
                100,
                80,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        let total_stake: u128 = 100;
        // 60 votes against, only 40 remaining — can't reach 67
        gov.vote(id, test_address(1), false, 60, 2000).unwrap();

        gov.tally(total_stake, 3000);
        assert_eq!(
            gov.get_proposal(&id).unwrap().status,
            ParamProposalStatus::Rejected
        );
    }

    #[test]
    fn test_expiry() {
        let mut gov = ParameterGovernance::new().with_config(5_000, 5_000);
        let id = gov
            .propose(
                test_address(1),
                ParameterKey::BlockReward,
                100,
                80,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        // No votes, tally after deadline
        gov.tally(100, 7000);
        assert_eq!(
            gov.get_proposal(&id).unwrap().status,
            ParamProposalStatus::Expired
        );
    }

    #[test]
    fn test_cancel_proposal() {
        let mut gov = ParameterGovernance::new();
        let proposer = test_address(1);
        let id = gov
            .propose(
                proposer,
                ParameterKey::BlockReward,
                100,
                80,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        // Only proposer can cancel
        let err = gov.cancel(id, test_address(2)).unwrap_err();
        assert!(matches!(err, ParamGovernanceError::ProposalNotActive));

        gov.cancel(id, proposer).unwrap();
        assert_eq!(
            gov.get_proposal(&id).unwrap().status,
            ParamProposalStatus::Cancelled
        );
    }

    #[test]
    fn test_emergency_pause() {
        let mut gov = ParameterGovernance::new();
        gov.pause();

        let result = gov.propose(
            test_address(1),
            ParameterKey::BlockReward,
            100,
            80,
            "test".to_string(),
            ENOUGH_STAKE,
            1000,
        );
        assert!(matches!(result, Err(ParamGovernanceError::Paused)));

        gov.unpause();
        assert!(gov
            .propose(
                test_address(1),
                ParameterKey::BlockReward,
                100,
                80,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .is_ok());
    }

    #[test]
    fn test_execute_mature() {
        let mut gov = ParameterGovernance::new().with_config(5_000, 2_000);

        // Create two proposals
        let id1 = gov
            .propose(
                test_address(1),
                ParameterKey::MinGasPrice,
                1_000_000_000,
                1_200_000_000,
                "test1".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();
        let id2 = gov
            .propose(
                test_address(1),
                ParameterKey::BlockGasLimit,
                30_000_000,
                32_000_000,
                "test2".to_string(),
                ENOUGH_STAKE,
                2000,
            )
            .unwrap();

        // Pass id1 first
        gov.vote(id1, test_address(1), true, 80, 2000).unwrap();
        gov.tally(100, 3000); // id1 queued at 3000, execute_after = 5000

        // Pass id2 later
        gov.vote(id2, test_address(2), true, 80, 4000).unwrap();
        gov.tally(100, 5000); // id2 queued at 5000, execute_after = 7000

        // At time 5500, only id1 is mature
        let executed = gov.execute_mature(5500);
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].0, ParameterKey::MinGasPrice);

        // At time 8000, id2 is also mature
        let executed = gov.execute_mature(8000);
        assert_eq!(executed.len(), 1);
        assert_eq!(executed[0].0, ParameterKey::BlockGasLimit);
    }

    #[test]
    fn test_duplicate_vote() {
        let mut gov = ParameterGovernance::new().with_config(10_000, 5_000);
        let id = gov
            .propose(
                test_address(1),
                ParameterKey::BlockReward,
                100,
                80,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
            )
            .unwrap();

        gov.vote(id, test_address(1), true, 50, 2000).unwrap();
        let err = gov.vote(id, test_address(1), false, 50, 3000).unwrap_err();
        assert!(matches!(err, ParamGovernanceError::AlreadyVoted(_)));
    }
}
