//! Agent Registration Resource Module
//!
//! Move-style linear resource for AI agent identity, discovery metadata, and staking.
//! Enforces stake minimums, reputation scoring, and kill-switch mechanics at the VM level.

use std::collections::HashMap;

use primitive_types::H160;
use qfc_types::Hash;

// ─── Error Codes ───

pub const E_INSUFFICIENT_STAKE: u64 = 200;
pub const E_NOT_OWNER: u64 = 201;
pub const E_AGENT_FROZEN: u64 = 202;
pub const E_ALREADY_REGISTERED: u64 = 203;
pub const E_INVALID_ENDPOINT: u64 = 204;
pub const E_COOLDOWN_NOT_ELAPSED: u64 = 205;
pub const E_BELOW_MIN_STAKE: u64 = 206;
pub const E_NOT_FOUND: u64 = 207;
pub const E_NOT_AUTHORIZED: u64 = 208;

// ─── Stake Tiers (8 decimals) ───

/// 100 QFC minimum for basic registration
pub const MIN_STAKE_BASIC: u64 = 100_000_000_00;
/// 1,000 QFC for verified tier
pub const MIN_STAKE_VERIFIED: u64 = 1_000_000_000_00;
/// 10,000 QFC for premium tier
pub const MIN_STAKE_PREMIUM: u64 = 10_000_000_000_00;
/// 7-day unstaking cooldown (seconds)
pub const UNSTAKE_COOLDOWN_SECS: u64 = 7 * 24 * 3600;

// ─── Reputation Constants ───

/// Starting reputation (50%)
pub const REPUTATION_INITIAL: u64 = 5000;
/// Maximum reputation
pub const REPUTATION_MAX: u64 = 10000;
/// Points gained per successful task
pub const REPUTATION_SUCCESS_DELTA: u64 = 10;
/// Points lost per failed task
pub const REPUTATION_FAILURE_DELTA: u64 = 50;
/// Points lost per slash
pub const REPUTATION_SLASH_DELTA: u64 = 500;

// ─── Stake Tier Enum ───

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StakeTier {
    Basic,
    Verified,
    Premium,
}

impl StakeTier {
    pub fn from_stake(stake: u64) -> Option<Self> {
        if stake >= MIN_STAKE_PREMIUM {
            Some(StakeTier::Premium)
        } else if stake >= MIN_STAKE_VERIFIED {
            Some(StakeTier::Verified)
        } else if stake >= MIN_STAKE_BASIC {
            Some(StakeTier::Basic)
        } else {
            None
        }
    }

    pub fn min_stake(&self) -> u64 {
        match self {
            StakeTier::Basic => MIN_STAKE_BASIC,
            StakeTier::Verified => MIN_STAKE_VERIFIED,
            StakeTier::Premium => MIN_STAKE_PREMIUM,
        }
    }
}

/// Helper: zero H160 address
pub fn zero_h160() -> H160 {
    H160::from([0u8; 20])
}

// ─── Events ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    Registered {
        agent_id: Hash,
        owner: H160,
        capabilities: Vec<String>,
        stake: u64,
    },
    Updated {
        agent_id: Hash,
        field: String,
    },
    Frozen {
        agent_id: Hash,
        frozen_by: H160,
        reason: String,
    },
    Unfrozen {
        agent_id: Hash,
        unfrozen_by: H160,
    },
    Slashed {
        agent_id: Hash,
        amount: u64,
        reason: String,
    },
    Revoked {
        agent_id: Hash,
        stake_returned: u64,
        stake_slashed: u64,
    },
    HeartbeatUpdated {
        agent_id: Hash,
        timestamp: u64,
    },
}

// ─── Core Resource ───

#[derive(Clone, Debug)]
pub struct AgentRegistration {
    pub id: Hash,
    pub owner: H160,
    pub protocol_digests: Vec<Hash>,
    pub capabilities: Vec<String>,
    pub endpoint: String,
    pub stake: u64,
    pub frozen: bool,
    pub reputation_score: u64,
    pub total_tasks_completed: u64,
    pub total_tasks_failed: u64,
    pub last_heartbeat: u64,
    pub created_at: u64,
    /// When revocation was initiated (0 = not revoking)
    pub revoke_initiated_at: u64,
}

// ─── Registry ───

pub struct AgentRegistry {
    agents: HashMap<Hash, AgentRegistration>,
    /// owner -> list of agent_ids
    owner_index: HashMap<H160, Vec<Hash>>,
    /// Emitted events (drained after each block)
    events: Vec<AgentEvent>,
    /// Current timestamp provider
    now: u64,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            owner_index: HashMap::new(),
            events: Vec::new(),
            now: 0,
        }
    }

    /// Set current timestamp
    pub fn set_timestamp(&mut self, now: u64) {
        self.now = now;
    }

    /// Drain and return accumulated events
    pub fn drain_events(&mut self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.events)
    }

    /// Register a new agent
    pub fn register(
        &mut self,
        agent_id: Hash,
        owner: H160,
        stake: u64,
        capabilities: Vec<String>,
        endpoint: String,
        protocol_digests: Vec<Hash>,
    ) -> Result<&AgentRegistration, u64> {
        if stake < MIN_STAKE_BASIC {
            return Err(E_INSUFFICIENT_STAKE);
        }
        if endpoint.is_empty() {
            return Err(E_INVALID_ENDPOINT);
        }
        if self.agents.contains_key(&agent_id) {
            return Err(E_ALREADY_REGISTERED);
        }

        let agent = AgentRegistration {
            id: agent_id,
            owner,
            protocol_digests,
            capabilities: capabilities.clone(),
            endpoint,
            stake,
            frozen: false,
            reputation_score: REPUTATION_INITIAL,
            total_tasks_completed: 0,
            total_tasks_failed: 0,
            last_heartbeat: self.now,
            created_at: self.now,
            revoke_initiated_at: 0,
        };

        self.owner_index.entry(owner).or_default().push(agent_id);
        self.agents.insert(agent_id, agent);

        self.events.push(AgentEvent::Registered {
            agent_id,
            owner,
            capabilities,
            stake,
        });

        Ok(self.agents.get(&agent_id).unwrap())
    }

    /// Get agent by ID
    pub fn get(&self, agent_id: &Hash) -> Option<&AgentRegistration> {
        self.agents.get(agent_id)
    }

    /// Get mutable agent by ID
    pub fn get_mut(&mut self, agent_id: &Hash) -> Option<&mut AgentRegistration> {
        self.agents.get_mut(agent_id)
    }

    /// List agents by owner
    pub fn list_by_owner(&self, owner: &H160) -> Vec<&AgentRegistration> {
        self.owner_index
            .get(owner)
            .map(|ids| ids.iter().filter_map(|id| self.agents.get(id)).collect())
            .unwrap_or_default()
    }

    /// Get all agents
    pub fn all_agents(&self) -> impl Iterator<Item = &AgentRegistration> {
        self.agents.values()
    }

    /// Update endpoint
    pub fn update_endpoint(
        &mut self,
        agent_id: &Hash,
        caller: H160,
        new_endpoint: String,
    ) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        if agent.frozen {
            return Err(E_AGENT_FROZEN);
        }
        agent.endpoint = new_endpoint;
        self.events.push(AgentEvent::Updated {
            agent_id: *agent_id,
            field: "endpoint".to_string(),
        });
        Ok(())
    }

    /// Update capabilities
    pub fn update_capabilities(
        &mut self,
        agent_id: &Hash,
        caller: H160,
        new_capabilities: Vec<String>,
    ) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        if agent.frozen {
            return Err(E_AGENT_FROZEN);
        }
        agent.capabilities = new_capabilities;
        self.events.push(AgentEvent::Updated {
            agent_id: *agent_id,
            field: "capabilities".to_string(),
        });
        Ok(())
    }

    /// Update protocol digests
    pub fn update_protocol_digests(
        &mut self,
        agent_id: &Hash,
        caller: H160,
        new_digests: Vec<Hash>,
    ) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        if agent.frozen {
            return Err(E_AGENT_FROZEN);
        }
        agent.protocol_digests = new_digests;
        self.events.push(AgentEvent::Updated {
            agent_id: *agent_id,
            field: "protocol_digests".to_string(),
        });
        Ok(())
    }

    /// Freeze agent (owner or authorized governance)
    pub fn freeze(
        &mut self,
        agent_id: &Hash,
        caller: H160,
        reason: String,
        is_governance: bool,
    ) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller && !is_governance {
            return Err(E_NOT_AUTHORIZED);
        }
        agent.frozen = true;
        self.events.push(AgentEvent::Frozen {
            agent_id: *agent_id,
            frozen_by: caller,
            reason,
        });
        Ok(())
    }

    /// Unfreeze agent (governance only for safety)
    pub fn unfreeze(
        &mut self,
        agent_id: &Hash,
        caller: H160,
        is_governance: bool,
    ) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        // Only governance can unfreeze (for safety — prevents compromised owner from unfreezing)
        if !is_governance && agent.owner != caller {
            return Err(E_NOT_AUTHORIZED);
        }
        agent.frozen = false;
        self.events.push(AgentEvent::Unfrozen {
            agent_id: *agent_id,
            unfrozen_by: caller,
        });
        Ok(())
    }

    /// Slash stake (called by verification system on misbehavior)
    pub fn slash(&mut self, agent_id: &Hash, amount: u64, reason: String) -> Result<u64, u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        let slash_amount = amount.min(agent.stake);
        agent.stake -= slash_amount;
        agent.reputation_score = agent
            .reputation_score
            .saturating_sub(REPUTATION_SLASH_DELTA);

        // Auto-freeze if reputation drops below threshold after consecutive failures
        if agent.total_tasks_failed >= 3 && agent.reputation_score < 2000 {
            agent.frozen = true;
        }

        self.events.push(AgentEvent::Slashed {
            agent_id: *agent_id,
            amount: slash_amount,
            reason,
        });
        Ok(slash_amount)
    }

    /// Heartbeat (agent signals it's alive)
    pub fn heartbeat(&mut self, agent_id: &Hash, caller: H160) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        agent.last_heartbeat = self.now;
        self.events.push(AgentEvent::HeartbeatUpdated {
            agent_id: *agent_id,
            timestamp: self.now,
        });
        Ok(())
    }

    /// Record task result (called by AI Coordinator)
    pub fn record_task_result(&mut self, agent_id: &Hash, success: bool) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if success {
            agent.total_tasks_completed += 1;
            agent.reputation_score =
                (agent.reputation_score + REPUTATION_SUCCESS_DELTA).min(REPUTATION_MAX);
        } else {
            agent.total_tasks_failed += 1;
            agent.reputation_score = agent
                .reputation_score
                .saturating_sub(REPUTATION_FAILURE_DELTA);
        }
        Ok(())
    }

    /// Initiate agent revocation (starts cooldown)
    pub fn initiate_revoke(&mut self, agent_id: &Hash, caller: H160) -> Result<(), u64> {
        let agent = self.agents.get_mut(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        agent.frozen = true;
        agent.revoke_initiated_at = self.now;
        Ok(())
    }

    /// Complete revocation after cooldown, returns stake to reclaim
    pub fn complete_revoke(&mut self, agent_id: &Hash, caller: H160) -> Result<u64, u64> {
        let agent = self.agents.get(agent_id).ok_or(E_NOT_FOUND)?;
        if agent.owner != caller {
            return Err(E_NOT_OWNER);
        }
        if agent.revoke_initiated_at == 0 {
            return Err(E_COOLDOWN_NOT_ELAPSED);
        }
        let elapsed = self.now.saturating_sub(agent.revoke_initiated_at);
        if elapsed < UNSTAKE_COOLDOWN_SECS {
            return Err(E_COOLDOWN_NOT_ELAPSED);
        }

        let stake_returned = agent.stake;
        let total_slashed = 0u64; // Any slashing already applied

        // Remove from indexes
        if let Some(ids) = self.owner_index.get_mut(&agent.owner) {
            ids.retain(|id| id != agent_id);
        }
        self.agents.remove(agent_id);

        self.events.push(AgentEvent::Revoked {
            agent_id: *agent_id,
            stake_returned,
            stake_slashed: total_slashed,
        });

        Ok(stake_returned)
    }

    /// Get stake tier for an agent
    pub fn stake_tier(&self, agent_id: &Hash) -> Option<StakeTier> {
        self.agents
            .get(agent_id)
            .and_then(|a| StakeTier::from_stake(a.stake))
    }

    /// Number of registered agents
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hash(seed: &[u8]) -> Hash {
        let mut h = [0u8; 32];
        let b = blake3::hash(seed);
        h.copy_from_slice(b.as_bytes());
        Hash::from(h)
    }

    fn owner_addr(n: u8) -> H160 {
        H160::from([n; 20])
    }

    fn make_registry(now: u64) -> AgentRegistry {
        let mut r = AgentRegistry::new();
        r.set_timestamp(now);
        r
    }

    fn register_basic(reg: &mut AgentRegistry, seed: &[u8], owner: H160) -> Hash {
        let id = test_hash(seed);
        reg.register(
            id,
            owner,
            MIN_STAKE_BASIC,
            vec!["text-generation".to_string()],
            "https://agent.example.com".to_string(),
            vec![],
        )
        .unwrap();
        id
    }

    #[test]
    fn test_register_basic() {
        let mut reg = make_registry(1000);
        let id = test_hash(b"agent1");
        let result = reg.register(
            id,
            owner_addr(1),
            MIN_STAKE_BASIC,
            vec!["text-generation".to_string()],
            "https://agent.example.com".to_string(),
            vec![test_hash(b"proto1")],
        );
        assert!(result.is_ok());
        let agent = result.unwrap();
        assert_eq!(agent.reputation_score, REPUTATION_INITIAL);
        assert_eq!(agent.stake, MIN_STAKE_BASIC);
        assert!(!agent.frozen);
        assert_eq!(reg.agent_count(), 1);
    }

    #[test]
    fn test_register_insufficient_stake() {
        let mut reg = make_registry(1000);
        let result = reg.register(
            test_hash(b"a"),
            owner_addr(1),
            MIN_STAKE_BASIC - 1,
            vec![],
            "https://x.com".to_string(),
            vec![],
        );
        assert_eq!(result.unwrap_err(), E_INSUFFICIENT_STAKE);
    }

    #[test]
    fn test_register_empty_endpoint() {
        let mut reg = make_registry(1000);
        let result = reg.register(
            test_hash(b"a"),
            owner_addr(1),
            MIN_STAKE_BASIC,
            vec![],
            "".to_string(),
            vec![],
        );
        assert_eq!(result.unwrap_err(), E_INVALID_ENDPOINT);
    }

    #[test]
    fn test_register_duplicate() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"dup", owner_addr(1));
        let result = reg.register(
            id,
            owner_addr(2),
            MIN_STAKE_BASIC,
            vec![],
            "https://x.com".to_string(),
            vec![],
        );
        assert_eq!(result.unwrap_err(), E_ALREADY_REGISTERED);
    }

    #[test]
    fn test_stake_tiers() {
        assert_eq!(StakeTier::from_stake(0), None);
        assert_eq!(StakeTier::from_stake(MIN_STAKE_BASIC - 1), None);
        assert_eq!(
            StakeTier::from_stake(MIN_STAKE_BASIC),
            Some(StakeTier::Basic)
        );
        assert_eq!(
            StakeTier::from_stake(MIN_STAKE_VERIFIED),
            Some(StakeTier::Verified)
        );
        assert_eq!(
            StakeTier::from_stake(MIN_STAKE_PREMIUM),
            Some(StakeTier::Premium)
        );
        assert_eq!(StakeTier::from_stake(u64::MAX), Some(StakeTier::Premium));
    }

    #[test]
    fn test_update_endpoint() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"ep", owner);

        assert!(reg
            .update_endpoint(&id, owner, "https://new.endpoint.com".to_string())
            .is_ok());
        assert_eq!(reg.get(&id).unwrap().endpoint, "https://new.endpoint.com");
    }

    #[test]
    fn test_update_endpoint_not_owner() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"ep2", owner_addr(1));
        assert_eq!(
            reg.update_endpoint(&id, owner_addr(2), "x".to_string())
                .unwrap_err(),
            E_NOT_OWNER
        );
    }

    #[test]
    fn test_update_endpoint_frozen() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"epf", owner);
        reg.freeze(&id, owner, "test".to_string(), false).unwrap();
        assert_eq!(
            reg.update_endpoint(&id, owner, "x".to_string())
                .unwrap_err(),
            E_AGENT_FROZEN
        );
    }

    #[test]
    fn test_update_capabilities() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"cap", owner);
        reg.update_capabilities(&id, owner, vec!["image-analysis".to_string()])
            .unwrap();
        assert_eq!(
            reg.get(&id).unwrap().capabilities,
            vec!["image-analysis".to_string()]
        );
    }

    #[test]
    fn test_freeze_by_owner() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"frz", owner);
        reg.freeze(&id, owner, "maintenance".to_string(), false)
            .unwrap();
        assert!(reg.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_freeze_by_governance() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"frzg", owner_addr(1));
        reg.freeze(&id, owner_addr(99), "misbehavior".to_string(), true)
            .unwrap();
        assert!(reg.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_freeze_not_authorized() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"frzn", owner_addr(1));
        assert_eq!(
            reg.freeze(&id, owner_addr(2), "x".to_string(), false)
                .unwrap_err(),
            E_NOT_AUTHORIZED
        );
    }

    #[test]
    fn test_unfreeze() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"unf", owner);
        reg.freeze(&id, owner, "test".to_string(), false).unwrap();
        assert!(reg.get(&id).unwrap().frozen);
        reg.unfreeze(&id, owner, false).unwrap();
        assert!(!reg.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_slash() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"slsh", owner_addr(1));
        let initial_stake = reg.get(&id).unwrap().stake;
        let slashed = reg.slash(&id, 1000, "bad result".to_string()).unwrap();
        assert_eq!(slashed, 1000);
        assert_eq!(reg.get(&id).unwrap().stake, initial_stake - 1000);
        assert_eq!(
            reg.get(&id).unwrap().reputation_score,
            REPUTATION_INITIAL - REPUTATION_SLASH_DELTA
        );
    }

    #[test]
    fn test_slash_exceeds_stake() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"slsh2", owner_addr(1));
        let stake = reg.get(&id).unwrap().stake;
        let slashed = reg.slash(&id, stake + 999, "x".to_string()).unwrap();
        assert_eq!(slashed, stake);
        assert_eq!(reg.get(&id).unwrap().stake, 0);
    }

    #[test]
    fn test_heartbeat() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"hb", owner);
        reg.set_timestamp(2000);
        reg.heartbeat(&id, owner).unwrap();
        assert_eq!(reg.get(&id).unwrap().last_heartbeat, 2000);
    }

    #[test]
    fn test_heartbeat_not_owner() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"hb2", owner_addr(1));
        assert_eq!(reg.heartbeat(&id, owner_addr(2)).unwrap_err(), E_NOT_OWNER);
    }

    #[test]
    fn test_record_task_success() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"ts", owner_addr(1));
        reg.record_task_result(&id, true).unwrap();
        let agent = reg.get(&id).unwrap();
        assert_eq!(agent.total_tasks_completed, 1);
        assert_eq!(
            agent.reputation_score,
            REPUTATION_INITIAL + REPUTATION_SUCCESS_DELTA
        );
    }

    #[test]
    fn test_record_task_failure() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"tf", owner_addr(1));
        reg.record_task_result(&id, false).unwrap();
        let agent = reg.get(&id).unwrap();
        assert_eq!(agent.total_tasks_failed, 1);
        assert_eq!(
            agent.reputation_score,
            REPUTATION_INITIAL - REPUTATION_FAILURE_DELTA
        );
    }

    #[test]
    fn test_reputation_caps() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"rc", owner_addr(1));
        // Max out reputation
        for _ in 0..1000 {
            reg.record_task_result(&id, true).unwrap();
        }
        assert_eq!(reg.get(&id).unwrap().reputation_score, REPUTATION_MAX);

        // Bottom out reputation
        let id2 = register_basic(&mut reg, b"rc2", owner_addr(1));
        for _ in 0..200 {
            reg.record_task_result(&id2, false).unwrap();
        }
        assert_eq!(reg.get(&id2).unwrap().reputation_score, 0);
    }

    #[test]
    fn test_auto_freeze_on_consecutive_failures() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"af", owner_addr(1));
        // Slash to drop reputation below 2000 with 3+ failures
        for _ in 0..3 {
            reg.record_task_result(&id, false).unwrap();
        }
        // Now slash should auto-freeze
        reg.slash(&id, 100, "bad".to_string()).unwrap();
        let agent = reg.get(&id).unwrap();
        // reputation: 5000 - 3*50 - 500 = 3350, still above 2000
        assert!(!agent.frozen); // not frozen yet

        // More failures to drop below 2000
        // Current: 4350 rep, need to get below 2500 (2000 + 500 slash delta)
        // Each failure = -50, need (4350 - 2500) / 50 = 37+ failures
        for _ in 0..40 {
            reg.record_task_result(&id, false).unwrap();
        }
        // reputation: 4350 - 40*50 = 2350, slash: 2350 - 500 = 1850 < 2000
        reg.slash(&id, 100, "bad".to_string()).unwrap();
        assert!(reg.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_revoke_with_cooldown() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"rev", owner);

        // Initiate revoke
        reg.initiate_revoke(&id, owner).unwrap();
        assert!(reg.get(&id).unwrap().frozen);

        // Too early
        reg.set_timestamp(1000 + UNSTAKE_COOLDOWN_SECS - 1);
        assert_eq!(
            reg.complete_revoke(&id, owner).unwrap_err(),
            E_COOLDOWN_NOT_ELAPSED
        );

        // After cooldown
        reg.set_timestamp(1000 + UNSTAKE_COOLDOWN_SECS);
        let returned = reg.complete_revoke(&id, owner).unwrap();
        assert_eq!(returned, MIN_STAKE_BASIC);
        assert!(reg.get(&id).is_none());
        assert_eq!(reg.agent_count(), 0);
    }

    #[test]
    fn test_revoke_not_owner() {
        let mut reg = make_registry(1000);
        let id = register_basic(&mut reg, b"rno", owner_addr(1));
        assert_eq!(
            reg.initiate_revoke(&id, owner_addr(2)).unwrap_err(),
            E_NOT_OWNER
        );
    }

    #[test]
    fn test_list_by_owner() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        register_basic(&mut reg, b"o1", owner);
        register_basic(&mut reg, b"o2", owner);
        register_basic(&mut reg, b"o3", owner_addr(2));

        assert_eq!(reg.list_by_owner(&owner).len(), 2);
        assert_eq!(reg.list_by_owner(&owner_addr(2)).len(), 1);
        assert_eq!(reg.list_by_owner(&owner_addr(99)).len(), 0);
    }

    #[test]
    fn test_events_emitted() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"ev", owner);
        reg.freeze(&id, owner, "test".to_string(), false).unwrap();

        let events = reg.drain_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AgentEvent::Registered { .. }));
        assert!(matches!(events[1], AgentEvent::Frozen { .. }));

        // Events drained
        assert!(reg.drain_events().is_empty());
    }

    #[test]
    fn test_update_protocol_digests() {
        let mut reg = make_registry(1000);
        let owner = owner_addr(1);
        let id = register_basic(&mut reg, b"pd", owner);
        let digest = test_hash(b"protocol-v2");
        reg.update_protocol_digests(&id, owner, vec![digest])
            .unwrap();
        assert_eq!(reg.get(&id).unwrap().protocol_digests, vec![digest]);
    }

    #[test]
    fn test_get_nonexistent() {
        let reg = make_registry(1000);
        assert!(reg.get(&test_hash(b"nope")).is_none());
    }

    #[test]
    fn test_stake_tier_for_agent() {
        let mut reg = make_registry(1000);
        let id = test_hash(b"tier");
        reg.register(
            id,
            owner_addr(1),
            MIN_STAKE_VERIFIED,
            vec![],
            "https://x.com".to_string(),
            vec![],
        )
        .unwrap();
        assert_eq!(reg.stake_tier(&id), Some(StakeTier::Verified));
    }
}
