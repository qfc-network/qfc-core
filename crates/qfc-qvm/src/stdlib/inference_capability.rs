//! Inference Capability Resource Module
//!
//! Move-style linear resource representing the right to spend a budget on AI inference tasks.
//! Enforces budget limits, model allowlists, expiry, and freeze mechanics at the VM level.

use std::collections::HashMap;

use primitive_types::H160;
use qfc_types::Hash;

// ─── Error Codes ───

pub const E_INSUFFICIENT_BUDGET: u64 = 100;
pub const E_MODEL_NOT_ALLOWED: u64 = 101;
pub const E_CAPABILITY_EXPIRED: u64 = 102;
pub const E_CAPABILITY_FROZEN: u64 = 103;
pub const E_NOT_OWNER: u64 = 104;
pub const E_ZERO_BUDGET: u64 = 105;
pub const E_NOT_FOUND: u64 = 106;
pub const E_NOT_AUTHORIZED: u64 = 107;

// ─── Events ───

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityEvent {
    Created {
        capability_id: Hash,
        owner: H160,
        budget: u64,
        allowed_models: Vec<String>,
        expires_at: u64,
    },
    Used {
        capability_id: Hash,
        model_id: String,
        fee: u64,
        remaining_budget: u64,
    },
    Frozen {
        capability_id: Hash,
        frozen_by: H160,
    },
    Unfrozen {
        capability_id: Hash,
        unfrozen_by: H160,
    },
    ToppedUp {
        capability_id: Hash,
        added: u64,
        new_budget: u64,
    },
    Destroyed {
        capability_id: Hash,
        remaining_budget: u64,
    },
}

// ─── Core Resource ───

#[derive(Clone, Debug)]
pub struct InferenceCapability {
    pub id: Hash,
    pub owner: H160,
    pub remaining_budget: u64,
    pub allowed_models: Vec<String>,
    /// Unix timestamp, 0 = no expiry
    pub expires_at: u64,
    pub frozen: bool,
    pub total_spent: u64,
    pub total_tasks: u64,
    pub created_at: u64,
}

// ─── Capability Store ───

pub struct CapabilityStore {
    capabilities: HashMap<Hash, InferenceCapability>,
    /// owner -> list of capability IDs
    owner_index: HashMap<H160, Vec<Hash>>,
    events: Vec<CapabilityEvent>,
    now: u64,
}

impl CapabilityStore {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
            owner_index: HashMap::new(),
            events: Vec::new(),
            now: 0,
        }
    }

    pub fn set_timestamp(&mut self, now: u64) {
        self.now = now;
    }

    pub fn drain_events(&mut self) -> Vec<CapabilityEvent> {
        std::mem::take(&mut self.events)
    }

    /// Create a new inference capability
    pub fn create(
        &mut self,
        capability_id: Hash,
        owner: H160,
        budget: u64,
        allowed_models: Vec<String>,
        expires_at: u64,
    ) -> Result<&InferenceCapability, u64> {
        if budget == 0 {
            return Err(E_ZERO_BUDGET);
        }

        let cap = InferenceCapability {
            id: capability_id,
            owner,
            remaining_budget: budget,
            allowed_models: allowed_models.clone(),
            expires_at,
            frozen: false,
            total_spent: 0,
            total_tasks: 0,
            created_at: self.now,
        };

        self.owner_index
            .entry(owner)
            .or_default()
            .push(capability_id);
        self.capabilities.insert(capability_id, cap);

        self.events.push(CapabilityEvent::Created {
            capability_id,
            owner,
            budget,
            allowed_models,
            expires_at,
        });

        Ok(self.capabilities.get(&capability_id).unwrap())
    }

    /// Get capability by ID
    pub fn get(&self, id: &Hash) -> Option<&InferenceCapability> {
        self.capabilities.get(id)
    }

    /// List capabilities by owner
    pub fn list_by_owner(&self, owner: &H160) -> Vec<&InferenceCapability> {
        self.owner_index
            .get(owner)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.capabilities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Use capability to pay for inference
    pub fn use_for_inference(
        &mut self,
        capability_id: &Hash,
        model_id: &str,
        fee: u64,
    ) -> Result<(), u64> {
        let cap = self
            .capabilities
            .get_mut(capability_id)
            .ok_or(E_NOT_FOUND)?;

        if cap.frozen {
            return Err(E_CAPABILITY_FROZEN);
        }
        if cap.expires_at != 0 && self.now >= cap.expires_at {
            return Err(E_CAPABILITY_EXPIRED);
        }
        if !cap.allowed_models.is_empty() && !cap.allowed_models.iter().any(|m| m == model_id) {
            return Err(E_MODEL_NOT_ALLOWED);
        }
        if cap.remaining_budget < fee {
            return Err(E_INSUFFICIENT_BUDGET);
        }

        cap.remaining_budget -= fee;
        cap.total_spent += fee;
        cap.total_tasks += 1;

        self.events.push(CapabilityEvent::Used {
            capability_id: *capability_id,
            model_id: model_id.to_string(),
            fee,
            remaining_budget: cap.remaining_budget,
        });

        Ok(())
    }

    /// Top up budget
    pub fn top_up(&mut self, capability_id: &Hash, caller: H160, amount: u64) -> Result<(), u64> {
        let cap = self
            .capabilities
            .get_mut(capability_id)
            .ok_or(E_NOT_FOUND)?;
        if cap.owner != caller {
            return Err(E_NOT_OWNER);
        }
        cap.remaining_budget += amount;

        self.events.push(CapabilityEvent::ToppedUp {
            capability_id: *capability_id,
            added: amount,
            new_budget: cap.remaining_budget,
        });

        Ok(())
    }

    /// Freeze capability
    pub fn freeze(
        &mut self,
        capability_id: &Hash,
        caller: H160,
        is_governance: bool,
    ) -> Result<(), u64> {
        let cap = self
            .capabilities
            .get_mut(capability_id)
            .ok_or(E_NOT_FOUND)?;
        if cap.owner != caller && !is_governance {
            return Err(E_NOT_AUTHORIZED);
        }
        cap.frozen = true;
        self.events.push(CapabilityEvent::Frozen {
            capability_id: *capability_id,
            frozen_by: caller,
        });
        Ok(())
    }

    /// Unfreeze capability
    pub fn unfreeze(
        &mut self,
        capability_id: &Hash,
        caller: H160,
        is_governance: bool,
    ) -> Result<(), u64> {
        let cap = self
            .capabilities
            .get_mut(capability_id)
            .ok_or(E_NOT_FOUND)?;
        if cap.owner != caller && !is_governance {
            return Err(E_NOT_AUTHORIZED);
        }
        cap.frozen = false;
        self.events.push(CapabilityEvent::Unfrozen {
            capability_id: *capability_id,
            unfrozen_by: caller,
        });
        Ok(())
    }

    /// Destroy capability and return remaining budget
    pub fn destroy(&mut self, capability_id: &Hash, caller: H160) -> Result<u64, u64> {
        let cap = self.capabilities.get(capability_id).ok_or(E_NOT_FOUND)?;
        if cap.owner != caller {
            return Err(E_NOT_OWNER);
        }
        let refund = cap.remaining_budget;
        let owner = cap.owner;

        if let Some(ids) = self.owner_index.get_mut(&owner) {
            ids.retain(|id| id != capability_id);
        }
        self.capabilities.remove(capability_id);

        self.events.push(CapabilityEvent::Destroyed {
            capability_id: *capability_id,
            remaining_budget: refund,
        });

        Ok(refund)
    }

    /// Number of capabilities
    pub fn count(&self) -> usize {
        self.capabilities.len()
    }
}

impl Default for CapabilityStore {
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

    fn owner(n: u8) -> H160 {
        H160::from([n; 20])
    }

    fn make_store(now: u64) -> CapabilityStore {
        let mut s = CapabilityStore::new();
        s.set_timestamp(now);
        s
    }

    fn create_basic(store: &mut CapabilityStore, seed: &[u8], o: H160) -> Hash {
        let id = test_hash(seed);
        store
            .create(
                id,
                o,
                10_000,
                vec!["bert-v1".to_string(), "llama-7b".to_string()],
                0, // no expiry
            )
            .unwrap();
        id
    }

    #[test]
    fn test_create_capability() {
        let mut store = make_store(1000);
        let id = test_hash(b"cap1");
        let cap = store
            .create(id, owner(1), 5000, vec!["bert-v1".to_string()], 2000)
            .unwrap();
        assert_eq!(cap.remaining_budget, 5000);
        assert_eq!(cap.expires_at, 2000);
        assert!(!cap.frozen);
        assert_eq!(store.count(), 1);
    }

    #[test]
    fn test_create_zero_budget() {
        let mut store = make_store(1000);
        assert_eq!(
            store
                .create(test_hash(b"z"), owner(1), 0, vec![], 0)
                .unwrap_err(),
            E_ZERO_BUDGET
        );
    }

    #[test]
    fn test_use_for_inference_success() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"use1", owner(1));
        store.use_for_inference(&id, "bert-v1", 100).unwrap();
        let cap = store.get(&id).unwrap();
        assert_eq!(cap.remaining_budget, 9900);
        assert_eq!(cap.total_spent, 100);
        assert_eq!(cap.total_tasks, 1);
    }

    #[test]
    fn test_use_insufficient_budget() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"insuf", owner(1));
        assert_eq!(
            store.use_for_inference(&id, "bert-v1", 20_000).unwrap_err(),
            E_INSUFFICIENT_BUDGET
        );
    }

    #[test]
    fn test_use_model_not_allowed() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"model", owner(1));
        assert_eq!(
            store.use_for_inference(&id, "gpt-4", 100).unwrap_err(),
            E_MODEL_NOT_ALLOWED
        );
    }

    #[test]
    fn test_use_empty_allowlist_allows_all() {
        let mut store = make_store(1000);
        let id = test_hash(b"any");
        store.create(id, owner(1), 5000, vec![], 0).unwrap();
        // Empty allowlist means any model is allowed
        assert!(store.use_for_inference(&id, "any-model", 100).is_ok());
    }

    #[test]
    fn test_use_frozen() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"frz", owner(1));
        store.freeze(&id, owner(1), false).unwrap();
        assert_eq!(
            store.use_for_inference(&id, "bert-v1", 100).unwrap_err(),
            E_CAPABILITY_FROZEN
        );
    }

    #[test]
    fn test_use_expired() {
        let mut store = make_store(1000);
        let id = test_hash(b"exp");
        store
            .create(id, owner(1), 5000, vec!["bert-v1".to_string()], 2000)
            .unwrap();
        store.set_timestamp(2000); // at expiry
        assert_eq!(
            store.use_for_inference(&id, "bert-v1", 100).unwrap_err(),
            E_CAPABILITY_EXPIRED
        );
    }

    #[test]
    fn test_use_before_expiry() {
        let mut store = make_store(1000);
        let id = test_hash(b"bexp");
        store
            .create(id, owner(1), 5000, vec!["bert-v1".to_string()], 2000)
            .unwrap();
        store.set_timestamp(1999);
        assert!(store.use_for_inference(&id, "bert-v1", 100).is_ok());
    }

    #[test]
    fn test_top_up() {
        let mut store = make_store(1000);
        let o = owner(1);
        let id = create_basic(&mut store, b"top", o);
        store.top_up(&id, o, 5000).unwrap();
        assert_eq!(store.get(&id).unwrap().remaining_budget, 15_000);
    }

    #[test]
    fn test_top_up_not_owner() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"topn", owner(1));
        assert_eq!(store.top_up(&id, owner(2), 100).unwrap_err(), E_NOT_OWNER);
    }

    #[test]
    fn test_freeze_unfreeze() {
        let mut store = make_store(1000);
        let o = owner(1);
        let id = create_basic(&mut store, b"fu", o);

        store.freeze(&id, o, false).unwrap();
        assert!(store.get(&id).unwrap().frozen);

        store.unfreeze(&id, o, false).unwrap();
        assert!(!store.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_freeze_governance() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"fg", owner(1));
        // Governance can freeze anyone
        store.freeze(&id, owner(99), true).unwrap();
        assert!(store.get(&id).unwrap().frozen);
    }

    #[test]
    fn test_freeze_not_authorized() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"fna", owner(1));
        assert_eq!(
            store.freeze(&id, owner(2), false).unwrap_err(),
            E_NOT_AUTHORIZED
        );
    }

    #[test]
    fn test_destroy() {
        let mut store = make_store(1000);
        let o = owner(1);
        let id = create_basic(&mut store, b"dest", o);
        // Use some budget first
        store.use_for_inference(&id, "bert-v1", 3000).unwrap();
        let refund = store.destroy(&id, o).unwrap();
        assert_eq!(refund, 7000);
        assert!(store.get(&id).is_none());
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn test_destroy_not_owner() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"destn", owner(1));
        assert_eq!(store.destroy(&id, owner(2)).unwrap_err(), E_NOT_OWNER);
    }

    #[test]
    fn test_list_by_owner() {
        let mut store = make_store(1000);
        let o = owner(1);
        create_basic(&mut store, b"l1", o);
        create_basic(&mut store, b"l2", o);
        create_basic(&mut store, b"l3", owner(2));

        assert_eq!(store.list_by_owner(&o).len(), 2);
        assert_eq!(store.list_by_owner(&owner(2)).len(), 1);
        assert_eq!(store.list_by_owner(&owner(99)).len(), 0);
    }

    #[test]
    fn test_events() {
        let mut store = make_store(1000);
        let o = owner(1);
        let id = create_basic(&mut store, b"ev", o);
        store.use_for_inference(&id, "bert-v1", 100).unwrap();
        store.top_up(&id, o, 500).unwrap();
        store.freeze(&id, o, false).unwrap();

        let events = store.drain_events();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], CapabilityEvent::Created { .. }));
        assert!(matches!(events[1], CapabilityEvent::Used { .. }));
        assert!(matches!(events[2], CapabilityEvent::ToppedUp { .. }));
        assert!(matches!(events[3], CapabilityEvent::Frozen { .. }));

        assert!(store.drain_events().is_empty());
    }

    #[test]
    fn test_multiple_uses_drain_budget() {
        let mut store = make_store(1000);
        let id = create_basic(&mut store, b"drain", owner(1));
        for _ in 0..100 {
            store.use_for_inference(&id, "bert-v1", 100).unwrap();
        }
        let cap = store.get(&id).unwrap();
        assert_eq!(cap.remaining_budget, 0);
        assert_eq!(cap.total_spent, 10_000);
        assert_eq!(cap.total_tasks, 100);

        // Next use should fail
        assert_eq!(
            store.use_for_inference(&id, "bert-v1", 1).unwrap_err(),
            E_INSUFFICIENT_BUDGET
        );
    }

    #[test]
    fn test_no_expiry() {
        let mut store = make_store(1000);
        let id = test_hash(b"noexp");
        store
            .create(id, owner(1), 5000, vec![], 0) // expires_at=0 means no expiry
            .unwrap();
        store.set_timestamp(u64::MAX - 1);
        // Should still work with no expiry
        assert!(store.use_for_inference(&id, "any", 100).is_ok());
    }

    #[test]
    fn test_concurrent_budget_usage() {
        let mut store = make_store(1000);
        let id = test_hash(b"conc");
        store.create(id, owner(1), 1000, vec![], 0).unwrap();

        // Simulate multiple tasks competing for budget
        assert!(store.use_for_inference(&id, "m1", 400).is_ok());
        assert!(store.use_for_inference(&id, "m2", 400).is_ok());
        // Only 200 left
        assert_eq!(
            store.use_for_inference(&id, "m3", 300).unwrap_err(),
            E_INSUFFICIENT_BUDGET
        );
        assert!(store.use_for_inference(&id, "m4", 200).is_ok());
        assert_eq!(store.get(&id).unwrap().remaining_budget, 0);
    }
}
