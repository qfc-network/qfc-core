//! Benchmarks for QFC transaction execution

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use qfc_crypto::{address_from_public_key, blake3_hash, Keypair};
use qfc_executor::Executor;
use qfc_state::StateDB;
use qfc_storage::Database;
use qfc_types::{Address, Transaction, U256};

fn create_transfer_tx(keypair: &Keypair, nonce: u64) -> Transaction {
    let recipient = Address::from_slice(&[1u8; 20]).unwrap();
    let gas_price = U256::from_u64(1_000_000_000);
    let mut tx = Transaction::transfer(recipient, U256::from_u64(1000), nonce, gas_price);

    // Set the public key before signing (it's included in the signed message)
    tx.public_key = keypair.public_key();

    // Now sign the transaction (which includes the public key)
    let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
    tx.signature = keypair.sign_hash(&tx_hash);
    tx
}

fn bench_validate_transaction(c: &mut Criterion) {
    // Create a funded state for a specific transaction
    let keypair = Keypair::generate();
    let tx = create_transfer_tx(&keypair, 0);

    // Derive sender from public key (the proper way now)
    let sender = address_from_public_key(&keypair.public_key());

    let db = Database::open_temp().unwrap();
    let state = StateDB::new(db);
    state
        .add_balance(&sender, U256::from_u128(u128::MAX))
        .expect("Failed to fund");

    let executor = Executor::testnet();

    c.bench_function("validate_transfer_tx", |b| {
        b.iter(|| executor.validate_transaction(black_box(&tx), black_box(&state)))
    });
}

fn bench_execute_transfer(c: &mut Criterion) {
    let executor = Executor::testnet();
    let block_producer = Address::from_slice(&[2u8; 20]).unwrap();

    c.bench_function("execute_single_transfer", |b| {
        b.iter_custom(|iters| {
            // Create keypair and derive sender
            let keypair = Keypair::generate();
            let sender = address_from_public_key(&keypair.public_key());

            // Create fresh state and fund the sender
            let db = Database::open_temp().unwrap();
            let state = StateDB::new(db);
            let huge_balance = U256::from_u128(u128::MAX);
            state.add_balance(&sender, huge_balance).expect("Failed to fund");

            let start = std::time::Instant::now();
            for i in 0..iters {
                let tx = create_transfer_tx(&keypair, i);
                let signed_tx = executor.validate_transaction(&tx, &state).unwrap();
                let _ = executor.execute(&signed_tx, &state, &block_producer);
            }
            start.elapsed()
        })
    });
}

fn bench_tx_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");

    for batch_size in [10u64, 50, 100] {
        group.bench_with_input(
            BenchmarkId::new("transfers", batch_size),
            &batch_size,
            |b, &size| {
                let executor = Executor::testnet();
                let block_producer = Address::from_slice(&[2u8; 20]).unwrap();

                b.iter_custom(|iters| {
                    let total_txs = iters * size;

                    // Create keypair and derive sender
                    let keypair = Keypair::generate();
                    let sender = address_from_public_key(&keypair.public_key());

                    // Create fresh state and fund the sender
                    let db = Database::open_temp().unwrap();
                    let state = StateDB::new(db);
                    let huge_balance = U256::from_u128(u128::MAX);
                    state.add_balance(&sender, huge_balance).expect("Failed to fund");

                    // Pre-create all transactions
                    let txs: Vec<_> = (0..total_txs)
                        .map(|n| create_transfer_tx(&keypair, n))
                        .collect();

                    let start = std::time::Instant::now();
                    for tx in &txs {
                        let signed_tx = executor.validate_transaction(tx, &state).unwrap();
                        let _ = executor.execute(&signed_tx, &state, &block_producer);
                    }
                    start.elapsed()
                })
            },
        );
    }

    group.finish();
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
    bench_execute_transfer,
    bench_tx_throughput,
    bench_transaction_serialization,
    bench_signature_creation,
);
criterion_main!(benches);
