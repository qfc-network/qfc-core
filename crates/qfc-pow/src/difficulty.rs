//! Difficulty adjustment algorithm

use qfc_types::{DifficultyConfig, U256};
use tracing::debug;

/// Adjust difficulty based on actual vs target proof count
///
/// Uses a bounded adjustment to prevent wild swings:
/// - If too many proofs: increase difficulty (lower target)
/// - If too few proofs: decrease difficulty (higher target)
/// - Max adjustment per epoch is configurable (default 10%)
pub fn adjust_difficulty(
    current_difficulty: &U256,
    actual_proofs: u64,
    config: &DifficultyConfig,
) -> U256 {
    let target = config.target_proofs_per_epoch;

    if actual_proofs == 0 {
        // No proofs at all - significantly decrease difficulty
        let new_difficulty = current_difficulty
            .saturating_mul(U256::from_u64(100 + config.max_adjustment_percent * 2))
            / U256::from_u64(100);
        return clamp_difficulty(&new_difficulty, config);
    }

    // Calculate ratio (scaled by 100 for precision)
    let ratio_scaled = (actual_proofs as u128 * 100) / target as u128;

    let new_difficulty = if ratio_scaled > 100 {
        // Too many proofs - increase difficulty (lower target value)
        let excess = ratio_scaled - 100;
        let adjustment = excess.min(config.max_adjustment_percent as u128);

        // Lower the target (harder)
        *current_difficulty * U256::from_u64(100) / U256::from_u64((100 + adjustment) as u64)
    } else if ratio_scaled < 100 {
        // Too few proofs - decrease difficulty (higher target value)
        let deficit = 100 - ratio_scaled;
        let adjustment = deficit.min(config.max_adjustment_percent as u128);

        // Raise the target (easier)
        *current_difficulty * U256::from_u64((100 + adjustment) as u64) / U256::from_u64(100)
    } else {
        // Perfect - no change
        *current_difficulty
    };

    let clamped = clamp_difficulty(&new_difficulty, config);

    debug!(
        "Difficulty adjusted: proofs={}, target={}, ratio={}%, new_difficulty={:?}",
        actual_proofs, target, ratio_scaled, clamped
    );

    clamped
}

/// Clamp difficulty within configured bounds
fn clamp_difficulty(difficulty: &U256, config: &DifficultyConfig) -> U256 {
    if *difficulty > config.min_difficulty {
        // Too easy, clamp to minimum difficulty (max target)
        config.min_difficulty
    } else if *difficulty < config.max_difficulty {
        // Too hard, clamp to maximum difficulty (min target)
        config.max_difficulty
    } else {
        *difficulty
    }
}

/// Calculate initial difficulty for a new network
///
/// Targets approximately `target_proofs_per_epoch` proofs with a modest hashrate
pub fn initial_difficulty(config: &DifficultyConfig) -> U256 {
    // Start with a moderate difficulty (24 bits of leading zeros)
    // This means ~16 million hashes per valid proof on average
    U256::from_be_bytes(&[
        0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ])
    .max(config.max_difficulty)
    .min(config.min_difficulty)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DifficultyConfig {
        DifficultyConfig {
            target_proofs_per_epoch: 10000,
            min_difficulty: U256::from_be_bytes(&[
                0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ]),
            max_difficulty: U256::from_be_bytes(&[
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ]),
            max_adjustment_percent: 10,
        }
    }

    #[test]
    fn test_adjust_difficulty_too_many_proofs() {
        let config = test_config();
        let current = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x0f, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);

        // 50% more proofs than target
        let new_difficulty = adjust_difficulty(&current, 15000, &config);

        // Difficulty should increase (target should decrease)
        assert!(new_difficulty < current);
    }

    #[test]
    fn test_adjust_difficulty_too_few_proofs() {
        let config = test_config();
        let current = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x0f, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);

        // 50% fewer proofs than target
        let new_difficulty = adjust_difficulty(&current, 5000, &config);

        // Difficulty should decrease (target should increase)
        assert!(new_difficulty > current);
    }

    #[test]
    fn test_adjust_difficulty_zero_proofs() {
        let config = test_config();
        let current = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x0f, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);

        // No proofs - should significantly decrease difficulty
        let new_difficulty = adjust_difficulty(&current, 0, &config);

        assert!(new_difficulty > current);
    }

    #[test]
    fn test_adjust_difficulty_perfect() {
        let config = test_config();
        let current = U256::from_be_bytes(&[
            0x00, 0x00, 0x00, 0x0f, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ]);

        // Exactly on target
        let new_difficulty = adjust_difficulty(&current, 10000, &config);

        assert_eq!(new_difficulty, current);
    }

    #[test]
    fn test_clamp_difficulty() {
        let config = test_config();

        // Test clamping to minimum difficulty (max target - easiest)
        let too_easy = U256::MAX;
        let clamped = clamp_difficulty(&too_easy, &config);
        assert_eq!(clamped, config.min_difficulty);

        // Test clamping to maximum difficulty (min target - hardest)
        let too_hard = U256::ZERO;
        let clamped = clamp_difficulty(&too_hard, &config);
        assert_eq!(clamped, config.max_difficulty);
    }

    #[test]
    fn test_initial_difficulty() {
        let config = test_config();
        let difficulty = initial_difficulty(&config);

        // Should be within bounds
        assert!(difficulty <= config.min_difficulty);
        assert!(difficulty >= config.max_difficulty);
    }
}
