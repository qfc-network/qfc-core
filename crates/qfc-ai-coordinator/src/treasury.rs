//! Treasury management — community fund with governance-controlled spending
//!
//! A percentage of transaction fees (default 5%) is directed to the treasury
//! address. Spend proposals allow the community to allocate treasury funds
//! for ecosystem development, grants, etc. via stake-weighted governance.

use std::collections::HashMap;

use qfc_types::{Address, Hash};

/// Default voting period for treasury proposals: 5 days
const DEFAULT_VOTING_PERIOD_MS: u64 = 5 * 86_400_000;

/// Default timelock: 72 hours after vote passes
const DEFAULT_TIMELOCK_MS: u64 = 72 * 3_600_000;

/// Supermajority threshold: >2/3 of total stake
const SUPERMAJORITY_NUMERATOR: u64 = 2;
const SUPERMAJORITY_DENOMINATOR: u64 = 3;

/// Minimum stake to create a treasury spend proposal
const MIN_PROPOSAL_STAKE: u128 = 10_000_000_000_000_000_000_000; // 10k QFC

/// Maximum single spend as percentage of treasury balance
const MAX_SPEND_PERCENT: u64 = 25;

/// Spend proposal status
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpendStatus {
    Active,
    Queued { execute_after: u64 },
    Executed,
    Rejected,
    Expired,
    Cancelled,
}

/// Errors during treasury operations
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreasuryError {
    #[error("Proposal not found: {0}")]
    ProposalNotFound(Hash),
    #[error("Proposal is not active")]
    ProposalNotActive,
    #[error("Voting period expired")]
    VotingPeriodExpired,
    #[error("Already voted: {0}")]
    AlreadyVoted(Address),
    #[error("Insufficient stake: need {need}, have {have}")]
    InsufficientStake { need: String, have: String },
    #[error("Spend amount exceeds {max}% of treasury balance")]
    SpendTooLarge { max: u64 },
    #[error("Timelock not expired")]
    TimelockNotExpired,
    #[error("Proposal not queued")]
    NotQueued,
    #[error("Insufficient treasury balance")]
    InsufficientBalance,
}

/// A proposal to spend treasury funds
#[derive(Clone, Debug)]
pub struct SpendProposal {
    pub proposal_id: Hash,
    pub proposer: Address,
    pub recipient: Address,
    pub amount: u128,
    pub description: String,
    pub votes: HashMap<Address, (bool, u128)>,
    pub status: SpendStatus,
    pub created_at: u64,
    pub voting_deadline: u64,
}

impl SpendProposal {
    pub fn stake_for(&self) -> u128 {
        self.votes
            .values()
            .filter(|(approve, _)| *approve)
            .map(|(_, stake)| stake)
            .sum()
    }

    pub fn stake_against(&self) -> u128 {
        self.votes
            .values()
            .filter(|(approve, _)| !*approve)
            .map(|(_, stake)| stake)
            .sum()
    }
}

/// Manages treasury spend proposals
pub struct Treasury {
    proposals: HashMap<Hash, SpendProposal>,
    /// Cumulative amount already disbursed (for tracking)
    total_disbursed: u128,
    voting_period_ms: u64,
    timelock_ms: u64,
    proposal_counter: u64,
}

impl Treasury {
    pub fn new() -> Self {
        Self {
            proposals: HashMap::new(),
            total_disbursed: 0,
            voting_period_ms: DEFAULT_VOTING_PERIOD_MS,
            timelock_ms: DEFAULT_TIMELOCK_MS,
            proposal_counter: 0,
        }
    }

    pub fn with_config(mut self, voting_period_ms: u64, timelock_ms: u64) -> Self {
        self.voting_period_ms = voting_period_ms;
        self.timelock_ms = timelock_ms;
        self
    }

    /// Submit a spend proposal.
    /// `treasury_balance` is the current balance of the treasury address.
    pub fn propose_spend(
        &mut self,
        proposer: Address,
        recipient: Address,
        amount: u128,
        description: String,
        proposer_stake: u128,
        treasury_balance: u128,
        now: u64,
    ) -> Result<Hash, TreasuryError> {
        if proposer_stake < MIN_PROPOSAL_STAKE {
            return Err(TreasuryError::InsufficientStake {
                need: MIN_PROPOSAL_STAKE.to_string(),
                have: proposer_stake.to_string(),
            });
        }

        if amount > treasury_balance {
            return Err(TreasuryError::InsufficientBalance);
        }

        // Check spend doesn't exceed 25% of treasury
        if treasury_balance > 0 {
            let max_spend = treasury_balance * MAX_SPEND_PERCENT as u128 / 100;
            if amount > max_spend {
                return Err(TreasuryError::SpendTooLarge {
                    max: MAX_SPEND_PERCENT,
                });
            }
        }

        self.proposal_counter += 1;
        let mut data = Vec::with_capacity(36);
        data.extend_from_slice(proposer.as_bytes());
        data.extend_from_slice(&self.proposal_counter.to_le_bytes());
        data.extend_from_slice(b"treasury");
        let proposal_id = qfc_crypto::blake3_hash(&data);

        let proposal = SpendProposal {
            proposal_id,
            proposer,
            recipient,
            amount,
            description,
            votes: HashMap::new(),
            status: SpendStatus::Active,
            created_at: now,
            voting_deadline: now + self.voting_period_ms,
        };

        self.proposals.insert(proposal_id, proposal);
        Ok(proposal_id)
    }

    /// Cast a stake-weighted vote
    pub fn vote(
        &mut self,
        proposal_id: Hash,
        voter: Address,
        approve: bool,
        voter_stake: u128,
        now: u64,
    ) -> Result<(), TreasuryError> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(TreasuryError::ProposalNotFound(proposal_id))?;

        if proposal.status != SpendStatus::Active {
            return Err(TreasuryError::ProposalNotActive);
        }

        if now > proposal.voting_deadline {
            return Err(TreasuryError::VotingPeriodExpired);
        }

        if proposal.votes.contains_key(&voter) {
            return Err(TreasuryError::AlreadyVoted(voter));
        }

        proposal.votes.insert(voter, (approve, voter_stake));
        Ok(())
    }

    /// Tally active proposals. Returns IDs of newly queued proposals.
    pub fn tally(&mut self, total_stake: u128, now: u64) -> Vec<Hash> {
        let mut queued = Vec::new();

        for proposal in self.proposals.values_mut() {
            if proposal.status != SpendStatus::Active {
                continue;
            }

            let stake_for = proposal.stake_for();
            let stake_against = proposal.stake_against();
            let total_voted = stake_for + stake_against;

            let passed = stake_for * SUPERMAJORITY_DENOMINATOR as u128
                > total_stake * SUPERMAJORITY_NUMERATOR as u128;

            if now > proposal.voting_deadline {
                if passed {
                    let execute_after = now + self.timelock_ms;
                    proposal.status = SpendStatus::Queued { execute_after };
                    queued.push(proposal.proposal_id);
                } else {
                    proposal.status = SpendStatus::Expired;
                }
                continue;
            }

            if passed {
                let execute_after = now + self.timelock_ms;
                proposal.status = SpendStatus::Queued { execute_after };
                queued.push(proposal.proposal_id);
                continue;
            }

            let remaining = total_stake.saturating_sub(total_voted);
            if (stake_for + remaining) * SUPERMAJORITY_DENOMINATOR as u128
                <= total_stake * SUPERMAJORITY_NUMERATOR as u128
            {
                proposal.status = SpendStatus::Rejected;
            }
        }

        queued
    }

    /// Execute a queued spend proposal. Returns (recipient, amount).
    /// Caller is responsible for actually transferring funds from treasury address.
    pub fn execute(
        &mut self,
        proposal_id: Hash,
        now: u64,
    ) -> Result<(Address, u128), TreasuryError> {
        let proposal = self
            .proposals
            .get_mut(&proposal_id)
            .ok_or(TreasuryError::ProposalNotFound(proposal_id))?;

        match &proposal.status {
            SpendStatus::Queued { execute_after } => {
                if now < *execute_after {
                    return Err(TreasuryError::TimelockNotExpired);
                }
            }
            _ => return Err(TreasuryError::NotQueued),
        }

        let recipient = proposal.recipient;
        let amount = proposal.amount;

        proposal.status = SpendStatus::Executed;
        self.total_disbursed += amount;

        Ok((recipient, amount))
    }

    /// Get total amount disbursed from treasury
    pub fn total_disbursed(&self) -> u128 {
        self.total_disbursed
    }

    pub fn get_proposal(&self, id: &Hash) -> Option<&SpendProposal> {
        self.proposals.get(id)
    }

    pub fn active_proposals(&self) -> Vec<&SpendProposal> {
        self.proposals
            .values()
            .filter(|p| p.status == SpendStatus::Active)
            .collect()
    }

    pub fn all_proposals(&self) -> Vec<&SpendProposal> {
        self.proposals.values().collect()
    }
}

impl Default for Treasury {
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

    const ENOUGH_STAKE: u128 = 20_000_000_000_000_000_000_000;
    const TREASURY_BAL: u128 = 1_000_000_000_000_000_000_000; // 1000 QFC

    #[test]
    fn test_propose_spend() {
        let mut treasury = Treasury::new();
        let id = treasury
            .propose_spend(
                test_address(1),
                test_address(2),
                100_000_000_000_000_000_000, // 100 QFC
                "Fund developer grant".to_string(),
                ENOUGH_STAKE,
                TREASURY_BAL,
                1000,
            )
            .unwrap();

        let proposal = treasury.get_proposal(&id).unwrap();
        assert_eq!(proposal.status, SpendStatus::Active);
        assert_eq!(proposal.recipient, test_address(2));
    }

    #[test]
    fn test_spend_too_large() {
        let mut treasury = Treasury::new();
        // Try to spend 30% of treasury (max is 25%)
        let result = treasury.propose_spend(
            test_address(1),
            test_address(2),
            TREASURY_BAL * 30 / 100,
            "Too big".to_string(),
            ENOUGH_STAKE,
            TREASURY_BAL,
            1000,
        );
        assert!(matches!(result, Err(TreasuryError::SpendTooLarge { .. })));
    }

    #[test]
    fn test_spend_exceeds_balance() {
        let mut treasury = Treasury::new();
        let result = treasury.propose_spend(
            test_address(1),
            test_address(2),
            TREASURY_BAL + 1,
            "Over balance".to_string(),
            ENOUGH_STAKE,
            TREASURY_BAL,
            1000,
        );
        assert!(matches!(result, Err(TreasuryError::InsufficientBalance)));
    }

    #[test]
    fn test_vote_tally_execute() {
        let mut treasury = Treasury::new().with_config(10_000, 5_000);
        let id = treasury
            .propose_spend(
                test_address(1),
                test_address(9),
                100,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
                1000,
            )
            .unwrap();

        // Vote with majority stake
        treasury.vote(id, test_address(1), true, 80, 2000).unwrap();

        let queued = treasury.tally(100, 3000);
        assert_eq!(queued.len(), 1);

        // Execute after timelock (3000 + 5000 = 8000)
        let err = treasury.execute(id, 5000).unwrap_err();
        assert!(matches!(err, TreasuryError::TimelockNotExpired));

        let (recipient, amount) = treasury.execute(id, 9000).unwrap();
        assert_eq!(recipient, test_address(9));
        assert_eq!(amount, 100);
        assert_eq!(treasury.total_disbursed(), 100);
    }

    #[test]
    fn test_rejection() {
        let mut treasury = Treasury::new().with_config(10_000, 5_000);
        let id = treasury
            .propose_spend(
                test_address(1),
                test_address(2),
                100,
                "test".to_string(),
                ENOUGH_STAKE,
                1000,
                1000,
            )
            .unwrap();

        treasury.vote(id, test_address(1), false, 60, 2000).unwrap();
        treasury.tally(100, 3000);

        assert_eq!(
            treasury.get_proposal(&id).unwrap().status,
            SpendStatus::Rejected
        );
    }
}
