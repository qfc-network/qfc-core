//! Task-to-miner assignment logic

use qfc_inference::{BackendType, GpuTier, ModelId};
use qfc_types::Address;
use serde::{Deserialize, Serialize};

/// Miner capability registration
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MinerCapability {
    /// Miner address
    pub address: Address,
    /// Backend type
    pub backend: BackendType,
    /// GPU tier
    pub tier: GpuTier,
    /// Available memory in MB
    pub memory_mb: u64,
    /// Models the miner has loaded
    pub loaded_models: Vec<ModelId>,
    /// Last heartbeat timestamp
    pub last_seen: u64,
}

impl MinerCapability {
    pub fn new(
        address: Address,
        backend: BackendType,
        tier: GpuTier,
        memory_mb: u64,
    ) -> Self {
        Self {
            address,
            backend,
            tier,
            memory_mb,
            loaded_models: Vec::new(),
            last_seen: 0,
        }
    }

    /// Check if this miner is still active (seen within last 60s)
    pub fn is_active(&self, current_time: u64) -> bool {
        current_time.saturating_sub(self.last_seen) < 60_000
    }
}

/// Miner registry for tracking active miners and their capabilities
pub struct MinerRegistry {
    miners: Vec<MinerCapability>,
}

impl MinerRegistry {
    pub fn new() -> Self {
        Self {
            miners: Vec::new(),
        }
    }

    /// Register or update a miner's capability
    pub fn register(&mut self, capability: MinerCapability) {
        if let Some(existing) = self
            .miners
            .iter_mut()
            .find(|m| m.address == capability.address)
        {
            *existing = capability;
        } else {
            self.miners.push(capability);
        }
    }

    /// Remove inactive miners
    pub fn prune_inactive(&mut self, current_time: u64) {
        self.miners.retain(|m| m.is_active(current_time));
    }

    /// Get miners capable of running tasks at a given tier
    pub fn miners_for_tier(&self, tier: GpuTier) -> Vec<&MinerCapability> {
        self.miners
            .iter()
            .filter(|m| tier_matches(m.tier, tier))
            .collect()
    }

    /// Get total number of registered miners
    pub fn count(&self) -> usize {
        self.miners.len()
    }

    /// Get a miner by address
    pub fn get(&self, address: &Address) -> Option<&MinerCapability> {
        self.miners.iter().find(|m| m.address == *address)
    }
}

impl Default for MinerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if miner_tier can handle tasks of required_tier
fn tier_matches(miner_tier: GpuTier, required_tier: GpuTier) -> bool {
    match (miner_tier, required_tier) {
        (GpuTier::Hot, _) => true,
        (GpuTier::Warm, GpuTier::Hot) => false,
        (GpuTier::Warm, _) => true,
        (GpuTier::Cold, GpuTier::Cold) => true,
        (GpuTier::Cold, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_miner_registry() {
        let mut registry = MinerRegistry::new();
        assert_eq!(registry.count(), 0);

        let miner = MinerCapability {
            address: Address::new([0x01; 20]),
            backend: BackendType::Cuda,
            tier: GpuTier::Hot,
            memory_mb: 80_000,
            loaded_models: vec![],
            last_seen: 1000,
        };

        registry.register(miner);
        assert_eq!(registry.count(), 1);

        // Hot miner should show up for all tiers
        assert_eq!(registry.miners_for_tier(GpuTier::Cold).len(), 1);
        assert_eq!(registry.miners_for_tier(GpuTier::Warm).len(), 1);
        assert_eq!(registry.miners_for_tier(GpuTier::Hot).len(), 1);
    }

    #[test]
    fn test_miner_active_check() {
        let miner = MinerCapability {
            address: Address::new([0x01; 20]),
            backend: BackendType::Cpu,
            tier: GpuTier::Cold,
            memory_mb: 8000,
            loaded_models: vec![],
            last_seen: 1000,
        };

        assert!(miner.is_active(1000));
        assert!(miner.is_active(60_999));
        assert!(!miner.is_active(61_000));
    }

    #[test]
    fn test_prune_inactive() {
        let mut registry = MinerRegistry::new();

        registry.register(MinerCapability {
            address: Address::new([0x01; 20]),
            backend: BackendType::Cpu,
            tier: GpuTier::Cold,
            memory_mb: 8000,
            loaded_models: vec![],
            last_seen: 1000,
        });

        registry.register(MinerCapability {
            address: Address::new([0x02; 20]),
            backend: BackendType::Metal,
            tier: GpuTier::Warm,
            memory_mb: 16000,
            loaded_models: vec![],
            last_seen: 50_000,
        });

        // At time 62_000, miner 1 (last_seen=1000) should be pruned
        registry.prune_inactive(62_000);
        assert_eq!(registry.count(), 1);
        assert!(registry.get(&Address::new([0x02; 20])).is_some());
    }
}
