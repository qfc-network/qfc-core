//! Model governance — on-chain proposal and voting for model registry

use std::collections::HashMap;

use qfc_inference::model::ModelInfo;
use qfc_types::{Address, Hash};

/// Voting threshold: >2/3 of active validators must approve
const SUPERMAJORITY_NUMERATOR: u64 = 2;
const SUPERMAJORITY_DENOMINATOR: u64 = 3;

/// Default voting period: 1 day in milliseconds
const DEFAULT_VOTING_PERIOD_MS: u64 = 86_400_000;

/// Proposal lifecycle status
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalStatus {
    Active,
    Passed,
    Rejected,
    Expired,
}

/// Errors during governance operations
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GovernanceError {
    #[error("Proposal not found: {0}")]
    ProposalNotFound(Hash),
    #[error("Proposal is not active")]
    ProposalNotActive,
    #[error("Voting period has expired")]
    VotingPeriodExpired,
    #[error("Voter {0} has already voted on this proposal")]
    AlreadyVoted(Address),
}

/// A proposal to add a model to the approved registry
#[derive(Clone, Debug)]
pub struct ModelProposal {
    pub proposal_id: Hash,
    pub proposer: Address,
    pub model_info: ModelInfo,
    pub votes_for: HashMap<Address, u64>,
    pub votes_against: HashMap<Address, u64>,
    pub status: ProposalStatus,
    pub created_at: u64,
    pub voting_deadline: u64,
}

/// Manages model governance proposals and voting
pub struct ModelGovernance {
    proposals: HashMap<Hash, ModelProposal>,
    voting_period_ms: u64,
    proposal_counter: u64,
}

impl ModelGovernance {
    pub fn new() -> Self {
        Self {
            proposals: HashMap::new(),
            voting_period_ms: DEFAULT_VOTING_PERIOD_MS,
            proposal_counter: 0,
        }
    }

    /// Create with custom voting period
    pub fn with_voting_period(mut self, period_ms: u64) -> Self {
        self.voting_period_ms = period_ms;
        self
    }

    /// Submit a new model proposal. Returns the proposal ID.
    pub fn propose_model(
        &mut self,
        proposer: Address,
        model_info: ModelInfo,
        now: u64,
    ) -> Hash {
        self.proposal_counter += 1;
        let mut data = Vec::with_capacity(28);
        data.extend_from_slice(proposer.as_bytes());
        data.extend_from_slice(&self.proposal_counter.to_le_bytes());
        let proposal_id = qfc_crypto::blake3_hash(&data);

        let proposal = ModelProposal {
            proposal_id,
            proposer,
            model_info,
            votes_for: HashMap::new(),
            votes_against: HashMap::new(),
            status: ProposalStatus::Active,
            created_at: now,
            voting_deadline: now + self.voting_period_ms,
        };

        self.proposals.insert(proposal_id, proposal);
        proposal_id
    }

    /// Cast a vote on a proposal.
    pub fn vote(
        &mut self,
        proposal_id: Hash,
        voter: Address,
        approve: bool,
        now: u64,
    ) -> Result<(), GovernanceError> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(GovernanceError::ProposalNotFound(proposal_id))?;

        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::ProposalNotActive);
        }

        if now > proposal.voting_deadline {
            return Err(GovernanceError::VotingPeriodExpired);
        }

        if proposal.votes_for.contains_key(&voter) || proposal.votes_against.contains_key(&voter) {
            return Err(GovernanceError::AlreadyVoted(voter));
        }

        if approve {
            proposal.votes_for.insert(voter, 1);
        } else {
            proposal.votes_against.insert(voter, 1);
        }

        Ok(())
    }

    /// Tally votes on all active proposals. Returns newly approved models.
    pub fn tally(&mut self, active_validator_count: u64, now: u64) -> Vec<ModelInfo> {
        let mut approved = Vec::new();

        for proposal in self.proposals.values_mut() {
            if proposal.status != ProposalStatus::Active {
                continue;
            }

            // Check if expired
            if now > proposal.voting_deadline {
                let votes_for = proposal.votes_for.len() as u64;
                if votes_for * SUPERMAJORITY_DENOMINATOR
                    > active_validator_count * SUPERMAJORITY_NUMERATOR
                {
                    proposal.status = ProposalStatus::Passed;
                    approved.push(proposal.model_info.clone());
                } else {
                    proposal.status = ProposalStatus::Expired;
                }
                continue;
            }

            // Check if supermajority reached early
            let votes_for = proposal.votes_for.len() as u64;
            if votes_for * SUPERMAJORITY_DENOMINATOR
                > active_validator_count * SUPERMAJORITY_NUMERATOR
            {
                proposal.status = ProposalStatus::Passed;
                approved.push(proposal.model_info.clone());
            }

            // Check if rejection is certain (remaining votes can't reach threshold)
            let votes_against = proposal.votes_against.len() as u64;
            let total_voted = votes_for + votes_against;
            let remaining = active_validator_count.saturating_sub(total_voted);
            if (votes_for + remaining) * SUPERMAJORITY_DENOMINATOR
                <= active_validator_count * SUPERMAJORITY_NUMERATOR
            {
                proposal.status = ProposalStatus::Rejected;
            }
        }

        approved
    }

    /// Get all active proposals
    pub fn active_proposals(&self) -> Vec<&ModelProposal> {
        self.proposals
            .values()
            .filter(|p| p.status == ProposalStatus::Active)
            .collect()
    }

    /// Get a specific proposal
    pub fn get_proposal(&self, proposal_id: &Hash) -> Option<&ModelProposal> {
        self.proposals.get(proposal_id)
    }

    /// Get all proposals
    pub fn all_proposals(&self) -> Vec<&ModelProposal> {
        self.proposals.values().collect()
    }
}

impl Default for ModelGovernance {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_inference::task::ModelId;
    use qfc_inference::GpuTier;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn test_model_info() -> ModelInfo {
        ModelInfo {
            id: ModelId::new("test-model", "v1.0"),
            description: "A test model".to_string(),
            min_memory_mb: 1024,
            min_tier: GpuTier::Warm,
            size_mb: 200,
            approved: false,
        }
    }

    #[test]
    fn test_propose_model() {
        let mut gov = ModelGovernance::new();
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);
        let proposal = gov.get_proposal(&id).unwrap();
        assert_eq!(proposal.status, ProposalStatus::Active);
        assert_eq!(proposal.proposer, proposer);
    }

    #[test]
    fn test_vote_and_tally_pass() {
        let mut gov = ModelGovernance::new().with_voting_period(10_000);
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);

        // 3 validators, need >2/3 = need >2, so 3 votes for
        gov.vote(id, test_address(1), true, 2000).unwrap();
        gov.vote(id, test_address(2), true, 3000).unwrap();
        gov.vote(id, test_address(3), true, 4000).unwrap();

        let approved = gov.tally(3, 5000);
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].id.name, "test-model");
        assert_eq!(gov.get_proposal(&id).unwrap().status, ProposalStatus::Passed);
    }

    #[test]
    fn test_vote_and_tally_reject() {
        let mut gov = ModelGovernance::new().with_voting_period(10_000);
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);

        // 3 validators, 2 vote against
        gov.vote(id, test_address(1), true, 2000).unwrap();
        gov.vote(id, test_address(2), false, 3000).unwrap();
        gov.vote(id, test_address(3), false, 4000).unwrap();

        let approved = gov.tally(3, 5000);
        assert!(approved.is_empty());
        assert_eq!(gov.get_proposal(&id).unwrap().status, ProposalStatus::Rejected);
    }

    #[test]
    fn test_tally_expired() {
        let mut gov = ModelGovernance::new().with_voting_period(5_000);
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);

        // Only 1 vote, then time expires
        gov.vote(id, test_address(1), true, 2000).unwrap();

        // Tally after deadline (1000 + 5000 = 6000)
        let approved = gov.tally(3, 7000);
        assert!(approved.is_empty());
        assert_eq!(gov.get_proposal(&id).unwrap().status, ProposalStatus::Expired);
    }

    #[test]
    fn test_duplicate_vote() {
        let mut gov = ModelGovernance::new().with_voting_period(10_000);
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);

        gov.vote(id, test_address(1), true, 2000).unwrap();
        let err = gov.vote(id, test_address(1), true, 3000).unwrap_err();
        assert!(matches!(err, GovernanceError::AlreadyVoted(_)));
    }

    #[test]
    fn test_vote_expired_proposal() {
        let mut gov = ModelGovernance::new().with_voting_period(1_000);
        let proposer = test_address(1);
        let id = gov.propose_model(proposer, test_model_info(), 1000);

        // Vote after deadline
        let err = gov.vote(id, test_address(1), true, 3000).unwrap_err();
        assert!(matches!(err, GovernanceError::VotingPeriodExpired));
    }

    #[test]
    fn test_active_proposals() {
        let mut gov = ModelGovernance::new();
        let p1 = gov.propose_model(test_address(1), test_model_info(), 1000);
        let _p2 = gov.propose_model(test_address(2), test_model_info(), 2000);

        assert_eq!(gov.active_proposals().len(), 2);

        // Pass first proposal
        gov.vote(p1, test_address(1), true, 3000).unwrap();
        gov.vote(p1, test_address(2), true, 3000).unwrap();
        gov.vote(p1, test_address(3), true, 3000).unwrap();
        gov.tally(3, 4000);

        assert_eq!(gov.active_proposals().len(), 1);
        assert_eq!(gov.all_proposals().len(), 2);
    }
}
