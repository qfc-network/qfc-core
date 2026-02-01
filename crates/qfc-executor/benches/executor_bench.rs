//! Benchmarks for QFC transaction execution
//!
//! Note: Full execution benchmarks are limited by the placeholder sender recovery
//! implementation. Once proper public key recovery is implemented, these benchmarks
//! can be expanded.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use qfc_crypto::{blake3_hash, Keypair};
use qfc_executor::Executor;
use qfc_state::StateDB;
use qfc_storage::Database;
use qfc_types::{Address, Transaction, U256};

/// Derive sender address the same way the executor does (from signature hash)
fn derive_sender_from_signature(tx: &Transaction) -> Address {
    let sender_hash = blake3_hash(tx.signature.as_bytes());
    Address::from_slice(&sender_hash.as_bytes()[12..32]).unwrap()
}

fn create_transfer_tx(keypair: &Keypair, nonce: u64) -> Transaction {
    let recipient = Address::from_slice(&[1u8; 20]).unwrap();
    let gas_price = U256::from_u64(1_000_000_000);
    let mut tx = Transaction::transfer(recipient, U256::from_u64(1000), nonce, gas_price);

    let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
    tx.signature = keypair.sign_hash(&tx_hash);
    tx
}

fn bench_validate_transaction(c: &mut Criterion) {
    // Create a funded state for a specific transaction
    let keypair = Keypair::generate();
    let tx = create_transfer_tx(&keypair, 0);

    let db = Database::open_temp().unwrap();
    let state = StateDB::new(db);
    let sender = derive_sender_from_signature(&tx);
    state
        .add_balance(&sender, U256::from_u128(u128::MAX))
        .expect("Failed to fund");

    let executor = Executor::testnet();

    c.bench_function("validate_transfer_tx", |b| {
        b.iter(|| executor.validate_transaction(black_box(&tx), black_box(&state)))
    });
}

fn bench_transaction_serialization(c: &mut Criterion) {
    let keypair = Keypair::generate();
    let tx = create_transfer_tx(&keypair, 0);

    let mut group = c.benchmark_group("tx_serialization");

    group.bench_function("serialize", |b| {
        b.iter(|| black_box(&tx).to_bytes_without_signature())
    });

    let bytes = tx.to_bytes_without_signature();
    group.bench_function("hash", |b| {
        b.iter(|| blake3_hash(black_box(&bytes)))
    });

    group.finish();
}

fn bench_signature_creation(c: &mut Criterion) {
    let keypair = Keypair::generate();
    let tx = create_transfer_tx(&keypair, 0);
    let tx_hash = blake3_hash(&tx.to_bytes_without_signature());

    c.bench_function("sign_tx_hash", |b| {
        b.iter(|| keypair.sign_hash(black_box(&tx_hash)))
    });
}

criterion_group!(
    benches,
    bench_validate_transaction,
    bench_transaction_serialization,
    bench_signature_creation,
);
criterion_main!(benches);
