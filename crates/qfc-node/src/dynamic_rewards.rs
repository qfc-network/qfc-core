//! Dynamic reward adjustment based on network conditions
//!
//! Adjusts fee burn rate, inference miner reward pool, and block reward
//! multiplier based on real-time network state (congestion, stake ratio,
//! active miner count).

use qfc_consensus::NetworkState;

/// Dynamic adjustment factors computed from network conditions
#[derive(Clone, Debug)]
pub struct RewardAdjustments {
    /// Fee burn rate multiplier (1.0 = normal, up to 2.0 during congestion)
    pub burn_rate_multiplier: f64,
    /// Block reward multiplier based on stake ratio (0.5 - 1.5)
    pub reward_multiplier: f64,
    /// Inference miner reward percent override (based on active miner count)
    pub inference_miner_percent: u64,
}

impl Default for RewardAdjustments {
    fn default() -> Self {
        Self {
            burn_rate_multiplier: 1.0,
            reward_multiplier: 1.0,
            inference_miner_percent: qfc_types::INFERENCE_MINERS_REWARD_PERCENT,
        }
    }
}

/// Compute dynamic reward adjustments from current network conditions.
///
/// # Parameters
/// - `network_state`: Current congestion level
/// - `total_staked`: Total QFC staked across all validators
/// - `circulating_supply`: Total circulating supply (for stake ratio)
/// - `active_miner_count`: Number of miners that submitted proofs recently
/// - `total_validator_count`: Number of active validators
pub fn compute_adjustments(
    network_state: NetworkState,
    total_staked: u128,
    circulating_supply: u128,
    active_miner_count: u64,
    total_validator_count: u64,
) -> RewardAdjustments {
    let mut adj = RewardAdjustments::default();

    // 1. Dynamic fee burn rate — increase burn during congestion
    //    Normal: 1.0x, Congested: 1.5x, UnderAttack: 2.0x
    adj.burn_rate_multiplier = match network_state {
        NetworkState::Normal => 1.0,
        NetworkState::Congested => 1.5,
        NetworkState::StorageShortage => 1.2,
        NetworkState::UnderAttack => 2.0,
    };

    // 2. Block reward multiplier based on stake ratio
    //    Target: 50% of supply staked
    //    Below target: boost rewards (up to 1.5x) to incentivize staking
    //    Above target: reduce rewards (down to 0.5x) to discourage over-concentration
    if circulating_supply > 0 {
        let stake_ratio = total_staked as f64 / circulating_supply as f64;
        let target_ratio = 0.50;

        if stake_ratio < target_ratio {
            // Below target: linear interpolation from 1.0 (at target) to 1.5 (at 0%)
            let deficit = (target_ratio - stake_ratio) / target_ratio;
            adj.reward_multiplier = 1.0 + deficit * 0.5;
        } else {
            // Above target: linear interpolation from 1.0 (at target) to 0.5 (at 100%)
            let excess = (stake_ratio - target_ratio) / (1.0 - target_ratio);
            adj.reward_multiplier = 1.0 - excess * 0.5;
        }

        // Clamp to [0.5, 1.5]
        adj.reward_multiplier = adj.reward_multiplier.clamp(0.5, 1.5);
    }

    // 3. Inference miner reward pool adjustment
    //    Scale the 15% pool based on miner participation:
    //    - 0 miners: 0% (all goes back to producer+voters)
    //    - 1-25% of validators mining: scale linearly from 5% to 15%
    //    - >25% of validators mining: full 15%
    if total_validator_count > 0 && active_miner_count > 0 {
        let miner_ratio = active_miner_count as f64 / total_validator_count as f64;
        let threshold = 0.25;

        if miner_ratio >= threshold {
            adj.inference_miner_percent = qfc_types::INFERENCE_MINERS_REWARD_PERCENT;
        } else {
            // Linear scale from 5% (at 0 miners) to 15% (at 25% participation)
            let scale = miner_ratio / threshold;
            adj.inference_miner_percent = 5 + (10.0 * scale) as u64;
        }
    } else {
        adj.inference_miner_percent = 0;
    }

    adj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normal_conditions() {
        let adj = compute_adjustments(
            NetworkState::Normal,
            50_000, // 50% staked
            100_000,
            10,
            40, // 25% mining
        );
        assert!((adj.burn_rate_multiplier - 1.0).abs() < 0.01);
        assert!((adj.reward_multiplier - 1.0).abs() < 0.01);
        assert_eq!(adj.inference_miner_percent, 15);
    }

    #[test]
    fn test_congested_increases_burn() {
        let adj = compute_adjustments(NetworkState::Congested, 50_000, 100_000, 10, 40);
        assert!((adj.burn_rate_multiplier - 1.5).abs() < 0.01);
    }

    #[test]
    fn test_under_attack_doubles_burn() {
        let adj = compute_adjustments(NetworkState::UnderAttack, 50_000, 100_000, 10, 40);
        assert!((adj.burn_rate_multiplier - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_low_stake_ratio_boosts_reward() {
        // Only 10% staked — should boost reward
        let adj = compute_adjustments(NetworkState::Normal, 10_000, 100_000, 10, 40);
        assert!(adj.reward_multiplier > 1.0);
        // At 10% staked: deficit = (0.5 - 0.1) / 0.5 = 0.8, mult = 1 + 0.8*0.5 = 1.4
        assert!((adj.reward_multiplier - 1.4).abs() < 0.01);
    }

    #[test]
    fn test_high_stake_ratio_reduces_reward() {
        // 80% staked — should reduce reward
        let adj = compute_adjustments(NetworkState::Normal, 80_000, 100_000, 10, 40);
        assert!(adj.reward_multiplier < 1.0);
        // At 80%: excess = (0.8 - 0.5) / 0.5 = 0.6, mult = 1 - 0.6*0.5 = 0.7
        assert!((adj.reward_multiplier - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_no_miners_zero_inference_pool() {
        let adj = compute_adjustments(NetworkState::Normal, 50_000, 100_000, 0, 40);
        assert_eq!(adj.inference_miner_percent, 0);
    }

    #[test]
    fn test_few_miners_scales_inference_pool() {
        // 5 out of 40 = 12.5% participation (below 25% threshold)
        let adj = compute_adjustments(NetworkState::Normal, 50_000, 100_000, 5, 40);
        // scale = 0.125 / 0.25 = 0.5, percent = 5 + 10*0.5 = 10
        assert_eq!(adj.inference_miner_percent, 10);
    }

    #[test]
    fn test_many_miners_full_inference_pool() {
        // 20 out of 40 = 50% participation (above 25% threshold)
        let adj = compute_adjustments(NetworkState::Normal, 50_000, 100_000, 20, 40);
        assert_eq!(adj.inference_miner_percent, 15);
    }

    #[test]
    fn test_reward_multiplier_clamped() {
        // 0% staked — should clamp to 1.5
        let adj = compute_adjustments(NetworkState::Normal, 0, 100_000, 10, 40);
        assert!((adj.reward_multiplier - 1.5).abs() < 0.01);

        // 100% staked — should clamp to 0.5
        let adj = compute_adjustments(NetworkState::Normal, 100_000, 100_000, 10, 40);
        assert!((adj.reward_multiplier - 0.5).abs() < 0.01);
    }
}
