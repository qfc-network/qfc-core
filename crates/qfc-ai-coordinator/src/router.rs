//! Task router: off-chain global dispatcher
//!
//! Matches tasks to miners by GPU tier and loaded model affinity.
//! Priority: Hot > Warm > Cold > Any; within priority: least-loaded.

use std::collections::HashMap;

use qfc_inference::task::ModelId;
use qfc_types::Address;

/// Model layer classification (mirrors qfc_inference::scheduler::ModelLayer)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelLayer {
    Hot,
    Warm,
    Cold,
}

/// A miner's current model and task status
#[derive(Clone, Debug)]
pub struct MinerModelStatus {
    pub address: Address,
    pub models: Vec<(ModelId, ModelLayer)>,
    pub pending_tasks: u32,
    pub tier: u8,
    pub last_updated: u64,
}

/// Routes tasks to the best-available miner
pub struct TaskRouter {
    miner_models: HashMap<Address, MinerModelStatus>,
}

impl TaskRouter {
    pub fn new() -> Self {
        Self {
            miner_models: HashMap::new(),
        }
    }

    /// Update a miner's loaded models and tier (called on heartbeat/status report)
    pub fn update_miner_models(
        &mut self,
        address: Address,
        models: Vec<(ModelId, ModelLayer)>,
        tier: u8,
    ) {
        let now = now_ms();
        let entry = self
            .miner_models
            .entry(address)
            .or_insert_with(|| MinerModelStatus {
                address,
                models: Vec::new(),
                pending_tasks: 0,
                tier,
                last_updated: now,
            });
        entry.models = models;
        entry.tier = tier;
        entry.last_updated = now;
    }

    /// Select the best miner for a given model and required tier.
    /// Priority: Hot > Warm > Cold > Any (without model); within priority: least-loaded.
    pub fn select_miner(&self, model_id: &ModelId, required_tier: u8) -> Option<Address> {
        let mut candidates: Vec<(&Address, Option<ModelLayer>, u32)> = Vec::new();

        for (addr, status) in &self.miner_models {
            // Must meet tier requirement
            if status.tier < required_tier {
                continue;
            }

            // Find what layer the model is in (if any)
            let model_layer = status
                .models
                .iter()
                .find(|(id, _)| id == model_id)
                .map(|(_, layer)| *layer);

            candidates.push((addr, model_layer, status.pending_tasks));
        }

        if candidates.is_empty() {
            return None;
        }

        // Sort by: model layer priority (Hot best), then least-loaded
        candidates.sort_by(|a, b| {
            let layer_priority = |l: Option<ModelLayer>| -> u8 {
                match l {
                    Some(ModelLayer::Hot) => 0,
                    Some(ModelLayer::Warm) => 1,
                    Some(ModelLayer::Cold) => 2,
                    None => 3,
                }
            };
            let pa = layer_priority(a.1);
            let pb = layer_priority(b.1);
            pa.cmp(&pb).then(a.2.cmp(&b.2))
        });

        candidates.first().map(|(addr, _, _)| **addr)
    }

    /// Increment pending task count for a miner
    pub fn assign_task(&mut self, miner: &Address) {
        if let Some(status) = self.miner_models.get_mut(miner) {
            status.pending_tasks += 1;
        }
    }

    /// Decrement pending task count for a miner
    pub fn complete_task(&mut self, miner: &Address) {
        if let Some(status) = self.miner_models.get_mut(miner) {
            status.pending_tasks = status.pending_tasks.saturating_sub(1);
        }
    }

    /// Remove miners that haven't reported in `max_age_ms`
    pub fn prune_stale(&mut self, now: u64, max_age_ms: u64) {
        self.miner_models
            .retain(|_, status| now.saturating_sub(status.last_updated) < max_age_ms);
    }

    /// Get number of tracked miners
    pub fn miner_count(&self) -> usize {
        self.miner_models.len()
    }
}

impl Default for TaskRouter {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn model(name: &str) -> ModelId {
        ModelId::new(name, "v1")
    }

    #[test]
    fn test_select_miner_hot_priority() {
        let mut router = TaskRouter::new();

        // Miner 1: has model hot
        router.update_miner_models(addr(1), vec![(model("bert"), ModelLayer::Hot)], 3);

        // Miner 2: has model warm
        router.update_miner_models(addr(2), vec![(model("bert"), ModelLayer::Warm)], 3);

        // Miner 3: has model cold
        router.update_miner_models(addr(3), vec![(model("bert"), ModelLayer::Cold)], 3);

        let selected = router.select_miner(&model("bert"), 1).unwrap();
        assert_eq!(selected, addr(1)); // Hot has priority
    }

    #[test]
    fn test_select_miner_least_loaded() {
        let mut router = TaskRouter::new();

        // Both have model hot, but different loads
        router.update_miner_models(addr(1), vec![(model("bert"), ModelLayer::Hot)], 3);
        router.assign_task(&addr(1));
        router.assign_task(&addr(1));

        router.update_miner_models(addr(2), vec![(model("bert"), ModelLayer::Hot)], 3);

        let selected = router.select_miner(&model("bert"), 1).unwrap();
        assert_eq!(selected, addr(2)); // Less loaded
    }

    #[test]
    fn test_select_miner_tier_filter() {
        let mut router = TaskRouter::new();

        // Miner 1: tier 1 (too low)
        router.update_miner_models(addr(1), vec![(model("bert"), ModelLayer::Hot)], 1);

        // Miner 2: tier 3 (good enough)
        router.update_miner_models(addr(2), vec![(model("bert"), ModelLayer::Warm)], 3);

        let selected = router.select_miner(&model("bert"), 2).unwrap();
        assert_eq!(selected, addr(2));
    }

    #[test]
    fn test_select_miner_none() {
        let router = TaskRouter::new();
        assert!(router.select_miner(&model("bert"), 1).is_none());
    }

    #[test]
    fn test_prune_stale() {
        let mut router = TaskRouter::new();
        router.update_miner_models(addr(1), vec![], 3);

        // Not stale yet
        let now = now_ms();
        router.prune_stale(now, 60_000);
        assert_eq!(router.miner_count(), 1);

        // Simulate staleness
        if let Some(status) = router.miner_models.get_mut(&addr(1)) {
            status.last_updated = now.saturating_sub(120_000);
        }
        router.prune_stale(now, 60_000);
        assert_eq!(router.miner_count(), 0);
    }

    #[test]
    fn test_assign_complete_task() {
        let mut router = TaskRouter::new();
        router.update_miner_models(addr(1), vec![], 3);

        router.assign_task(&addr(1));
        assert_eq!(router.miner_models[&addr(1)].pending_tasks, 1);

        router.complete_task(&addr(1));
        assert_eq!(router.miner_models[&addr(1)].pending_tasks, 0);

        // Can't go below 0
        router.complete_task(&addr(1));
        assert_eq!(router.miner_models[&addr(1)].pending_tasks, 0);
    }
}
