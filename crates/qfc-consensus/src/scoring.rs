//! Contribution scoring for validators

use qfc_types::{
    ValidatorNode, WEIGHT_ACCURACY, WEIGHT_COMPUTE, WEIGHT_NETWORK, WEIGHT_REPUTATION,
    WEIGHT_STAKE, WEIGHT_STORAGE, WEIGHT_UPTIME,
};

/// Network state for dynamic weight adjustment
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkState {
    /// Normal operation
    Normal,
    /// Network is congested
    Congested,
    /// Storage is running low
    StorageShortage,
    /// Under attack
    UnderAttack,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self::Normal
    }
}

/// Calculate contribution score for a validator
pub fn calculate_contribution_score(
    validator: &ValidatorNode,
    total_stake: u128,
    total_hashrate: u64,
    total_storage_gb: u64,
    network_state: NetworkState,
) -> u64 {
    let mut score = 0.0f64;

    // 1. Stake contribution (30%)
    if total_stake > 0 {
        let stake_ratio = validator.stake.low_u128() as f64 / total_stake as f64;
        score += stake_ratio * WEIGHT_STAKE;
    }

    // 2. Compute contribution (20%) - optional
    if validator.provides_compute && total_hashrate > 0 {
        let compute_ratio = validator.hashrate as f64 / total_hashrate as f64;
        score += compute_ratio * WEIGHT_COMPUTE;
    }

    // 3. Uptime (15%)
    let uptime_score = validator.uptime_ratio();
    score += uptime_score * WEIGHT_UPTIME;

    // 4. Validation accuracy (15%)
    let accuracy_score = validator.accuracy_ratio();
    score += accuracy_score * WEIGHT_ACCURACY;

    // 5. Network service quality (10%)
    let latency_score = 1.0 / (1.0 + validator.avg_latency_ms as f64 / 100.0);
    let bandwidth_score = (validator.bandwidth_mbps as f64 / 1000.0).min(1.0);
    let service_score = latency_score * 0.6 + bandwidth_score * 0.4;
    score += service_score * WEIGHT_NETWORK;

    // 6. Storage contribution (5%)
    if total_storage_gb > 0 {
        let storage_ratio = validator.storage_provided_gb as f64 / total_storage_gb as f64;
        score += storage_ratio * WEIGHT_STORAGE;
    }

    // 7. Historical reputation (5%)
    let reputation_score = validator.reputation_ratio();
    score += reputation_score * WEIGHT_REPUTATION;

    // Apply network state multiplier
    let multiplier = get_network_multiplier(validator, network_state);
    score *= multiplier;

    // Convert to u64 (scale by 10^9 for precision)
    (score * 1_000_000_000.0) as u64
}

/// Get network state multiplier for dynamic weight adjustment
fn get_network_multiplier(validator: &ValidatorNode, state: NetworkState) -> f64 {
    match state {
        NetworkState::Normal => 1.0,

        NetworkState::Congested => {
            if validator.provides_compute {
                1.2 // +20% bonus for compute providers
            } else {
                1.0
            }
        }

        NetworkState::StorageShortage => {
            if validator.storage_provided_gb > 1000 {
                1.15 // +15% bonus for large storage providers
            } else {
                1.0
            }
        }

        NetworkState::UnderAttack => {
            if validator.reputation_ratio() > 0.9 {
                1.3 // +30% bonus for highly trusted nodes
            } else if validator.reputation_ratio() < 0.5 {
                0.7 // -30% penalty for low reputation
            } else {
                1.0
            }
        }
    }
}

/// Calculate selection probability from contribution score
pub fn score_to_probability(score: u64, total_score: u64) -> f64 {
    if total_score == 0 {
        return 0.0;
    }
    score as f64 / total_score as f64
}

/// Calculate total contribution score for all validators
pub fn total_contribution_score(validators: &[ValidatorNode]) -> u64 {
    validators.iter().map(|v| v.contribution_score).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_types::U256;

    fn create_test_validator(stake: u64) -> ValidatorNode {
        let mut v = ValidatorNode::default();
        v.stake = U256::from_u64(stake);
        v.uptime = 9500; // 95%
        v.accuracy = 9800; // 98%
        v.reputation = 8000; // 80%
        v
    }

    #[test]
    fn test_basic_scoring() {
        let validator = create_test_validator(10000);
        let score = calculate_contribution_score(
            &validator,
            100000, // total stake
            0,      // no hashrate
            0,      // no storage
            NetworkState::Normal,
        );

        assert!(score > 0);
    }

    #[test]
    fn test_higher_stake_higher_score() {
        let v1 = create_test_validator(10000);
        let v2 = create_test_validator(20000);

        let score1 = calculate_contribution_score(&v1, 100000, 0, 0, NetworkState::Normal);
        let score2 = calculate_contribution_score(&v2, 100000, 0, 0, NetworkState::Normal);

        assert!(score2 > score1);
    }

    #[test]
    fn test_network_state_bonus() {
        let mut validator = create_test_validator(10000);
        validator.provides_compute = true;
        validator.hashrate = 1000;

        let normal_score =
            calculate_contribution_score(&validator, 100000, 10000, 0, NetworkState::Normal);
        let congested_score =
            calculate_contribution_score(&validator, 100000, 10000, 0, NetworkState::Congested);

        assert!(congested_score > normal_score);
    }
}
