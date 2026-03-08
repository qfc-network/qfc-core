//! Concurrent inference_score update tests
//!
//! Verifies that update_inference_score is atomic under concurrent access
//! from multiple threads (simulating multiple miners submitting proofs).

use std::sync::Arc;

use qfc_consensus::{ConsensusConfig, ConsensusEngine};
use qfc_types::{Address, PublicKey, ValidatorNode, U256};

fn make_validator(id: u8) -> ValidatorNode {
    ValidatorNode::new(
        Address::new([id; 20]),
        PublicKey::ZERO,
        U256::from_u64(10_000),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_inference_score_atomicity() {
    let engine = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));

    // Register 5 validators
    let validators: Vec<ValidatorNode> = (1..=5).map(make_validator).collect();
    engine.update_validators(validators);

    // Each validator gets 100 concurrent score updates of 1000 FLOPS each
    let mut handles = Vec::new();
    for v_id in 1..=5u8 {
        for _ in 0..100 {
            let engine = engine.clone();
            let addr = Address::new([v_id; 20]);
            handles.push(tokio::spawn(async move {
                engine.update_inference_score(&addr, 1000, 1);
            }));
        }
    }

    for h in handles {
        h.await.unwrap();
    }

    // Each validator should have exactly 100 * 1000 = 100,000 FLOPS
    let validators = engine.get_validators();
    for v in &validators {
        assert_eq!(
            v.inference_score, 100_000,
            "Validator {:?} expected 100000 FLOPS, got {}",
            v.address, v.inference_score
        );
        assert_eq!(
            v.tasks_completed, 100,
            "Validator {:?} expected 100 tasks, got {}",
            v.address, v.tasks_completed
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_concurrent_score_updates_many_validators() {
    let engine = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));

    // 50 validators
    let validators: Vec<ValidatorNode> = (0..50).map(|i| make_validator(i as u8)).collect();
    engine.update_validators(validators);

    // 200 concurrent updates spread across validators
    let mut handles = Vec::new();
    for i in 0..200u64 {
        let engine = engine.clone();
        let v_id = (i % 50) as u8;
        let addr = Address::new([v_id; 20]);
        let flops = (i + 1) * 100; // varying FLOPS
        handles.push(tokio::spawn(async move {
            engine.update_inference_score(&addr, flops, 1);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Verify totals: each validator gets 4 updates (200/50)
    // Validator i gets updates at i, i+50, i+100, i+150
    let validators = engine.get_validators();
    for (idx, v) in validators.iter().enumerate() {
        let expected_tasks = 4u64;
        assert_eq!(
            v.tasks_completed, expected_tasks,
            "Validator {} expected {} tasks", idx, expected_tasks
        );

        // Expected FLOPS: sum of (i+1)*100 for i in {idx, idx+50, idx+100, idx+150}
        let i = idx as u64;
        let expected_flops: u64 = [(i + 1), (i + 51), (i + 101), (i + 151)]
            .iter()
            .map(|x| x * 100)
            .sum();
        assert_eq!(
            v.inference_score, expected_flops,
            "Validator {} expected {} FLOPS, got {}",
            idx, expected_flops, v.inference_score
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_score_update_nonexistent_validator() {
    let engine = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));

    // Register only 1 validator
    engine.update_validators(vec![make_validator(1)]);

    // Update both existing and non-existing validators concurrently
    let mut handles = Vec::new();
    for v_id in 0..10u8 {
        let engine = engine.clone();
        let addr = Address::new([v_id; 20]);
        handles.push(tokio::spawn(async move {
            engine.update_inference_score(&addr, 500, 1);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    // Only validator 1 should have been updated
    let validators = engine.get_validators();
    assert_eq!(validators.len(), 1);
    assert_eq!(validators[0].inference_score, 500);
    assert_eq!(validators[0].tasks_completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_concurrent_score_saturating_add() {
    let engine = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
    engine.update_validators(vec![make_validator(1)]);

    // Update with very large values to test saturating_add
    let mut handles = Vec::new();
    for _ in 0..10 {
        let engine = engine.clone();
        let addr = Address::new([1; 20]);
        handles.push(tokio::spawn(async move {
            engine.update_inference_score(&addr, u64::MAX / 5, 1);
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let validators = engine.get_validators();
    // saturating_add should cap at u64::MAX
    assert_eq!(
        validators[0].inference_score,
        u64::MAX,
        "Should saturate at u64::MAX"
    );
    assert_eq!(validators[0].tasks_completed, 10);
}
