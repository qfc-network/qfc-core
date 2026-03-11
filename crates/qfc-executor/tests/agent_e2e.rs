//! E2E test suite for agent wallet scenarios (Issue #84)
//!
//! Tests the full lifecycle: register → fund → session key → inference,
//! plus edge cases: limits, revocation, daily reset, concurrent keys.

use qfc_executor::account_abstraction::UserOperation;
use qfc_executor::agent::{AgentError, AgentGuard, AgentPermission, AgentRecord};
use qfc_executor::agent_aa::AgentAA;
use qfc_types::Address;

fn test_address(byte: u8) -> Address {
    Address::from_slice(&[byte; 20]).unwrap()
}

fn make_agent(
    id: &str,
    owner: u8,
    permissions: u8,
    daily_limit: u128,
    max_per_tx: u128,
    deposit: u128,
) -> AgentRecord {
    AgentRecord {
        agent_id: id.to_string(),
        owner: test_address(owner),
        agent_address: test_address(owner + 0x80),
        permissions: vec![permissions],
        daily_limit,
        max_per_tx,
        deposit,
        spent_today: 0,
        last_reset: 1_000_000,
        nonce: 0,
        active: true,
    }
}

fn make_user_op(sender: Address, nonce: u64) -> UserOperation {
    UserOperation {
        sender,
        nonce,
        init_code: vec![],
        call_data: vec![1, 2, 3, 4],
        call_gas_limit: 100_000,
        verification_gas_limit: 50_000,
        pre_verification_gas: 21_000,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 100_000_000,
        paymaster_and_data: vec![],
        signature: vec![0xAA; 64],
    }
}

// ============ E2E Scenario Tests ============

/// Full lifecycle: register → fund → issue session key → perform operations → revoke
#[test]
fn test_e2e_full_agent_lifecycle() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64; // ~2023 timestamp

    // 1. Register agent
    let agent = make_agent("ai-bot-1", 0x01, 0xFF, 1_000_000, 200_000, 5_000_000);
    guard.register_agent(agent);
    assert!(guard.get_agent("ai-bot-1").is_some());

    // 2. Issue session key
    let sk_addr = test_address(0x55);
    let sk = guard
        .issue_session_key("ai-bot-1", sk_addr, vec![0xFF], 200_000, 3600, now)
        .unwrap();
    assert_eq!(sk.expires_at, now + 3600);

    // 3. Perform multiple operations
    for i in 0..5 {
        let agent_id = guard
            .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 100_000, i, now + i)
            .unwrap();
        assert_eq!(agent_id, "ai-bot-1");
        guard.record_spend("ai-bot-1", 100_000, now + i).unwrap();
        guard.increment_session_key_nonce(&sk_addr).unwrap();
    }

    // Verify spending tracked correctly
    let agent = guard.get_agent("ai-bot-1").unwrap();
    assert_eq!(agent.spent_today, 500_000);
    assert_eq!(agent.deposit, 5_000_000 - 500_000);
    assert_eq!(agent.nonce, 5);

    // 4. Revoke agent
    guard.revoke_agent("ai-bot-1").unwrap();
    assert!(!guard.get_agent("ai-bot-1").unwrap().active);

    // 5. Verify session key is also revoked
    let result = guard.validate_session_key(&sk_addr, 5, now + 100);
    assert!(matches!(result, Err(AgentError::SessionKeyRevoked(_))));
}

/// Test per-tx limit enforcement across session key and agent levels
#[test]
fn test_e2e_per_tx_limit_enforcement() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    // Agent with max_per_tx = 200_000
    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 10_000_000, 200_000, 50_000_000,
    ));

    // Session key with max_per_tx = 100_000 (more restrictive than agent)
    let sk_addr = test_address(0x55);
    guard
        .issue_session_key("bot", sk_addr, vec![0xFF], 100_000, 3600, now)
        .unwrap();

    // Within session key limit: OK
    assert!(guard
        .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 80_000, 0, now)
        .is_ok());

    // Exceeds session key limit but within agent limit
    let result =
        guard.validate_agent_operation(&sk_addr, AgentPermission::Transfer, 150_000, 0, now);
    assert!(matches!(result, Err(AgentError::PerTxLimitExceeded { .. })));
}

/// Test daily limit enforcement and reset
#[test]
fn test_e2e_daily_limit_and_reset() {
    let mut guard = AgentGuard::new();
    let day_start = 1_700_000_000u64;
    let day_secs = 86_400u64;

    guard.register_agent(make_agent("bot", 0x01, 0xFF, 500_000, 200_000, 50_000_000));

    let sk_addr = test_address(0x55);
    guard
        .issue_session_key("bot", sk_addr, vec![0xFF], 200_000, day_secs * 2, day_start)
        .unwrap();

    // Spend up to daily limit
    for i in 0..2 {
        guard
            .validate_agent_operation(
                &sk_addr,
                AgentPermission::Transfer,
                200_000,
                i,
                day_start + i,
            )
            .unwrap();
        guard.record_spend("bot", 200_000, day_start + i).unwrap();
        guard.increment_session_key_nonce(&sk_addr).unwrap();
    }

    // Third transaction should fail (400_000 + 200_000 > 500_000)
    let result = guard.validate_agent_operation(
        &sk_addr,
        AgentPermission::Transfer,
        200_000,
        2,
        day_start + 100,
    );
    assert!(matches!(result, Err(AgentError::DailyLimitExceeded { .. })));

    // After day boundary, limit should reset
    let next_day = day_start + day_secs + 1;
    let result =
        guard.validate_agent_operation(&sk_addr, AgentPermission::Transfer, 200_000, 2, next_day);
    assert!(result.is_ok());
}

/// Test session key rotation
#[test]
fn test_e2e_session_key_rotation() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 1_000_000, 500_000, 50_000_000,
    ));

    // Issue initial key
    let old_addr = test_address(0x55);
    guard
        .issue_session_key("bot", old_addr, vec![0xFF], 200_000, 3600, now)
        .unwrap();

    // Use it once
    guard
        .validate_agent_operation(&old_addr, AgentPermission::Transfer, 100_000, 0, now + 100)
        .unwrap();
    guard.record_spend("bot", 100_000, now + 100).unwrap();
    guard.increment_session_key_nonce(&old_addr).unwrap();

    // Rotate to new key
    let new_addr = test_address(0x66);
    let _new_sk = guard
        .rotate_session_key(&old_addr, new_addr, 7200, now + 200)
        .unwrap();

    // Old key should be revoked
    let result = guard.validate_session_key(&old_addr, 1, now + 300);
    assert!(matches!(result, Err(AgentError::SessionKeyRevoked(_))));

    // New key should work with nonce=0
    assert!(guard
        .validate_agent_operation(&new_addr, AgentPermission::Transfer, 100_000, 0, now + 300)
        .is_ok());
}

/// Test multiple concurrent session keys for one agent
#[test]
fn test_e2e_concurrent_session_keys() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 2_000_000, 500_000, 50_000_000,
    ));

    // Issue 3 session keys with different permissions
    let sk_transfer = test_address(0x55);
    let sk_inference = test_address(0x66);
    let sk_all = test_address(0x77);

    guard
        .issue_session_key(
            "bot",
            sk_transfer,
            vec![AgentPermission::Transfer as u8],
            200_000,
            3600,
            now,
        )
        .unwrap();
    guard
        .issue_session_key(
            "bot",
            sk_inference,
            vec![AgentPermission::InferenceTask as u8],
            500_000,
            3600,
            now,
        )
        .unwrap();
    guard
        .issue_session_key("bot", sk_all, vec![0xFF], 500_000, 3600, now)
        .unwrap();

    // Transfer key can transfer but not submit inference
    assert!(guard
        .validate_agent_operation(&sk_transfer, AgentPermission::Transfer, 100_000, 0, now)
        .is_ok());
    assert!(matches!(
        guard.validate_agent_operation(
            &sk_transfer,
            AgentPermission::InferenceTask,
            100_000,
            0,
            now
        ),
        Err(AgentError::PermissionDenied(_))
    ));

    // Inference key can submit inference but not transfer
    assert!(guard
        .validate_agent_operation(
            &sk_inference,
            AgentPermission::InferenceTask,
            100_000,
            0,
            now
        )
        .is_ok());
    assert!(matches!(
        guard.validate_agent_operation(&sk_inference, AgentPermission::Transfer, 100_000, 0, now),
        Err(AgentError::PermissionDenied(_))
    ));

    // All-permission key can do both
    assert!(guard
        .validate_agent_operation(&sk_all, AgentPermission::Transfer, 100_000, 0, now)
        .is_ok());
    assert!(guard
        .validate_agent_operation(&sk_all, AgentPermission::InferenceTask, 100_000, 0, now)
        .is_ok());
}

/// Test session key TTL enforcement
#[test]
fn test_e2e_session_key_ttl() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 1_000_000, 500_000, 50_000_000,
    ));

    let sk_addr = test_address(0x55);
    guard
        .issue_session_key("bot", sk_addr, vec![0xFF], 500_000, 300, now) // 5 min TTL
        .unwrap();

    // Valid during TTL
    assert!(guard
        .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 100_000, 0, now + 100)
        .is_ok());

    // Expired after TTL
    let result =
        guard.validate_agent_operation(&sk_addr, AgentPermission::Transfer, 100_000, 0, now + 301);
    assert!(matches!(result, Err(AgentError::SessionKeyExpired { .. })));
}

/// Test deposit depletion
#[test]
fn test_e2e_deposit_depletion() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    // Agent with small deposit (high per-tx limit to test deposit exhaustion)
    guard.register_agent(make_agent(
        "bot",
        0x01,
        0xFF,
        u128::MAX,
        1_000_000,
        1_000_000,
    ));

    let sk_addr = test_address(0x55);
    guard
        .issue_session_key("bot", sk_addr, vec![0xFF], 1_000_000, 3600, now)
        .unwrap();

    // Spend most of deposit
    guard
        .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 900_000, 0, now)
        .unwrap();
    guard.record_spend("bot", 900_000, now).unwrap();
    guard.increment_session_key_nonce(&sk_addr).unwrap();

    // Try to spend more than remaining deposit (100_000 remaining)
    let result =
        guard.validate_agent_operation(&sk_addr, AgentPermission::Transfer, 200_000, 1, now + 1);
    assert!(matches!(
        result,
        Err(AgentError::InsufficientDeposit { .. })
    ));
}

/// Test AgentAA integration: full flow with UserOp submission and execution
#[test]
fn test_e2e_agent_aa_full_flow() {
    let mut aa = AgentAA::new(9000);
    let now = 1_700_000_000u64;

    // Register agent with large deposit (must cover UserOp gas: 171_000 * 1 Gwei = 171B wei)
    let agent = make_agent(
        "ai-agent",
        0x01,
        0xFF,
        1_000_000_000_000_000_000,
        1_000_000_000_000_000_000,
        1_000_000_000_000_000_000,
    );
    aa.register_agent(agent);

    // Issue session key
    let sk_addr = test_address(0x55);
    aa.guard
        .issue_session_key(
            "ai-agent",
            sk_addr,
            vec![0xFF],
            1_000_000_000_000_000_000,
            3600,
            now,
        )
        .unwrap();

    // Submit UserOp
    let user_op = make_user_op(test_address(0x81), 0);
    let _op_hash = aa
        .submit_agent_user_op(
            user_op,
            &sk_addr,
            AgentPermission::ContractCall,
            1_000_000,
            0,
            now + 100,
        )
        .unwrap();

    assert_eq!(aa.user_op_pool.size(), 1);

    // Execute pending ops (simulates block producer bundling)
    let results = aa.execute_pending_ops(10);
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_ok());
    assert_eq!(aa.user_op_pool.size(), 0);

    // Verify spend was recorded
    let agent = aa.guard.get_agent("ai-agent").unwrap();
    assert_eq!(agent.spent_today, 1_000_000);
}

/// Test AgentAA: revoke agent cancels pending ops authorization
#[test]
fn test_e2e_agent_aa_revoke_blocks_new_ops() {
    let mut aa = AgentAA::new(9000);
    let now = 1_700_000_000u64;

    aa.register_agent(make_agent(
        "bot",
        0x01,
        0xFF,
        1_000_000_000_000_000_000,
        1_000_000_000_000_000_000,
        1_000_000_000_000_000_000,
    ));

    let sk_addr = test_address(0x55);
    aa.guard
        .issue_session_key(
            "bot",
            sk_addr,
            vec![0xFF],
            1_000_000_000_000_000_000,
            3600,
            now,
        )
        .unwrap();

    // Submit one op successfully
    let user_op1 = make_user_op(test_address(0x81), 0);
    aa.submit_agent_user_op(
        user_op1,
        &sk_addr,
        AgentPermission::Transfer,
        100_000,
        0,
        now + 1,
    )
    .unwrap();

    // Revoke agent
    aa.guard.revoke_agent("bot").unwrap();

    // New submission should fail
    let user_op2 = make_user_op(test_address(0x81), 1);
    let result = aa.submit_agent_user_op(
        user_op2,
        &sk_addr,
        AgentPermission::Transfer,
        100_000,
        1,
        now + 2,
    );
    assert!(result.is_err());
}

/// Test nonce replay protection
#[test]
fn test_e2e_nonce_replay_protection() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 1_000_000, 500_000, 50_000_000,
    ));

    let sk_addr = test_address(0x55);
    guard
        .issue_session_key("bot", sk_addr, vec![0xFF], 500_000, 3600, now)
        .unwrap();

    // Use nonce 0
    guard
        .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 100_000, 0, now)
        .unwrap();
    guard.record_spend("bot", 100_000, now).unwrap();
    guard.increment_session_key_nonce(&sk_addr).unwrap();

    // Replay with nonce 0 should fail
    let result = guard.validate_agent_operation(
        &sk_addr,
        AgentPermission::Transfer,
        100_000,
        0, // replayed nonce
        now + 1,
    );
    assert!(matches!(
        result,
        Err(AgentError::SessionKeyNonceMismatch { .. })
    ));

    // Nonce 1 should work
    assert!(guard
        .validate_agent_operation(&sk_addr, AgentPermission::Transfer, 100_000, 1, now + 1)
        .is_ok());
}

/// Test multi-agent isolation
#[test]
fn test_e2e_multi_agent_isolation() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    // Two agents with different owners
    guard.register_agent(make_agent("bot-a", 0x01, 0xFF, 500_000, 200_000, 1_000_000));
    guard.register_agent(make_agent("bot-b", 0x02, 0xFF, 500_000, 200_000, 1_000_000));

    let sk_a = test_address(0x55);
    let sk_b = test_address(0x66);

    guard
        .issue_session_key("bot-a", sk_a, vec![0xFF], 200_000, 3600, now)
        .unwrap();
    guard
        .issue_session_key("bot-b", sk_b, vec![0xFF], 200_000, 3600, now)
        .unwrap();

    // Spend on agent A
    guard
        .validate_agent_operation(&sk_a, AgentPermission::Transfer, 200_000, 0, now)
        .unwrap();
    guard.record_spend("bot-a", 200_000, now).unwrap();
    guard.increment_session_key_nonce(&sk_a).unwrap();

    // Agent B should be unaffected
    let agent_b = guard.get_agent("bot-b").unwrap();
    assert_eq!(agent_b.spent_today, 0);
    assert_eq!(agent_b.deposit, 1_000_000);

    // Agent A should have spent
    let agent_a = guard.get_agent("bot-a").unwrap();
    assert_eq!(agent_a.spent_today, 200_000);
    assert_eq!(agent_a.deposit, 800_000);
}

/// Test session key cannot exceed agent permissions
#[test]
fn test_e2e_session_key_permission_subset() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    // Agent with transfer + inference only
    let perms = AgentPermission::Transfer as u8 | AgentPermission::InferenceTask as u8;
    guard.register_agent(make_agent(
        "bot", 0x01, perms, 1_000_000, 500_000, 50_000_000,
    ));

    let sk_addr = test_address(0x55);

    // Try to issue key with staking (not in agent's permissions)
    let result = guard.issue_session_key(
        "bot",
        sk_addr,
        vec![AgentPermission::Staking as u8],
        500_000,
        3600,
        now,
    );
    assert!(matches!(result, Err(AgentError::PermissionDenied(_))));

    // Issue key with valid subset
    let result = guard.issue_session_key(
        "bot",
        sk_addr,
        vec![AgentPermission::Transfer as u8],
        500_000,
        3600,
        now,
    );
    assert!(result.is_ok());
}

/// Test session key cleanup after pruning
#[test]
fn test_e2e_session_key_pruning() {
    let mut guard = AgentGuard::new();
    let now = 1_700_000_000u64;

    guard.register_agent(make_agent(
        "bot", 0x01, 0xFF, 1_000_000, 500_000, 50_000_000,
    ));

    // Issue keys with different TTLs
    for i in 0..5u8 {
        let addr = Address::from_slice(&[i + 50; 20]).unwrap();
        let ttl = (i as u64 + 1) * 100; // 100, 200, 300, 400, 500 seconds
        guard
            .issue_session_key("bot", addr, vec![0xFF], 500_000, ttl, now)
            .unwrap();
    }

    assert_eq!(guard.list_session_keys("bot", now).len(), 5);

    // After 350 seconds, 3 keys should have expired
    let pruned = guard.prune_session_keys(now + 350);
    assert_eq!(pruned, 3);
    assert_eq!(guard.list_session_keys("bot", now + 350).len(), 2);
}
