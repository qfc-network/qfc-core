//! Agent permission enforcement and session key management
//!
//! Issue #81: Permission matrix enforcement with daily/per-tx limits
//! Issue #82: Session key lifecycle (issue/rotate/revoke) with TTL and nonce guard

use qfc_types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info};

// ============ Permission Definitions ============

/// Agent permission flags (bitmask)
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPermission {
    /// Can execute token transfers
    Transfer = 0x01,
    /// Can call smart contracts
    ContractCall = 0x02,
    /// Can submit inference tasks
    InferenceTask = 0x04,
    /// Can stake/unstake
    Staking = 0x08,
    /// Can delegate
    Delegation = 0x10,
    /// All permissions
    All = 0xFF,
}

impl AgentPermission {
    pub fn from_byte(b: u8) -> Vec<Self> {
        let mut perms = Vec::new();
        if b & 0x01 != 0 {
            perms.push(Self::Transfer);
        }
        if b & 0x02 != 0 {
            perms.push(Self::ContractCall);
        }
        if b & 0x04 != 0 {
            perms.push(Self::InferenceTask);
        }
        if b & 0x08 != 0 {
            perms.push(Self::Staking);
        }
        if b & 0x10 != 0 {
            perms.push(Self::Delegation);
        }
        perms
    }

    pub fn has_permission(permissions: &[u8], perm: AgentPermission) -> bool {
        if permissions.is_empty() {
            return false;
        }
        // First byte is the permission bitmask
        permissions[0] & (perm as u8) != 0
    }
}

// ============ Agent Info ============

/// On-chain agent record (mirrors contract state)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentRecord {
    pub agent_id: String,
    pub owner: Address,
    pub agent_address: Address,
    pub permissions: Vec<u8>,
    pub daily_limit: u128,
    pub max_per_tx: u128,
    pub deposit: u128,
    pub spent_today: u128,
    pub last_reset: u64,
    pub nonce: u64,
    pub active: bool,
}

// ============ Session Key ============

/// A session key bound to an agent with TTL and nonce tracking
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionKey {
    /// The session key address (derived from public key)
    pub key_address: Address,
    /// Agent ID this key is bound to
    pub agent_id: String,
    /// Owner who issued this key
    pub owner: Address,
    /// Permissions granted to this session key (subset of agent permissions)
    pub permissions: Vec<u8>,
    /// Per-tx spending limit (may be lower than agent's max_per_tx)
    pub max_per_tx: u128,
    /// Unix timestamp when this key was issued
    pub issued_at: u64,
    /// Unix timestamp when this key expires (TTL)
    pub expires_at: u64,
    /// Monotonic nonce for replay protection
    pub nonce: u64,
    /// Whether this key has been revoked
    pub revoked: bool,
}

impl SessionKey {
    /// Check if this session key is currently valid
    pub fn is_valid(&self, now: u64) -> bool {
        !self.revoked && now < self.expires_at && now >= self.issued_at
    }
}

// ============ Errors ============

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum AgentError {
    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    #[error("Agent not active: {0}")]
    AgentNotActive(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Daily limit exceeded: spent {spent}, limit {limit}, attempted {amount}")]
    DailyLimitExceeded {
        spent: String,
        limit: String,
        amount: String,
    },

    #[error("Per-tx limit exceeded: limit {limit}, attempted {amount}")]
    PerTxLimitExceeded { limit: String, amount: String },

    #[error("Insufficient agent deposit: need {need}, have {have}")]
    InsufficientDeposit { need: String, have: String },

    #[error("Session key expired at {expires_at}, now {now}")]
    SessionKeyExpired { expires_at: u64, now: u64 },

    #[error("Session key revoked: {0}")]
    SessionKeyRevoked(Address),

    #[error("Session key not found: {0}")]
    SessionKeyNotFound(Address),

    #[error("Session key nonce mismatch: expected {expected}, got {got}")]
    SessionKeyNonceMismatch { expected: u64, got: u64 },

    #[error("Invalid session key TTL: max {max_ttl}s, requested {requested}s")]
    InvalidTTL { max_ttl: u64, requested: u64 },

    #[error("Max session keys reached for agent {0}: limit {1}")]
    MaxSessionKeysReached(String, usize),
}

// ============ Agent Permission Guard ============

/// Duration of one day in seconds (for daily limit reset)
const DAY_SECS: u64 = 86_400;

/// Maximum TTL for a session key (7 days)
pub const MAX_SESSION_KEY_TTL: u64 = 7 * DAY_SECS;

/// Maximum number of active session keys per agent
pub const MAX_SESSION_KEYS_PER_AGENT: usize = 10;

/// Enforces agent permission checks and spending limits
pub struct AgentGuard {
    /// Active agents indexed by agent_id
    agents: HashMap<String, AgentRecord>,
    /// Session keys indexed by key_address
    session_keys: HashMap<Address, SessionKey>,
    /// Session keys indexed by agent_id (for listing/cleanup)
    agent_session_keys: HashMap<String, Vec<Address>>,
}

impl AgentGuard {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            session_keys: HashMap::new(),
            agent_session_keys: HashMap::new(),
        }
    }

    /// Register or update an agent record
    pub fn register_agent(&mut self, agent: AgentRecord) {
        let agent_id = agent.agent_id.clone();
        self.agents.insert(agent_id, agent);
    }

    /// Get an agent record
    pub fn get_agent(&self, agent_id: &str) -> Option<&AgentRecord> {
        self.agents.get(agent_id)
    }

    /// Get a mutable agent record
    pub fn get_agent_mut(&mut self, agent_id: &str) -> Option<&mut AgentRecord> {
        self.agents.get_mut(agent_id)
    }

    /// Check if an operation is permitted and within limits.
    /// Returns Ok(()) if allowed, or an appropriate error.
    pub fn check_permission(
        &mut self,
        agent_id: &str,
        permission: AgentPermission,
        amount: u128,
        now: u64,
    ) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentError::AgentNotFound(agent_id.to_string()))?;

        // 1. Check active
        if !agent.active {
            return Err(AgentError::AgentNotActive(agent_id.to_string()));
        }

        // 2. Check permission bitmask
        if !AgentPermission::has_permission(&agent.permissions, permission) {
            return Err(AgentError::PermissionDenied(format!(
                "Agent {} lacks permission {:?}",
                agent_id, permission
            )));
        }

        // 3. Check per-tx limit
        if agent.max_per_tx > 0 && amount > agent.max_per_tx {
            return Err(AgentError::PerTxLimitExceeded {
                limit: agent.max_per_tx.to_string(),
                amount: amount.to_string(),
            });
        }

        // 4. Reset daily counter if new day
        if now >= agent.last_reset + DAY_SECS {
            agent.spent_today = 0;
            agent.last_reset = now - (now % DAY_SECS); // Align to day boundary
        }

        // 5. Check daily limit
        if agent.daily_limit > 0 {
            let new_spent = agent.spent_today.saturating_add(amount);
            if new_spent > agent.daily_limit {
                return Err(AgentError::DailyLimitExceeded {
                    spent: agent.spent_today.to_string(),
                    limit: agent.daily_limit.to_string(),
                    amount: amount.to_string(),
                });
            }
        }

        // 6. Check deposit sufficiency
        if amount > 0 && agent.deposit < amount {
            return Err(AgentError::InsufficientDeposit {
                need: amount.to_string(),
                have: agent.deposit.to_string(),
            });
        }

        Ok(())
    }

    /// Record a successful spend against an agent's daily limit and deposit.
    pub fn record_spend(
        &mut self,
        agent_id: &str,
        amount: u128,
        now: u64,
    ) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentError::AgentNotFound(agent_id.to_string()))?;

        // Reset daily counter if new day
        if now >= agent.last_reset + DAY_SECS {
            agent.spent_today = 0;
            agent.last_reset = now - (now % DAY_SECS);
        }

        agent.spent_today = agent.spent_today.saturating_add(amount);
        agent.deposit = agent.deposit.saturating_sub(amount);
        agent.nonce += 1;

        debug!(
            "Agent {} spent {}: daily {}/{}, deposit remaining {}",
            agent_id, amount, agent.spent_today, agent.daily_limit, agent.deposit
        );

        Ok(())
    }

    /// Deactivate an agent (revoke)
    pub fn revoke_agent(&mut self, agent_id: &str) -> Result<(), AgentError> {
        let agent = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentError::AgentNotFound(agent_id.to_string()))?;

        agent.active = false;

        // Revoke all session keys for this agent
        if let Some(keys) = self.agent_session_keys.get(agent_id) {
            for key_addr in keys {
                if let Some(sk) = self.session_keys.get_mut(key_addr) {
                    sk.revoked = true;
                }
            }
        }

        info!("Agent revoked: {}", agent_id);
        Ok(())
    }

    // ============ Session Key Management (Issue #82) ============

    /// Issue a new session key for an agent
    pub fn issue_session_key(
        &mut self,
        agent_id: &str,
        key_address: Address,
        permissions: Vec<u8>,
        max_per_tx: u128,
        ttl_secs: u64,
        now: u64,
    ) -> Result<SessionKey, AgentError> {
        // Validate agent exists and is active
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| AgentError::AgentNotFound(agent_id.to_string()))?;

        if !agent.active {
            return Err(AgentError::AgentNotActive(agent_id.to_string()));
        }

        // Validate TTL
        if ttl_secs > MAX_SESSION_KEY_TTL {
            return Err(AgentError::InvalidTTL {
                max_ttl: MAX_SESSION_KEY_TTL,
                requested: ttl_secs,
            });
        }

        // Session key permissions must be a subset of agent permissions
        if !permissions.is_empty() && !agent.permissions.is_empty() {
            let sk_mask = permissions[0];
            let agent_mask = agent.permissions[0];
            if sk_mask & !agent_mask != 0 {
                return Err(AgentError::PermissionDenied(
                    "Session key permissions exceed agent permissions".into(),
                ));
            }
        }

        // Session key max_per_tx cannot exceed agent's max_per_tx
        if agent.max_per_tx > 0 && max_per_tx > agent.max_per_tx {
            return Err(AgentError::PermissionDenied(
                "Session key max_per_tx exceeds agent limit".into(),
            ));
        }

        // Check max session keys per agent
        let active_count = self
            .agent_session_keys
            .get(agent_id)
            .map(|keys| {
                keys.iter()
                    .filter(|k| {
                        self.session_keys
                            .get(k)
                            .map(|sk| sk.is_valid(now))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);

        if active_count >= MAX_SESSION_KEYS_PER_AGENT {
            return Err(AgentError::MaxSessionKeysReached(
                agent_id.to_string(),
                MAX_SESSION_KEYS_PER_AGENT,
            ));
        }

        let session_key = SessionKey {
            key_address,
            agent_id: agent_id.to_string(),
            owner: agent.owner,
            permissions,
            max_per_tx,
            issued_at: now,
            expires_at: now + ttl_secs,
            nonce: 0,
            revoked: false,
        };

        self.session_keys.insert(key_address, session_key.clone());
        self.agent_session_keys
            .entry(agent_id.to_string())
            .or_default()
            .push(key_address);

        info!(
            "Session key issued: {} for agent {} (TTL: {}s, expires: {})",
            key_address, agent_id, ttl_secs, session_key.expires_at
        );

        Ok(session_key)
    }

    /// Rotate a session key: revoke old, issue new with same or updated params
    pub fn rotate_session_key(
        &mut self,
        old_key_address: &Address,
        new_key_address: Address,
        ttl_secs: u64,
        now: u64,
    ) -> Result<SessionKey, AgentError> {
        // Get the old key's params
        let old_key = self
            .session_keys
            .get(old_key_address)
            .ok_or_else(|| AgentError::SessionKeyNotFound(*old_key_address))?
            .clone();

        // Revoke old key
        if let Some(sk) = self.session_keys.get_mut(old_key_address) {
            sk.revoked = true;
        }

        // Issue new key with same params
        self.issue_session_key(
            &old_key.agent_id,
            new_key_address,
            old_key.permissions,
            old_key.max_per_tx,
            ttl_secs,
            now,
        )
    }

    /// Revoke a session key
    pub fn revoke_session_key(&mut self, key_address: &Address) -> Result<(), AgentError> {
        let sk = self
            .session_keys
            .get_mut(key_address)
            .ok_or_else(|| AgentError::SessionKeyNotFound(*key_address))?;

        sk.revoked = true;
        info!("Session key revoked: {}", key_address);
        Ok(())
    }

    /// Validate a session key and check nonce for replay protection
    pub fn validate_session_key(
        &self,
        key_address: &Address,
        expected_nonce: u64,
        now: u64,
    ) -> Result<&SessionKey, AgentError> {
        let sk = self
            .session_keys
            .get(key_address)
            .ok_or_else(|| AgentError::SessionKeyNotFound(*key_address))?;

        if sk.revoked {
            return Err(AgentError::SessionKeyRevoked(*key_address));
        }

        if now >= sk.expires_at {
            return Err(AgentError::SessionKeyExpired {
                expires_at: sk.expires_at,
                now,
            });
        }

        if expected_nonce != sk.nonce {
            return Err(AgentError::SessionKeyNonceMismatch {
                expected: sk.nonce,
                got: expected_nonce,
            });
        }

        Ok(sk)
    }

    /// Increment the nonce of a session key after successful use
    pub fn increment_session_key_nonce(
        &mut self,
        key_address: &Address,
    ) -> Result<u64, AgentError> {
        let sk = self
            .session_keys
            .get_mut(key_address)
            .ok_or_else(|| AgentError::SessionKeyNotFound(*key_address))?;

        sk.nonce += 1;
        Ok(sk.nonce)
    }

    /// Get a session key by address
    pub fn get_session_key(&self, key_address: &Address) -> Option<&SessionKey> {
        self.session_keys.get(key_address)
    }

    /// List active session keys for an agent
    pub fn list_session_keys(&self, agent_id: &str, now: u64) -> Vec<&SessionKey> {
        self.agent_session_keys
            .get(agent_id)
            .map(|keys| {
                keys.iter()
                    .filter_map(|k| {
                        let sk = self.session_keys.get(k)?;
                        if sk.is_valid(now) {
                            Some(sk)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Prune expired/revoked session keys
    pub fn prune_session_keys(&mut self, now: u64) -> usize {
        let expired: Vec<Address> = self
            .session_keys
            .iter()
            .filter(|(_, sk)| !sk.is_valid(now))
            .map(|(addr, _)| *addr)
            .collect();

        let count = expired.len();
        for addr in &expired {
            self.session_keys.remove(addr);
        }

        // Clean up agent_session_keys index
        for keys in self.agent_session_keys.values_mut() {
            keys.retain(|k| !expired.contains(k));
        }
        self.agent_session_keys.retain(|_, keys| !keys.is_empty());

        if count > 0 {
            debug!("Pruned {} expired session keys", count);
        }
        count
    }

    /// Validate an agent operation submitted via session key.
    /// Combines session key validation + permission check + limit enforcement.
    pub fn validate_agent_operation(
        &mut self,
        key_address: &Address,
        permission: AgentPermission,
        amount: u128,
        nonce: u64,
        now: u64,
    ) -> Result<String, AgentError> {
        // 1. Validate session key
        let sk = self.validate_session_key(key_address, nonce, now)?;
        let agent_id = sk.agent_id.clone();
        let sk_permissions = sk.permissions.clone();
        let sk_max_per_tx = sk.max_per_tx;

        // 2. Check session key-level permission
        if !sk_permissions.is_empty()
            && !AgentPermission::has_permission(&sk_permissions, permission)
        {
            return Err(AgentError::PermissionDenied(format!(
                "Session key {} lacks permission {:?}",
                key_address, permission
            )));
        }

        // 3. Check session key per-tx limit
        if sk_max_per_tx > 0 && amount > sk_max_per_tx {
            return Err(AgentError::PerTxLimitExceeded {
                limit: sk_max_per_tx.to_string(),
                amount: amount.to_string(),
            });
        }

        // 4. Check agent-level permission and limits
        self.check_permission(&agent_id, permission, amount, now)?;

        Ok(agent_id)
    }
}

impl Default for AgentGuard {
    fn default() -> Self {
        Self::new()
    }
}

// ============ Tests ============

#[cfg(test)]
mod tests {
    use super::*;

    fn test_address(byte: u8) -> Address {
        Address::from_slice(&[byte; 20]).unwrap()
    }

    fn make_agent(id: &str, daily_limit: u128, max_per_tx: u128) -> AgentRecord {
        AgentRecord {
            agent_id: id.to_string(),
            owner: test_address(0x01),
            agent_address: test_address(0x02),
            permissions: vec![AgentPermission::All as u8],
            daily_limit,
            max_per_tx,
            deposit: 10_000_000_000_000_000_000, // 10 QFC
            spent_today: 0,
            last_reset: 1_000_000,
            nonce: 0,
            active: true,
        }
    }

    #[test]
    fn test_permission_check_allowed() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 100_000));

        let result = guard.check_permission("bot-1", AgentPermission::Transfer, 50_000, 1_000_100);
        assert!(result.is_ok());
    }

    #[test]
    fn test_permission_check_per_tx_exceeded() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 100_000));

        let result = guard.check_permission(
            "bot-1",
            AgentPermission::Transfer,
            150_000, // exceeds max_per_tx of 100,000
            1_000_100,
        );
        assert!(matches!(result, Err(AgentError::PerTxLimitExceeded { .. })));
    }

    #[test]
    fn test_permission_check_daily_limit_exceeded() {
        let mut guard = AgentGuard::new();
        let mut agent = make_agent("bot-1", 1_000_000, 500_000);
        agent.spent_today = 800_000;
        agent.last_reset = 1_000_000;
        guard.register_agent(agent);

        let result = guard.check_permission(
            "bot-1",
            AgentPermission::Transfer,
            300_000,   // 800k + 300k > 1M daily limit
            1_000_100, // same day
        );
        assert!(matches!(result, Err(AgentError::DailyLimitExceeded { .. })));
    }

    #[test]
    fn test_daily_limit_resets_after_day() {
        let mut guard = AgentGuard::new();
        let mut agent = make_agent("bot-1", 1_000_000, 500_000);
        agent.spent_today = 900_000;
        agent.last_reset = 1_000_000;
        guard.register_agent(agent);

        // Next day: daily counter should reset
        let result = guard.check_permission(
            "bot-1",
            AgentPermission::Transfer,
            500_000,
            1_000_000 + DAY_SECS + 1, // next day
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_permission_denied_missing_perm() {
        let mut guard = AgentGuard::new();
        let mut agent = make_agent("bot-1", 1_000_000, 500_000);
        agent.permissions = vec![AgentPermission::Transfer as u8]; // only transfer
        guard.register_agent(agent);

        let result = guard.check_permission(
            "bot-1",
            AgentPermission::Staking, // not allowed
            100,
            1_000_100,
        );
        assert!(matches!(result, Err(AgentError::PermissionDenied(_))));
    }

    #[test]
    fn test_record_spend() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        guard.record_spend("bot-1", 50_000, 1_000_100).unwrap();

        let agent = guard.get_agent("bot-1").unwrap();
        assert_eq!(agent.spent_today, 50_000);
        assert_eq!(agent.deposit, 10_000_000_000_000_000_000 - 50_000);
        assert_eq!(agent.nonce, 1);
    }

    #[test]
    fn test_revoke_agent() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        // Issue a session key
        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        // Revoke agent
        guard.revoke_agent("bot-1").unwrap();

        // Agent should be inactive
        assert!(!guard.get_agent("bot-1").unwrap().active);

        // Session key should be revoked
        assert!(guard.get_session_key(&sk_addr).unwrap().revoked);

        // Permission check should fail
        let result = guard.check_permission("bot-1", AgentPermission::Transfer, 100, 1_000_100);
        assert!(matches!(result, Err(AgentError::AgentNotActive(_))));
    }

    // ---- Session Key Tests (Issue #82) ----

    #[test]
    fn test_issue_session_key() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        let sk = guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        assert_eq!(sk.agent_id, "bot-1");
        assert_eq!(sk.key_address, sk_addr);
        assert_eq!(sk.expires_at, 1_000_000 + 3600);
        assert_eq!(sk.nonce, 0);
        assert!(!sk.revoked);
    }

    #[test]
    fn test_session_key_expired() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        // Try to validate after expiry
        let result = guard.validate_session_key(&sk_addr, 0, 1_003_601);
        assert!(matches!(result, Err(AgentError::SessionKeyExpired { .. })));
    }

    #[test]
    fn test_session_key_nonce_mismatch() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        // Valid nonce=0
        assert!(guard.validate_session_key(&sk_addr, 0, 1_000_100).is_ok());

        // Invalid nonce=5
        let result = guard.validate_session_key(&sk_addr, 5, 1_000_100);
        assert!(matches!(
            result,
            Err(AgentError::SessionKeyNonceMismatch { .. })
        ));
    }

    #[test]
    fn test_session_key_nonce_increment() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        // Use nonce 0
        guard.validate_session_key(&sk_addr, 0, 1_000_100).unwrap();
        guard.increment_session_key_nonce(&sk_addr).unwrap();

        // Now nonce should be 1
        guard.validate_session_key(&sk_addr, 1, 1_000_200).unwrap();

        // Old nonce 0 should fail
        let result = guard.validate_session_key(&sk_addr, 0, 1_000_300);
        assert!(matches!(
            result,
            Err(AgentError::SessionKeyNonceMismatch { .. })
        ));
    }

    #[test]
    fn test_rotate_session_key() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let old_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", old_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        let new_addr = test_address(0x66);
        let new_sk = guard
            .rotate_session_key(&old_addr, new_addr, 7200, 1_001_000)
            .unwrap();

        // Old key should be revoked
        assert!(guard.get_session_key(&old_addr).unwrap().revoked);

        // New key should be active
        assert_eq!(new_sk.key_address, new_addr);
        assert_eq!(new_sk.expires_at, 1_001_000 + 7200);
        assert!(!new_sk.revoked);
    }

    #[test]
    fn test_revoke_session_key() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        guard.revoke_session_key(&sk_addr).unwrap();

        let result = guard.validate_session_key(&sk_addr, 0, 1_000_100);
        assert!(matches!(result, Err(AgentError::SessionKeyRevoked(_))));
    }

    #[test]
    fn test_max_ttl_exceeded() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        let result = guard.issue_session_key(
            "bot-1",
            sk_addr,
            vec![0xFF],
            100_000,
            MAX_SESSION_KEY_TTL + 1, // exceeds max
            1_000_000,
        );
        assert!(matches!(result, Err(AgentError::InvalidTTL { .. })));
    }

    #[test]
    fn test_max_session_keys_per_agent() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        // Issue MAX keys
        for i in 0..MAX_SESSION_KEYS_PER_AGENT {
            let addr = Address::from_slice(&[i as u8 + 10; 20]).unwrap();
            guard
                .issue_session_key("bot-1", addr, vec![0xFF], 100_000, 3600, 1_000_000)
                .unwrap();
        }

        // Next one should fail
        let extra_addr = test_address(0xFE);
        let result =
            guard.issue_session_key("bot-1", extra_addr, vec![0xFF], 100_000, 3600, 1_000_000);
        assert!(matches!(
            result,
            Err(AgentError::MaxSessionKeysReached(_, _))
        ));
    }

    #[test]
    fn test_session_key_permission_subset() {
        let mut guard = AgentGuard::new();
        let mut agent = make_agent("bot-1", 1_000_000, 500_000);
        agent.permissions =
            vec![AgentPermission::Transfer as u8 | AgentPermission::InferenceTask as u8];
        guard.register_agent(agent);

        let sk_addr = test_address(0x55);
        // Try to issue session key with staking permission (not in agent's permissions)
        let result = guard.issue_session_key(
            "bot-1",
            sk_addr,
            vec![AgentPermission::Staking as u8],
            100_000,
            3600,
            1_000_000,
        );
        assert!(matches!(result, Err(AgentError::PermissionDenied(_))));
    }

    #[test]
    fn test_prune_expired_session_keys() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk1 = test_address(0x55);
        let sk2 = test_address(0x66);
        let sk3 = test_address(0x77);

        guard
            .issue_session_key("bot-1", sk1, vec![0xFF], 100_000, 100, 1_000_000) // expires at 1_000_100
            .unwrap();
        guard
            .issue_session_key("bot-1", sk2, vec![0xFF], 100_000, 200, 1_000_000) // expires at 1_000_200
            .unwrap();
        guard
            .issue_session_key("bot-1", sk3, vec![0xFF], 100_000, 3600, 1_000_000) // expires at 1_003_600
            .unwrap();

        let pruned = guard.prune_session_keys(1_000_300);
        assert_eq!(pruned, 2); // sk1 and sk2 expired
        assert!(guard.get_session_key(&sk1).is_none());
        assert!(guard.get_session_key(&sk2).is_none());
        assert!(guard.get_session_key(&sk3).is_some());
    }

    #[test]
    fn test_validate_agent_operation_via_session_key() {
        let mut guard = AgentGuard::new();
        guard.register_agent(make_agent("bot-1", 1_000_000, 500_000));

        let sk_addr = test_address(0x55);
        guard
            .issue_session_key("bot-1", sk_addr, vec![0xFF], 100_000, 3600, 1_000_000)
            .unwrap();

        // Valid operation
        let agent_id = guard
            .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 50_000, 0, 1_000_100)
            .unwrap();
        assert_eq!(agent_id, "bot-1");

        // Record spend and increment nonce
        guard.record_spend("bot-1", 50_000, 1_000_100).unwrap();
        guard.increment_session_key_nonce(&sk_addr).unwrap();

        // Next operation with nonce=1
        let agent_id = guard
            .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 50_000, 1, 1_000_200)
            .unwrap();
        assert_eq!(agent_id, "bot-1");
    }

    #[test]
    fn test_insufficient_deposit() {
        let mut guard = AgentGuard::new();
        let mut agent = make_agent("bot-1", u128::MAX, u128::MAX); // no daily/per-tx limit
        agent.deposit = 100; // very low deposit
        guard.register_agent(agent);

        let result = guard.check_permission(
            "bot-1",
            AgentPermission::Transfer,
            1_000, // exceeds deposit
            1_000_100,
        );
        assert!(matches!(
            result,
            Err(AgentError::InsufficientDeposit { .. })
        ));
    }
}
