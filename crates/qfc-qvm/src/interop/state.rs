//! Cross-VM State Management
//!
//! Handles state coordination between QVM and EVM execution environments.

use primitive_types::{H160, H256};
use std::collections::{HashMap, HashSet};

use crate::executor::{ExecutionError, ExecutionResult};

/// State access type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessType {
    Read,
    Write,
}

/// State access record
#[derive(Debug, Clone)]
pub struct StateAccess {
    pub contract: H160,
    pub slot: H256,
    pub access_type: AccessType,
    pub value: Option<H256>,
}

/// Cross-VM state coordinator
#[derive(Debug, Default)]
pub struct StateCoordinator {
    /// Pending state changes from QVM
    qvm_changes: HashMap<(H160, H256), H256>,

    /// Pending state changes from EVM
    evm_changes: HashMap<(H160, H256), H256>,

    /// Access list for gas calculation (EIP-2930)
    access_list: HashSet<(H160, H256)>,

    /// Warm accounts (EIP-2929)
    warm_accounts: HashSet<H160>,

    /// State snapshots for rollback
    snapshots: Vec<StateSnapshot>,

    /// Current snapshot ID
    snapshot_id: u64,
}

/// State snapshot for rollback
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub id: u64,
    pub qvm_changes: HashMap<(H160, H256), H256>,
    pub evm_changes: HashMap<(H160, H256), H256>,
}

impl StateCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a state read
    pub fn record_read(&mut self, contract: H160, slot: H256) {
        self.access_list.insert((contract, slot));
        self.warm_accounts.insert(contract);
    }

    /// Record a state write from QVM
    pub fn record_qvm_write(&mut self, contract: H160, slot: H256, value: H256) {
        self.access_list.insert((contract, slot));
        self.warm_accounts.insert(contract);
        self.qvm_changes.insert((contract, slot), value);
    }

    /// Record a state write from EVM
    pub fn record_evm_write(&mut self, contract: H160, slot: H256, value: H256) {
        self.access_list.insert((contract, slot));
        self.warm_accounts.insert(contract);
        self.evm_changes.insert((contract, slot), value);
    }

    /// Check if a slot is warm (accessed in current transaction)
    pub fn is_slot_warm(&self, contract: H160, slot: H256) -> bool {
        self.access_list.contains(&(contract, slot))
    }

    /// Check if an account is warm
    pub fn is_account_warm(&self, address: H160) -> bool {
        self.warm_accounts.contains(&address)
    }

    /// Get the current value of a slot (considering pending changes)
    pub fn get_current_value(&self, contract: H160, slot: H256) -> Option<H256> {
        // QVM changes take precedence (most recent)
        if let Some(&value) = self.qvm_changes.get(&(contract, slot)) {
            return Some(value);
        }
        // Then EVM changes
        if let Some(&value) = self.evm_changes.get(&(contract, slot)) {
            return Some(value);
        }
        None
    }

    /// Create a state snapshot
    pub fn snapshot(&mut self) -> u64 {
        let id = self.snapshot_id;
        self.snapshot_id += 1;

        self.snapshots.push(StateSnapshot {
            id,
            qvm_changes: self.qvm_changes.clone(),
            evm_changes: self.evm_changes.clone(),
        });

        id
    }

    /// Revert to a snapshot
    pub fn revert(&mut self, snapshot_id: u64) -> ExecutionResult<()> {
        let pos = self.snapshots.iter().position(|s| s.id == snapshot_id);

        if let Some(pos) = pos {
            let snapshot = self.snapshots[pos].clone();
            self.qvm_changes = snapshot.qvm_changes;
            self.evm_changes = snapshot.evm_changes;

            // Remove this and all later snapshots
            self.snapshots.truncate(pos);
            Ok(())
        } else {
            Err(ExecutionError::Internal(format!(
                "Snapshot {} not found",
                snapshot_id
            )))
        }
    }

    /// Commit all pending changes
    pub fn commit(&mut self) -> CommittedChanges {
        let changes = CommittedChanges {
            qvm_changes: std::mem::take(&mut self.qvm_changes),
            evm_changes: std::mem::take(&mut self.evm_changes),
        };

        self.snapshots.clear();
        changes
    }

    /// Clear all pending changes and snapshots
    pub fn clear(&mut self) {
        self.qvm_changes.clear();
        self.evm_changes.clear();
        self.access_list.clear();
        self.warm_accounts.clear();
        self.snapshots.clear();
    }

    /// Get all QVM state changes
    pub fn get_qvm_changes(&self) -> &HashMap<(H160, H256), H256> {
        &self.qvm_changes
    }

    /// Get all EVM state changes
    pub fn get_evm_changes(&self) -> &HashMap<(H160, H256), H256> {
        &self.evm_changes
    }

    /// Get the access list
    pub fn get_access_list(&self) -> Vec<(H160, Vec<H256>)> {
        let mut result: HashMap<H160, Vec<H256>> = HashMap::new();

        for (contract, slot) in &self.access_list {
            result.entry(*contract).or_default().push(*slot);
        }

        result.into_iter().collect()
    }

    /// Check for state conflicts between QVM and EVM
    pub fn check_conflicts(&self) -> Vec<StateConflict> {
        let mut conflicts = Vec::new();

        for ((contract, slot), qvm_value) in &self.qvm_changes {
            if let Some(evm_value) = self.evm_changes.get(&(*contract, *slot)) {
                if qvm_value != evm_value {
                    conflicts.push(StateConflict {
                        contract: *contract,
                        slot: *slot,
                        qvm_value: *qvm_value,
                        evm_value: *evm_value,
                    });
                }
            }
        }

        conflicts
    }

    /// Resolve conflicts by preferring QVM changes
    pub fn resolve_conflicts_prefer_qvm(&mut self) {
        for ((contract, slot), _) in self.qvm_changes.iter() {
            self.evm_changes.remove(&(*contract, *slot));
        }
    }

    /// Resolve conflicts by preferring EVM changes
    pub fn resolve_conflicts_prefer_evm(&mut self) {
        for ((contract, slot), value) in self.evm_changes.iter() {
            self.qvm_changes.insert((*contract, *slot), *value);
        }
    }
}

/// Committed state changes
#[derive(Debug, Clone)]
pub struct CommittedChanges {
    pub qvm_changes: HashMap<(H160, H256), H256>,
    pub evm_changes: HashMap<(H160, H256), H256>,
}

impl CommittedChanges {
    /// Get all changes merged (QVM takes precedence)
    pub fn merged(&self) -> HashMap<(H160, H256), H256> {
        let mut result = self.evm_changes.clone();
        result.extend(self.qvm_changes.clone());
        result
    }

    /// Get total number of changes
    pub fn len(&self) -> usize {
        let mut slots: HashSet<(H160, H256)> = HashSet::new();
        slots.extend(self.qvm_changes.keys());
        slots.extend(self.evm_changes.keys());
        slots.len()
    }

    /// Check if there are no changes
    pub fn is_empty(&self) -> bool {
        self.qvm_changes.is_empty() && self.evm_changes.is_empty()
    }
}

/// State conflict between QVM and EVM
#[derive(Debug, Clone)]
pub struct StateConflict {
    pub contract: H160,
    pub slot: H256,
    pub qvm_value: H256,
    pub evm_value: H256,
}

/// State proof for cross-VM verification
#[derive(Debug, Clone)]
pub struct StateProof {
    pub contract: H160,
    pub slot: H256,
    pub value: H256,
    pub proof: Vec<H256>,
}

/// Cross-VM state verifier
pub struct StateVerifier;

impl StateVerifier {
    /// Verify a state proof
    pub fn verify_proof(proof: &StateProof, _root: H256) -> bool {
        // Simplified verification - in production would verify Merkle proof
        !proof.proof.is_empty()
    }

    /// Generate a state proof
    pub fn generate_proof(
        _contract: H160,
        _slot: H256,
        value: H256,
    ) -> StateProof {
        // Simplified proof generation
        StateProof {
            contract: H160::zero(),
            slot: H256::zero(),
            value,
            proof: vec![H256::zero()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_coordinator() {
        let mut coordinator = StateCoordinator::new();
        let contract = H160::from_low_u64_be(0x1234);
        let slot = H256::from_low_u64_be(0);
        let value = H256::from_low_u64_be(42);

        // Record a write
        coordinator.record_qvm_write(contract, slot, value);

        // Check it's warm
        assert!(coordinator.is_slot_warm(contract, slot));
        assert!(coordinator.is_account_warm(contract));

        // Get current value
        assert_eq!(coordinator.get_current_value(contract, slot), Some(value));
    }

    #[test]
    fn test_snapshot_revert() {
        let mut coordinator = StateCoordinator::new();
        let contract = H160::from_low_u64_be(0x1234);
        let slot = H256::from_low_u64_be(0);

        // Write value 1
        coordinator.record_qvm_write(contract, slot, H256::from_low_u64_be(1));

        // Take snapshot
        let snapshot_id = coordinator.snapshot();

        // Write value 2
        coordinator.record_qvm_write(contract, slot, H256::from_low_u64_be(2));
        assert_eq!(
            coordinator.get_current_value(contract, slot),
            Some(H256::from_low_u64_be(2))
        );

        // Revert
        coordinator.revert(snapshot_id).unwrap();
        assert_eq!(
            coordinator.get_current_value(contract, slot),
            Some(H256::from_low_u64_be(1))
        );
    }

    #[test]
    fn test_conflict_detection() {
        let mut coordinator = StateCoordinator::new();
        let contract = H160::from_low_u64_be(0x1234);
        let slot = H256::from_low_u64_be(0);

        // QVM writes value 1
        coordinator.record_qvm_write(contract, slot, H256::from_low_u64_be(1));

        // EVM writes value 2
        coordinator.record_evm_write(contract, slot, H256::from_low_u64_be(2));

        // Should detect conflict
        let conflicts = coordinator.check_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].qvm_value, H256::from_low_u64_be(1));
        assert_eq!(conflicts[0].evm_value, H256::from_low_u64_be(2));
    }

    #[test]
    fn test_commit() {
        let mut coordinator = StateCoordinator::new();
        let contract = H160::from_low_u64_be(0x1234);
        let slot = H256::from_low_u64_be(0);
        let value = H256::from_low_u64_be(42);

        coordinator.record_qvm_write(contract, slot, value);

        let committed = coordinator.commit();
        assert_eq!(committed.qvm_changes.len(), 1);
        assert_eq!(committed.qvm_changes.get(&(contract, slot)), Some(&value));

        // Coordinator should be cleared
        assert!(coordinator.qvm_changes.is_empty());
    }

    #[test]
    fn test_access_list() {
        let mut coordinator = StateCoordinator::new();
        let contract1 = H160::from_low_u64_be(0x1234);
        let contract2 = H160::from_low_u64_be(0x5678);
        let slot1 = H256::from_low_u64_be(0);
        let slot2 = H256::from_low_u64_be(1);

        coordinator.record_read(contract1, slot1);
        coordinator.record_read(contract1, slot2);
        coordinator.record_read(contract2, slot1);

        let access_list = coordinator.get_access_list();
        assert_eq!(access_list.len(), 2); // 2 contracts

        let total_slots: usize = access_list.iter().map(|(_, slots)| slots.len()).sum();
        assert_eq!(total_slots, 3);
    }
}
