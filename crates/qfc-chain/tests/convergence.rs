//! Cross-node convergence tests for the consolidated consensus fix
//! (docs/adr/0012-consensus-convergence-fixes.md, spec: FIX-SPEC-convergence).
//!
//! These are the spec's "required tests" 1–7 and 9: shared deterministic
//! state transition (D7), self-contained validation (D8), schedule
//! enforcement (D9), fork choice / reorg / import hardening (D10), and
//! non-validator imports.

use qfc_chain::{Chain, ChainConfig, GenesisConfig, GenesisValidator};
use qfc_consensus::{ConsensusConfig, ConsensusEngine};
use qfc_crypto::{address_from_public_key, blake3_hash, VrfKeypair};
use qfc_storage::Database;
use qfc_types::{Block, Hash, BLOCK_INTERVAL_MS, U256};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------- helpers

const VALIDATORS: usize = 4;

/// Deterministic validator keypairs for the test net.
fn keys() -> Vec<VrfKeypair> {
    (1..=VALIDATORS as u8)
        .map(|i| VrfKeypair::from_secret_bytes(&[i * 0x11; 32]).unwrap())
        .collect()
}

/// A genesis config whose validator set we hold ALL the keys for.
fn test_genesis() -> GenesisConfig {
    let keys = keys();
    let validators = keys
        .iter()
        .map(|k| {
            let addr = address_from_public_key(&k.public_key());
            GenesisValidator {
                address: format!("0x{}", hex::encode(addr.as_bytes())),
                public_key: hex::encode(k.public_key().as_bytes()),
                stake: "1000000".to_string(),
            }
        })
        .collect();

    GenesisConfig {
        chain_id: qfc_types::DEFAULT_CHAIN_ID,
        timestamp: 0,
        extra_data: b"convergence-test".to_vec(),
        alloc: HashMap::new(),
        validators,
    }
}

/// A fresh chain over a temp DB with a NON-validator engine (like an
/// observer/full node). Production is driven by separate validator engines.
fn make_chain() -> Arc<Chain> {
    let db = Database::open_temp().unwrap();
    let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
    Arc::new(
        Chain::new(
            db,
            ChainConfig {
                chain_id: qfc_types::DEFAULT_CHAIN_ID,
                genesis: test_genesis(),
            },
            consensus,
        )
        .unwrap(),
    )
}

/// One producing engine per validator key, anchored to the SAME genesis
/// seed and validator set as `chain` — exactly what each validator node's
/// engine holds after boot.
fn validator_engines(chain: &Chain) -> Vec<ConsensusEngine> {
    let mut seed = [0u8; 32];
    seed.copy_from_slice(chain.genesis_hash().unwrap().as_bytes());
    let validator_set = chain.get_validators();

    keys()
        .into_iter()
        .map(|key| {
            let addr = address_from_public_key(&key.public_key());
            let engine = ConsensusEngine::new_validator(ConsensusConfig::default(), key, addr);
            engine.set_genesis_seed(seed);
            engine.update_validators(validator_set.clone());
            engine
        })
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A base slot comfortably in the past (historical for every node), with
/// room for long branches of consecutive slots.
fn past_slot_base() -> u64 {
    (now_ms() - 6 * 3600 * 1000) / BLOCK_INTERVAL_MS
}

/// A mid-slot timestamp (2s in), away from both boundary-tolerance windows.
fn slot_timestamp(slot: u64) -> u64 {
    slot * BLOCK_INTERVAL_MS + 2000
}

/// Seal an empty block for `slot` on `parent`, signed by the slot's ELECTED
/// leader, with the state root produced by the shared deterministic
/// transition. `chain` is only used for execution (state is root-addressed).
fn seal_at_slot(chain: &Chain, engines: &[ConsensusEngine], parent: &Block, slot: u64) -> Block {
    let leader = engines[0].select_producer(slot).expect("leader");
    let idx = engines
        .iter()
        .position(|e| e.our_address() == Some(leader))
        .expect("leader engine");
    let timestamp = slot_timestamp(slot);

    let outcome = chain
        .execute_at(
            parent.state_root(),
            parent.number() + 1,
            timestamp,
            &leader,
            &[],
        )
        .unwrap();

    engines[idx]
        .produce_block(
            parent,
            vec![],
            outcome.receipts,
            outcome.state_root,
            outcome.gas_used,
            vec![],
            timestamp,
        )
        .unwrap()
}

fn block_hash(block: &Block) -> Hash {
    blake3_hash(&block.header_bytes())
}

fn block_reward() -> U256 {
    qfc_types::block_reward_for_year(0)
}

// ---------------------------------------------------------------- tests

/// REQUIRED TEST 1 — THE D7 test: node A produces blocks (rewards included
/// in the state root), node B imports them, roots match. Before the fix,
/// every peer block failed "State root mismatch" because rewards were never
/// replayed on import.
///
/// Also REQUIRED TEST 9: node B runs a NON-validator engine that never
/// started an epoch — validation must not depend on `current_epoch`.
#[tokio::test]
async fn cross_node_import_replays_rewards() {
    let chain_a = make_chain();
    let chain_b = make_chain();
    let engines = validator_engines(&chain_a);
    assert_eq!(chain_a.genesis_hash(), chain_b.genesis_hash());

    let base = past_slot_base();
    let mut parent = chain_a.head().unwrap().block;
    let mut produced = Vec::new();
    for i in 0..3u64 {
        let block = seal_at_slot(&chain_a, &engines, &parent, base + i);
        chain_a.import_block(block.clone()).await.unwrap();
        parent = block.clone();
        produced.push(block);
    }

    // Node B (non-validator, epoch state untouched) imports A's blocks.
    for block in &produced {
        chain_b.import_block(block.clone()).await.unwrap();
    }

    assert_eq!(chain_b.block_number(), 3);
    assert_eq!(chain_a.state_root(), chain_b.state_root());
    assert_eq!(
        chain_a.head().unwrap().hash,
        chain_b.head().unwrap().hash,
        "nodes must converge on the same head"
    );

    // Rewards actually landed and replayed identically: every producer's
    // balance is a multiple of the block reward and equal on both nodes.
    for block in &produced {
        let producer = block.producer();
        let bal_a = chain_a.get_balance(&producer).unwrap();
        let bal_b = chain_b.get_balance(&producer).unwrap();
        assert_eq!(bal_a, bal_b);
        assert!(bal_a >= block_reward(), "producer reward missing");
    }
}

/// REQUIRED TEST 2 — historical import: blocks several epochs old must
/// validate (VRF is verified against the seed derived from the BLOCK's
/// epoch, never the importer's current epoch).
#[tokio::test]
async fn historical_blocks_import_across_epochs() {
    let chain_a = make_chain();
    let chain_b = make_chain();
    let engines = validator_engines(&chain_a);

    // Three blocks spread ~1 hour apart in the past: every pair is separated
    // by hundreds of epochs, and all are thousands of epochs older than the
    // importer's wall-clock epoch.
    let base = past_slot_base();
    let mut parent = chain_a.head().unwrap().block;
    let mut produced = Vec::new();
    for i in 0..3u64 {
        let slot = base + i * 720; // 720 slots = 1h apart
        let block = seal_at_slot(&chain_a, &engines, &parent, slot);
        chain_a.import_block(block.clone()).await.unwrap();
        parent = block.clone();
        produced.push(block);
    }

    for block in &produced {
        chain_b
            .import_block(block.clone())
            .await
            .expect("historical block must validate against its own epoch seed");
    }
    assert_eq!(chain_b.block_number(), 3);
}

/// REQUIRED TEST 3 — leader enforcement: a block signed by a validator that
/// is NOT the elected leader of its slot is rejected.
#[tokio::test]
async fn wrong_slot_producer_rejected() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;

    let slot = past_slot_base();
    let leader = engines[0].select_producer(slot).unwrap();
    let wrong_idx = engines
        .iter()
        .position(|e| e.our_address() != Some(leader))
        .unwrap();
    let wrong_addr = engines[wrong_idx].our_address().unwrap();
    let timestamp = slot_timestamp(slot);

    let outcome = chain
        .execute_at(genesis.state_root(), 1, timestamp, &wrong_addr, &[])
        .unwrap();
    let block = engines[wrong_idx]
        .produce_block(
            &genesis,
            vec![],
            outcome.receipts,
            outcome.state_root,
            outcome.gas_used,
            vec![],
            timestamp,
        )
        .unwrap();

    let err = chain.import_block(block).await.unwrap_err();
    assert!(
        err.to_string().contains("producer"),
        "expected InvalidProducer, got: {err}"
    );
    assert_eq!(
        chain.block_number(),
        0,
        "rejected block must not advance head"
    );
}

/// REQUIRED TEST 5a — fork choice adopts the longer branch (reorg) and
/// rewrites the canonical index; state root follows the new branch.
#[tokio::test]
async fn reorg_adopts_longer_branch() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;
    let base = past_slot_base();

    // Canonical: one block at slot base.
    let b1 = seal_at_slot(&chain, &engines, &genesis, base);
    chain.import_block(b1.clone()).await.unwrap();
    assert_eq!(chain.block_number(), 1);

    // Competing branch from genesis, two blocks on later slots.
    let c1 = seal_at_slot(&chain, &engines, &genesis, base + 2);
    let c2 = seal_at_slot(&chain, &engines, &c1, base + 3);

    // c1 lands as a side block (same height, hash may or may not win —
    // either way c2 must force the reorg).
    let _ = chain.import_block(c1.clone()).await.unwrap();
    chain.import_block(c2.clone()).await.unwrap();

    assert_eq!(chain.block_number(), 2, "longer branch must win");
    assert_eq!(chain.head().unwrap().hash, block_hash(&c2));
    assert_eq!(chain.state_root(), c2.state_root());

    // Canonical index rewritten: height 1 now resolves to c1.
    let canonical_1 = chain.get_block_by_number(1).unwrap().unwrap();
    assert_eq!(block_hash(&canonical_1), block_hash(&c1));

    // The displaced block remains fetchable by hash (branch store).
    assert!(chain.get_block_by_hash(&block_hash(&b1)).unwrap().is_some());
}

/// REQUIRED TEST 5b — same-height tie-break: lowest hash wins, and the
/// outcome is the same regardless of arrival order (boundary races converge).
#[tokio::test]
async fn same_height_tie_breaks_to_lowest_hash() {
    let chain_x = make_chain();
    let chain_y = make_chain();
    let engines = validator_engines(&chain_x);
    let genesis = chain_x.head().unwrap().block;
    let base = past_slot_base();

    let a = seal_at_slot(&chain_x, &engines, &genesis, base);
    let b = seal_at_slot(&chain_x, &engines, &genesis, base + 1);
    assert_ne!(block_hash(&a), block_hash(&b));
    let winner = block_hash(&a).min(block_hash(&b));

    // Node X sees a then b; node Y sees b then a.
    chain_x.import_block(a.clone()).await.unwrap();
    chain_x.import_block(b.clone()).await.unwrap();
    chain_y.import_block(b.clone()).await.unwrap();
    chain_y.import_block(a.clone()).await.unwrap();

    assert_eq!(chain_x.head().unwrap().hash, winner);
    assert_eq!(chain_y.head().unwrap().hash, winner);
    assert_eq!(chain_x.state_root(), chain_y.state_root());
}

/// REQUIRED TEST 5c — a reorg never crosses the finalized (height, hash).
#[tokio::test]
async fn reorg_refuses_to_cross_finalized() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;
    let base = past_slot_base();

    let b1 = seal_at_slot(&chain, &engines, &genesis, base);
    let b2 = seal_at_slot(&chain, &engines, &b1, base + 1);
    chain.import_block(b1.clone()).await.unwrap();
    chain.import_block(b2.clone()).await.unwrap();

    // Finalize height 1.
    chain.record_finalized(1, block_hash(&b1));

    // Longer competing branch diverging BELOW the finalized height.
    let c1 = seal_at_slot(&chain, &engines, &genesis, base + 4);
    let c2 = seal_at_slot(&chain, &engines, &c1, base + 5);
    let c3 = seal_at_slot(&chain, &engines, &c2, base + 6);
    for c in [&c1, &c2, &c3] {
        // Valid blocks; stored on the side branch. The reorg attempt (at c3,
        // number 3 > head 2) must be refused by the finality guard.
        chain.import_block(c.clone()).await.unwrap();
    }

    assert_eq!(chain.head().unwrap().hash, block_hash(&b2));
    assert_eq!(chain.block_number(), 2);
    let canonical_1 = chain.get_block_by_number(1).unwrap().unwrap();
    assert_eq!(block_hash(&canonical_1), block_hash(&b1));
}

/// REQUIRED TEST 5d — reorg depth cap: a fork deeper than MAX_REORG_DEPTH
/// below the tip is never adopted, however long it grows.
///
/// Canonical chain: 65 blocks. Side branch diverging at genesis: 66 blocks.
/// The winning-length reorg would need to walk 66 > 64 blocks back — refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reorg_depth_cap_respected() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;
    let base = past_slot_base();

    let canonical_len = qfc_chain::MAX_REORG_DEPTH + 1; // 65
    let mut parent = genesis.clone();
    let mut canonical_head = Hash::ZERO;
    for i in 0..canonical_len {
        let b = seal_at_slot(&chain, &engines, &parent, base + i);
        canonical_head = block_hash(&b);
        chain.import_block(b.clone()).await.unwrap();
        parent = b;
    }
    assert_eq!(chain.block_number(), canonical_len);

    // Side branch from genesis, one block longer, on disjoint slots.
    let side_base = base + canonical_len + 10;
    let mut parent = genesis;
    for i in 0..(canonical_len + 1) {
        let b = seal_at_slot(&chain, &engines, &parent, side_base + i);
        chain.import_block(b.clone()).await.unwrap();
        parent = b;
    }

    // Head unchanged: adopting the longer branch would exceed the depth cap.
    assert_eq!(chain.block_number(), canonical_len);
    assert_eq!(chain.head().unwrap().hash, canonical_head);
}

/// REQUIRED TEST 6 — a failed import leaves the live state root untouched
/// (no state poisoning): execution happens on a scratch state and nothing
/// commits on mismatch.
#[tokio::test]
async fn failed_import_leaves_live_state_untouched() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;

    let mut block = seal_at_slot(&chain, &engines, &genesis, past_slot_base());
    // Tamper the claimed state root: validation passes, execution comparison
    // must fail WITHOUT committing anything.
    block.header.state_root = Hash::new([0xde; 32]);
    let tampered_hash = block_hash(&block);

    let root_before = chain.state_root();
    let head_before = chain.head().unwrap().hash;

    let err = chain.import_block(block).await.unwrap_err();
    assert!(
        err.to_string().contains("State root mismatch"),
        "unexpected error: {err}"
    );

    assert_eq!(chain.state_root(), root_before, "live state root poisoned");
    assert_eq!(chain.head().unwrap().hash, head_before);
    assert_eq!(chain.block_number(), 0);
    assert!(
        chain.get_block_by_hash(&tampered_hash).unwrap().is_none(),
        "rejected block must not be stored"
    );
}

/// REQUIRED TEST 7 — concurrent import calls serialize on the chain-wide
/// import lock: N tasks importing a 10-block chain in arbitrary order end
/// with a consistent head and no interleaved-commit corruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_imports_serialize() {
    let chain_a = make_chain();
    let chain_b = make_chain();
    let engines = validator_engines(&chain_a);
    let base = past_slot_base();

    let mut parent = chain_a.head().unwrap().block;
    let mut blocks = Vec::new();
    for i in 0..10u64 {
        let b = seal_at_slot(&chain_a, &engines, &parent, base + i);
        chain_a.import_block(b.clone()).await.unwrap();
        parent = b.clone();
        blocks.push(b);
    }

    // Hammer node B from 10 concurrent tasks, one block each, retrying until
    // the parent has landed (InvalidParent) or the block is in.
    let mut handles = Vec::new();
    for block in blocks {
        let chain = chain_b.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match chain.import_block(block.clone()).await {
                    Ok(_) => break,
                    Err(qfc_chain::ChainError::BlockAlreadyKnown) => break,
                    Err(qfc_chain::ChainError::InvalidParent { .. }) => {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Err(e) => panic!("unexpected import failure: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(chain_b.block_number(), 10);
    assert_eq!(chain_b.state_root(), chain_a.state_root());
    assert_eq!(chain_b.head().unwrap().hash, chain_a.head().unwrap().hash);

    // Canonical index is complete and consistent.
    for n in 1..=10u64 {
        let blk = chain_b.get_block_by_number(n).unwrap().unwrap();
        assert_eq!(blk.number(), n);
    }
}

/// Producer self-validation: `store_produced_block` runs the same
/// validate+execute path as import, so a produced block that peers would
/// reject (wrong claimed root) is refused locally too.
#[tokio::test]
async fn store_produced_block_self_validates() {
    let chain = make_chain();
    let engines = validator_engines(&chain);
    let genesis = chain.head().unwrap().block;

    let mut block = seal_at_slot(&chain, &engines, &genesis, past_slot_base());
    block.header.state_root = Hash::new([0xaa; 32]);

    let err = chain.store_produced_block(&block).await.unwrap_err();
    assert!(err.to_string().contains("state root mismatch"), "{err}");
    assert_eq!(chain.block_number(), 0);

    // And a correct block stores fine through the same path.
    let good = seal_at_slot(&chain, &engines, &genesis, past_slot_base() + 1);
    chain.store_produced_block(&good).await.unwrap();
    assert_eq!(chain.block_number(), 1);
    assert_eq!(chain.state_root(), good.state_root());
}
