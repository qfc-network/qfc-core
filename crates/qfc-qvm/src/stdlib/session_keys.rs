//! Session Key Module
//!
//! Temporary keys with scoped permissions, spending limits, TTL, and replay protection.
//! Session keys allow agents to operate autonomously without exposing the owner's private key.

use std::collections::HashMap;

use primitive_types::H160;
use qfc_types::Hash;

// ─── Permission Bitmask Constants ───

/// Submit inference tasks
pub const PERM_INFERENCE: u64 = 0x01;
/// Transfer tokens
pub const PERM_TRANSFER: u64 = 0x02;
/// Stake/unstake operations
pub const PERM_STAKE: u64 = 0x04;
/// Register sub-agents
pub const PERM_REGISTER: u64 = 0x08;

/// All permissions
pub const PERM_ALL: u64 = PERM_INFERENCE | PERM_TRANSFER | PERM_STAKE | PERM_REGISTER;

// ─── Error Codes ───

pub const E_NOT_OWNER: u64 = 300;
pub const E_KEY_EXPIRED: u64 = 301;
pub const E_KEY_REVOKED: u64 = 302;
pub const E_PERMISSION_DENIED: u64 = 303;
pub const E_SPENDING_LIMIT_EXCEEDED: u64 = 304;
pub const E_INVALID_NONCE: u64 = 305;
pub const E_MAX_KEYS_EXCEEDED: u64 = 306;
pub const E_NOT_FOUND: u64 = 307;
pub const E_INVALID_TTL: u64 = 308;
pub const E_AGENT_NOT_FOUND: u64 = 309;
pub const E_ZERO_PERMISSIONS: u64 = 310;

// ─── Limits ───

/// Maximum session key TTL: 7 days
pub const MAX_TTL_SECS: u64 = 7 * 24 * 3600;
/// Maximum session keys per agent
pub const MAX_KEYS_PER_AGENT: usize = 10;

// ─── Events ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionKeyEvent {
    Issued {
        key_id: Hash,
        agent_id: Hash,
        public_key: Vec<u8>,
        permissions: u64,
        expires_at: u64,
    },
    Revoked {
        key_id: Hash,
        agent_id: Hash,
    },
    Rotated {
        old_key_id: Hash,
        new_key_id: Hash,
        agent_id: Hash,
    },
}

// ─── Core Resource ───

#[derive(Clone, Debug)]
pub struct SessionKey {
    pub id: Hash,
    pub agent_id: Hash,
    pub public_key: Vec<u8>,
    pub permissions: u64,
    pub spending_limit: u64,
    pub spent_this_period: u64,
    pub period_start: u64,
    pub period_duration: u64,
    pub expires_at: u64,
    pub nonce: u64,
    pub revoked: bool,
    pub created_at: u64,
}

impl SessionKey {
    /// Check if the key has a specific permission
    pub fn has_permission(&self, perm: u64) -> bool {
        self.permissions & perm == perm
    }

    /// Check if the key is expired
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    /// Check if the key is valid (not expired, not revoked)
    pub fn is_valid(&self, now: u64) -> bool {
        !self.revoked && !self.is_expired(now)
    }
}

// ─── Session Key Store ───

pub struct SessionKeyStore {
    /// All session keys by key ID
    keys: HashMap<Hash, SessionKey>,
    /// agent_id -> list of key IDs
    agent_keys: HashMap<Hash, Vec<Hash>>,
    events: Vec<SessionKeyEvent>,
    now: u64,
}

impl SessionKeyStore {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
            agent_keys: HashMap::new(),
            events: Vec::new(),
            now: 0,
        }
    }

    pub fn set_timestamp(&mut self, now: u64) {
        self.now = now;
    }

    pub fn drain_events(&mut self) -> Vec<SessionKeyEvent> {
        std::mem::take(&mut self.events)
    }

    /// Issue a new session key for an agent
    pub fn issue(
        &mut self,
        key_id: Hash,
        agent_id: Hash,
        _owner: H160,
        public_key: Vec<u8>,
        permissions: u64,
        spending_limit: u64,
        period_duration: u64,
        ttl_secs: u64,
    ) -> Result<&SessionKey, u64> {
        if permissions == 0 {
            return Err(E_ZERO_PERMISSIONS);
        }
        if ttl_secs == 0 || ttl_secs > MAX_TTL_SECS {
            return Err(E_INVALID_TTL);
        }

        // Check max keys per agent
        let current_count = self
            .agent_keys
            .get(&agent_id)
            .map(|keys| {
                keys.iter()
                    .filter(|kid| {
                        self.keys
                            .get(*kid)
                            .map(|k| k.is_valid(self.now))
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);

        if current_count >= MAX_KEYS_PER_AGENT {
            return Err(E_MAX_KEYS_EXCEEDED);
        }

        let expires_at = self.now + ttl_secs;
        let key = SessionKey {
            id: key_id,
            agent_id,
            public_key: public_key.clone(),
            permissions,
            spending_limit,
            spent_this_period: 0,
            period_start: self.now,
            period_duration,
            expires_at,
            nonce: 0,
            revoked: false,
            created_at: self.now,
        };

        self.agent_keys.entry(agent_id).or_default().push(key_id);
        self.keys.insert(key_id, key);

        self.events.push(SessionKeyEvent::Issued {
            key_id,
            agent_id,
            public_key,
            permissions,
            expires_at,
        });

        Ok(self.keys.get(&key_id).unwrap())
    }

    /// Validate a session key for an operation
    pub fn validate(
        &mut self,
        key_id: &Hash,
        operation: u64,
        amount: u64,
        provided_nonce: u64,
    ) -> Result<(), u64> {
        let key = self.keys.get_mut(key_id).ok_or(E_NOT_FOUND)?;

        if key.revoked {
            return Err(E_KEY_REVOKED);
        }
        if key.is_expired(self.now) {
            return Err(E_KEY_EXPIRED);
        }
        if !key.has_permission(operation) {
            return Err(E_PERMISSION_DENIED);
        }
        if provided_nonce != key.nonce {
            return Err(E_INVALID_NONCE);
        }

        // Check spending limit with period auto-reset
        if key.period_duration > 0 {
            let elapsed = self.now.saturating_sub(key.period_start);
            if elapsed >= key.period_duration {
                // Reset period
                key.spent_this_period = 0;
                key.period_start = self.now;
            }
        }

        if key.spending_limit > 0 && key.spent_this_period + amount > key.spending_limit {
            return Err(E_SPENDING_LIMIT_EXCEEDED);
        }

        // Record spend and increment nonce
        key.spent_this_period += amount;
        key.nonce += 1;

        Ok(())
    }

    /// Get a session key by ID
    pub fn get(&self, key_id: &Hash) -> Option<&SessionKey> {
        self.keys.get(key_id)
    }

    /// List active session keys for an agent
    pub fn list_for_agent(&self, agent_id: &Hash) -> Vec<&SessionKey> {
        self.agent_keys
            .get(agent_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| {
                        let key = self.keys.get(id)?;
                        if key.is_valid(self.now) {
                            Some(key)
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Revoke a session key
    pub fn revoke(&mut self, key_id: &Hash) -> Result<(), u64> {
        let key = self.keys.get_mut(key_id).ok_or(E_NOT_FOUND)?;
        key.revoked = true;
        let agent_id = key.agent_id;

        self.events.push(SessionKeyEvent::Revoked {
            key_id: *key_id,
            agent_id,
        });

        Ok(())
    }

    /// Revoke all session keys for an agent
    pub fn revoke_all_for_agent(&mut self, agent_id: &Hash) {
        if let Some(ids) = self.agent_keys.get(agent_id) {
            let ids_clone: Vec<Hash> = ids.clone();
            for kid in ids_clone {
                if let Some(key) = self.keys.get_mut(&kid) {
                    if !key.revoked {
                        key.revoked = true;
                        self.events.push(SessionKeyEvent::Revoked {
                            key_id: kid,
                            agent_id: *agent_id,
                        });
                    }
                }
            }
        }
    }

    /// Rotate: revoke old key and issue new one with same agent and permissions
    pub fn rotate(
        &mut self,
        old_key_id: &Hash,
        new_key_id: Hash,
        new_public_key: Vec<u8>,
        new_ttl_secs: u64,
    ) -> Result<&SessionKey, u64> {
        let old = self.keys.get(old_key_id).ok_or(E_NOT_FOUND)?;
        let agent_id = old.agent_id;
        let permissions = old.permissions;
        let spending_limit = old.spending_limit;
        let period_duration = old.period_duration;
        let owner = H160::zero(); // Owner check done at caller level

        // Revoke old
        self.revoke(old_key_id)?;

        // Issue new
        self.issue(
            new_key_id,
            agent_id,
            owner,
            new_public_key,
            permissions,
            spending_limit,
            period_duration,
            new_ttl_secs,
        )?;

        self.events.push(SessionKeyEvent::Rotated {
            old_key_id: *old_key_id,
            new_key_id,
            agent_id,
        });

        Ok(self.keys.get(&new_key_id).unwrap())
    }

    /// Prune expired and revoked keys
    pub fn prune(&mut self) -> usize {
        let expired_ids: Vec<Hash> = self
            .keys
            .iter()
            .filter(|(_, k)| k.revoked || k.is_expired(self.now))
            .map(|(id, _)| *id)
            .collect();

        let count = expired_ids.len();
        for id in &expired_ids {
            if let Some(key) = self.keys.remove(id) {
                if let Some(ids) = self.agent_keys.get_mut(&key.agent_id) {
                    ids.retain(|kid| kid != id);
                }
            }
        }
        count
    }

    /// Number of keys
    pub fn key_count(&self) -> usize {
        self.keys.len()
    }
}

impl Default for SessionKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper: describe permissions as human-readable strings
pub fn describe_permissions(perms: u64) -> Vec<&'static str> {
    let mut result = Vec::new();
    if perms & PERM_INFERENCE != 0 {
        result.push("inference");
    }
    if perms & PERM_TRANSFER != 0 {
        result.push("transfer");
    }
    if perms & PERM_STAKE != 0 {
        result.push("stake");
    }
    if perms & PERM_REGISTER != 0 {
        result.push("register");
    }
    result
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

    fn make_store(now: u64) -> SessionKeyStore {
        let mut s = SessionKeyStore::new();
        s.set_timestamp(now);
        s
    }

    fn issue_basic(store: &mut SessionKeyStore, seed: &[u8], agent_id: Hash) -> Hash {
        let key_id = test_hash(seed);
        store
            .issue(
                key_id,
                agent_id,
                H160::from([1; 20]),
                vec![0xAA; 32],
                PERM_INFERENCE,
                1000,  // spending limit
                86400, // 1 day period
                MAX_TTL_SECS,
            )
            .unwrap();
        key_id
    }

    #[test]
    fn test_issue_session_key() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent1");
        let key_id = issue_basic(&mut store, b"key1", agent_id);
        let key = store.get(&key_id).unwrap();
        assert_eq!(key.permissions, PERM_INFERENCE);
        assert_eq!(key.spending_limit, 1000);
        assert_eq!(key.nonce, 0);
        assert!(!key.revoked);
        assert!(key.is_valid(1000));
    }

    #[test]
    fn test_issue_zero_permissions() {
        let mut store = make_store(1000);
        assert_eq!(
            store
                .issue(test_hash(b"k"), test_hash(b"a"), H160::zero(), vec![], 0, 0, 0, 3600)
                .unwrap_err(),
            E_ZERO_PERMISSIONS
        );
    }

    #[test]
    fn test_issue_invalid_ttl() {
        let mut store = make_store(1000);
        // TTL = 0
        assert_eq!(
            store
                .issue(test_hash(b"k"), test_hash(b"a"), H160::zero(), vec![], PERM_INFERENCE, 0, 0, 0)
                .unwrap_err(),
            E_INVALID_TTL
        );
        // TTL too long
        assert_eq!(
            store
                .issue(
                    test_hash(b"k2"),
                    test_hash(b"a"),
                    H160::zero(),
                    vec![],
                    PERM_INFERENCE,
                    0,
                    0,
                    MAX_TTL_SECS + 1,
                )
                .unwrap_err(),
            E_INVALID_TTL
        );
    }

    #[test]
    fn test_validate_success() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"vk", agent_id);
        assert!(store.validate(&key_id, PERM_INFERENCE, 100, 0).is_ok());
        // Nonce should increment
        assert_eq!(store.get(&key_id).unwrap().nonce, 1);
    }

    #[test]
    fn test_validate_wrong_permission() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"wp", agent_id);
        assert_eq!(
            store.validate(&key_id, PERM_TRANSFER, 100, 0).unwrap_err(),
            E_PERMISSION_DENIED
        );
    }

    #[test]
    fn test_validate_expired() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"exp", agent_id);
        store.set_timestamp(1000 + MAX_TTL_SECS); // at expiry
        assert_eq!(
            store.validate(&key_id, PERM_INFERENCE, 100, 0).unwrap_err(),
            E_KEY_EXPIRED
        );
    }

    #[test]
    fn test_validate_revoked() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"rev", agent_id);
        store.revoke(&key_id).unwrap();
        assert_eq!(
            store.validate(&key_id, PERM_INFERENCE, 100, 0).unwrap_err(),
            E_KEY_REVOKED
        );
    }

    #[test]
    fn test_validate_wrong_nonce() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"nonce", agent_id);
        assert_eq!(
            store.validate(&key_id, PERM_INFERENCE, 100, 1).unwrap_err(),
            E_INVALID_NONCE
        );
    }

    #[test]
    fn test_spending_limit() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"spend", agent_id);
        // Spend up to limit
        store.validate(&key_id, PERM_INFERENCE, 500, 0).unwrap();
        store.validate(&key_id, PERM_INFERENCE, 400, 1).unwrap();
        // Exceeds limit (500 + 400 + 200 > 1000)
        assert_eq!(
            store.validate(&key_id, PERM_INFERENCE, 200, 2).unwrap_err(),
            E_SPENDING_LIMIT_EXCEEDED
        );
    }

    #[test]
    fn test_spending_limit_period_reset() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"reset", agent_id);
        // Spend to limit
        store.validate(&key_id, PERM_INFERENCE, 1000, 0).unwrap();
        // Can't spend more
        assert_eq!(
            store.validate(&key_id, PERM_INFERENCE, 1, 1).unwrap_err(),
            E_SPENDING_LIMIT_EXCEEDED
        );
        // Advance past period
        store.set_timestamp(1000 + 86400);
        // Should work now (period reset)
        store.validate(&key_id, PERM_INFERENCE, 500, 1).unwrap();
        assert_eq!(store.get(&key_id).unwrap().spent_this_period, 500);
    }

    #[test]
    fn test_no_spending_limit() {
        let mut store = make_store(1000);
        let key_id = test_hash(b"nolimit");
        store
            .issue(
                key_id,
                test_hash(b"agent"),
                H160::zero(),
                vec![],
                PERM_INFERENCE,
                0, // no limit
                0,
                3600,
            )
            .unwrap();
        // Should allow any amount
        store.validate(&key_id, PERM_INFERENCE, u64::MAX, 0).unwrap();
    }

    #[test]
    fn test_list_for_agent() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        issue_basic(&mut store, b"k1", agent_id);
        issue_basic(&mut store, b"k2", agent_id);
        issue_basic(&mut store, b"k3", test_hash(b"other"));

        assert_eq!(store.list_for_agent(&agent_id).len(), 2);
    }

    #[test]
    fn test_list_excludes_revoked() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let k1 = issue_basic(&mut store, b"k1", agent_id);
        issue_basic(&mut store, b"k2", agent_id);
        store.revoke(&k1).unwrap();

        assert_eq!(store.list_for_agent(&agent_id).len(), 1);
    }

    #[test]
    fn test_revoke_all_for_agent() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        issue_basic(&mut store, b"k1", agent_id);
        issue_basic(&mut store, b"k2", agent_id);
        store.revoke_all_for_agent(&agent_id);

        assert_eq!(store.list_for_agent(&agent_id).len(), 0);
    }

    #[test]
    fn test_rotate() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let old_id = issue_basic(&mut store, b"old", agent_id);
        let new_id = test_hash(b"new");

        store
            .rotate(&old_id, new_id, vec![0xBB; 32], MAX_TTL_SECS)
            .unwrap();

        // Old key should be revoked
        assert!(store.get(&old_id).unwrap().revoked);
        // New key should be valid
        assert!(store.get(&new_id).unwrap().is_valid(1000));
        // Should inherit permissions
        assert_eq!(store.get(&new_id).unwrap().permissions, PERM_INFERENCE);
    }

    #[test]
    fn test_max_keys_per_agent() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        for i in 0..MAX_KEYS_PER_AGENT {
            issue_basic(&mut store, format!("key-{i}").as_bytes(), agent_id);
        }
        // 11th key should fail
        assert_eq!(
            store
                .issue(
                    test_hash(b"overflow"),
                    agent_id,
                    H160::zero(),
                    vec![],
                    PERM_INFERENCE,
                    0,
                    0,
                    3600,
                )
                .unwrap_err(),
            E_MAX_KEYS_EXCEEDED
        );
    }

    #[test]
    fn test_prune() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let k1 = issue_basic(&mut store, b"p1", agent_id);
        issue_basic(&mut store, b"p2", agent_id);
        store.revoke(&k1).unwrap();

        let pruned = store.prune();
        assert_eq!(pruned, 1);
        assert_eq!(store.key_count(), 1);
    }

    #[test]
    fn test_has_permission() {
        let key = SessionKey {
            id: test_hash(b"k"),
            agent_id: test_hash(b"a"),
            public_key: vec![],
            permissions: PERM_INFERENCE | PERM_TRANSFER,
            spending_limit: 0,
            spent_this_period: 0,
            period_start: 0,
            period_duration: 0,
            expires_at: u64::MAX,
            nonce: 0,
            revoked: false,
            created_at: 0,
        };
        assert!(key.has_permission(PERM_INFERENCE));
        assert!(key.has_permission(PERM_TRANSFER));
        assert!(!key.has_permission(PERM_STAKE));
        assert!(!key.has_permission(PERM_REGISTER));
        // Combined check
        assert!(key.has_permission(PERM_INFERENCE | PERM_TRANSFER));
        assert!(!key.has_permission(PERM_INFERENCE | PERM_STAKE));
    }

    #[test]
    fn test_describe_permissions() {
        assert_eq!(describe_permissions(PERM_INFERENCE), vec!["inference"]);
        assert_eq!(
            describe_permissions(PERM_ALL),
            vec!["inference", "transfer", "stake", "register"]
        );
        assert!(describe_permissions(0).is_empty());
    }

    #[test]
    fn test_events_emitted() {
        let mut store = make_store(1000);
        let agent_id = test_hash(b"agent");
        let key_id = issue_basic(&mut store, b"ev", agent_id);
        store.revoke(&key_id).unwrap();

        let events = store.drain_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], SessionKeyEvent::Issued { .. }));
        assert!(matches!(events[1], SessionKeyEvent::Revoked { .. }));
        assert!(store.drain_events().is_empty());
    }
}
