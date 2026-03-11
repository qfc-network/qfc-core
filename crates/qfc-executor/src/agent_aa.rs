//! Agent + Account Abstraction integration (Issue #83)
//!
//! Connects the AgentRegistry permission system with EIP-4337 EntryPoint
//! and Paymaster sponsorship. Allows agents to submit UserOperations that
//! are gas-sponsored by the agent's deposit via a Paymaster.

use crate::account_abstraction::{AAError, EntryPoint, UserOpPool, UserOperation};
use crate::agent::{AgentError, AgentGuard, AgentPermission, AgentRecord};
use qfc_types::{Address, Hash};
use tracing::{debug, info, warn};

/// Well-known address for the Agent Paymaster contract
pub const AGENT_PAYMASTER_ADDRESS: [u8; 20] = [
    0x43, 0x37, 0xAA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x02,
];

/// Error type for agent AA operations
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AgentAAError {
    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),

    #[error("AA error: {0}")]
    AccountAbstraction(#[from] AAError),

    #[error("Agent paymaster not funded for agent {0}")]
    PaymasterNotFunded(String),

    #[error("Invalid UserOp: {0}")]
    InvalidUserOp(String),
}

/// Bridges AgentGuard (permissions/limits) with EIP-4337 (UserOp/Paymaster)
pub struct AgentAA {
    /// Agent permission guard
    pub guard: AgentGuard,
    /// EIP-4337 EntryPoint
    pub entry_point: EntryPoint,
    /// UserOp pool for agent-submitted operations
    pub user_op_pool: UserOpPool,
    /// Agent Paymaster address
    paymaster_address: Address,
}

impl AgentAA {
    pub fn new(chain_id: u64) -> Self {
        let paymaster_address = Address::from_slice(&AGENT_PAYMASTER_ADDRESS).unwrap();
        let mut entry_point = EntryPoint::new(chain_id);

        // Pre-approve the agent paymaster
        entry_point.stake_paymaster(paymaster_address, 1_000_000, 86400, 0);

        Self {
            guard: AgentGuard::new(),
            entry_point,
            user_op_pool: UserOpPool::new(1000, 20),
            paymaster_address,
        }
    }

    /// Register an agent and set up its paymaster deposit
    pub fn register_agent(&mut self, agent: AgentRecord) {
        let deposit = agent.deposit;
        let agent_id = agent.agent_id.clone();
        self.guard.register_agent(agent);

        // Fund the paymaster with the agent's deposit for gas sponsorship
        if deposit > 0 {
            self.entry_point
                .add_paymaster_deposit(self.paymaster_address, deposit);
            debug!("Agent {} paymaster funded with {} wei", agent_id, deposit);
        }
    }

    /// Submit a UserOperation on behalf of an agent via session key.
    ///
    /// Flow:
    /// 1. Validate session key (TTL, nonce, permissions)
    /// 2. Check agent permission and spending limits
    /// 3. Attach paymaster data (agent deposit sponsors gas)
    /// 4. Validate UserOp via EntryPoint
    /// 5. Add to UserOp pool
    pub fn submit_agent_user_op(
        &mut self,
        mut user_op: UserOperation,
        session_key_address: &Address,
        permission: AgentPermission,
        spend_amount: u128,
        nonce: u64,
        now: u64,
    ) -> Result<Hash, AgentAAError> {
        // 1-2. Validate via AgentGuard
        let agent_id = self.guard.validate_agent_operation(
            session_key_address,
            permission,
            spend_amount,
            nonce,
            now,
        )?;

        // 3. Attach paymaster data if not already set
        if user_op.paymaster_and_data.is_empty() {
            let mut pm_data = self.paymaster_address.as_bytes().to_vec();
            // Append agent_id as extra paymaster context
            pm_data.extend_from_slice(agent_id.as_bytes());
            user_op.paymaster_and_data = pm_data;
        }

        // 4. Validate via EntryPoint
        self.entry_point.validate_user_op(&user_op, now)?;

        // 5. Add to pool
        let _chain_id = self.entry_point.address().as_bytes()[0] as u64; // simplified
        let op_hash = self
            .user_op_pool
            .add(user_op, self.entry_point.address(), 9000, now)?;

        // 6. Record spend and increment session key nonce
        self.guard.record_spend(&agent_id, spend_amount, now)?;
        self.guard
            .increment_session_key_nonce(session_key_address)?;

        info!(
            "Agent UserOp submitted: agent={}, op={}, spend={}",
            agent_id,
            hex::encode(&op_hash.as_bytes()[..8]),
            spend_amount
        );

        Ok(op_hash)
    }

    /// Execute pending agent UserOps (called by block producer during bundling)
    pub fn execute_pending_ops(&mut self, max_ops: usize) -> Vec<(Hash, Result<(), AgentAAError>)> {
        let pending: Vec<_> = self
            .user_op_pool
            .get_pending(max_ops)
            .into_iter()
            .map(|p| (p.hash, p.user_op.clone()))
            .collect();

        let mut results = Vec::new();

        for (hash, user_op) in pending {
            match self.entry_point.execute_user_op(&user_op) {
                Ok(receipt) => {
                    self.user_op_pool.remove(&hash);
                    debug!(
                        "Agent UserOp executed: {} success={}",
                        hex::encode(&hash.as_bytes()[..8]),
                        receipt.success
                    );
                    results.push((hash, Ok(())));
                }
                Err(e) => {
                    self.user_op_pool.remove(&hash);
                    warn!(
                        "Agent UserOp failed: {} error={}",
                        hex::encode(&hash.as_bytes()[..8]),
                        e
                    );
                    results.push((hash, Err(e.into())));
                }
            }
        }

        results
    }

    /// Top up an agent's paymaster deposit
    pub fn fund_agent_paymaster(
        &mut self,
        agent_id: &str,
        amount: u128,
    ) -> Result<(), AgentAAError> {
        let agent = self
            .guard
            .get_agent_mut(agent_id)
            .ok_or_else(|| AgentError::AgentNotFound(agent_id.to_string()))?;

        agent.deposit += amount;
        self.entry_point
            .add_paymaster_deposit(self.paymaster_address, amount);

        info!(
            "Agent {} paymaster topped up: +{} (deposit: {})",
            agent_id, amount, agent.deposit
        );

        Ok(())
    }

    /// Prune expired session keys and UserOps
    pub fn prune(&mut self, now: u64) -> (usize, usize) {
        let keys_pruned = self.guard.prune_session_keys(now);
        let ops_pruned = self.user_op_pool.prune_expired(now.saturating_sub(3600)); // 1h expiry
        (keys_pruned, ops_pruned)
    }
}

// ============ Tests ============

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRecord;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn make_agent(id: &str) -> AgentRecord {
        AgentRecord {
            agent_id: id.to_string(),
            owner: test_address(0x01),
            agent_address: test_address(0x02),
            permissions: vec![0xFF],                // all permissions
            daily_limit: 1_000_000_000_000_000_000, // 1 QFC
            max_per_tx: 100_000_000_000_000_000,    // 0.1 QFC
            deposit: 5_000_000_000_000_000_000,     // 5 QFC
            spent_today: 0,
            last_reset: 1_000_000,
            nonce: 0,
            active: true,
        }
    }

    fn make_user_op(sender: Address, nonce: u64) -> UserOperation {
        UserOperation {
            sender,
            nonce,
            init_code: vec![],
            call_data: vec![1, 2, 3, 4],
            call_gas_limit: 100_000,
            verification_gas_limit: 50_000,
            pre_verification_gas: 21_000,
            max_fee_per_gas: 1_000_000_000,
            max_priority_fee_per_gas: 100_000_000,
            paymaster_and_data: vec![],
            signature: vec![0xAA; 64],
        }
    }

    #[test]
    fn test_agent_aa_register_and_submit() {
        let mut aa = AgentAA::new(9000);
        aa.register_agent(make_agent("bot-1"));

        // Issue session key
        let sk_addr = test_address(0x55);
        aa.guard
            .issue_session_key(
                "bot-1",
                sk_addr,
                vec![0xFF],
                100_000_000_000_000_000,
                3600,
                1_000_000,
            )
            .unwrap();

        // Submit UserOp via session key
        let user_op = make_user_op(test_address(0x02), 0);
        let result = aa.submit_agent_user_op(
            user_op,
            &sk_addr,
            AgentPermission::ContractCall,
            1_000_000, // spend amount
            0,         // session key nonce
            1_000_100,
        );

        assert!(result.is_ok());
        assert_eq!(aa.user_op_pool.size(), 1);
    }

    #[test]
    fn test_agent_aa_permission_denied() {
        let mut aa = AgentAA::new(9000);
        let mut agent = make_agent("bot-1");
        agent.permissions = vec![AgentPermission::Transfer as u8]; // only transfer
        aa.register_agent(agent);

        let sk_addr = test_address(0x55);
        aa.guard
            .issue_session_key(
                "bot-1",
                sk_addr,
                vec![AgentPermission::Transfer as u8],
                100_000,
                3600,
                1_000_000,
            )
            .unwrap();

        let user_op = make_user_op(test_address(0x02), 0);
        let result = aa.submit_agent_user_op(
            user_op,
            &sk_addr,
            AgentPermission::Staking, // not allowed
            1_000,
            0,
            1_000_100,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_agent_aa_execute_pending() {
        let mut aa = AgentAA::new(9000);
        aa.register_agent(make_agent("bot-1"));

        let sk_addr = test_address(0x55);
        aa.guard
            .issue_session_key(
                "bot-1",
                sk_addr,
                vec![0xFF],
                100_000_000_000_000_000,
                3600,
                1_000_000,
            )
            .unwrap();

        let user_op = make_user_op(test_address(0x02), 0);
        aa.submit_agent_user_op(
            user_op,
            &sk_addr,
            AgentPermission::ContractCall,
            1_000_000,
            0,
            1_000_100,
        )
        .unwrap();

        assert_eq!(aa.user_op_pool.size(), 1);

        let results = aa.execute_pending_ops(10);
        assert_eq!(results.len(), 1);
        assert_eq!(aa.user_op_pool.size(), 0);
    }

    #[test]
    fn test_agent_aa_fund_paymaster() {
        let mut aa = AgentAA::new(9000);
        aa.register_agent(make_agent("bot-1"));

        let initial_deposit = aa.guard.get_agent("bot-1").unwrap().deposit;
        aa.fund_agent_paymaster("bot-1", 1_000_000).unwrap();

        let new_deposit = aa.guard.get_agent("bot-1").unwrap().deposit;
        assert_eq!(new_deposit, initial_deposit + 1_000_000);
    }

    #[test]
    fn test_agent_aa_prune() {
        let mut aa = AgentAA::new(9000);
        aa.register_agent(make_agent("bot-1"));

        let sk1 = test_address(0x55);
        let sk2 = test_address(0x66);
        aa.guard
            .issue_session_key("bot-1", sk1, vec![0xFF], 100_000, 100, 1_000_000) // expires at 1_000_100
            .unwrap();
        aa.guard
            .issue_session_key("bot-1", sk2, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        let (keys_pruned, _) = aa.prune(1_000_200);
        assert_eq!(keys_pruned, 1);
    }
}
