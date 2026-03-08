//! Benchmarks for QFC mempool (transaction pool)

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use qfc_crypto::{blake3_hash, Keypair};
use qfc_mempool::Mempool;
use qfc_types::{Address, Transaction, U256};

fn create_test_tx(keypair: &Keypair, nonce: u64, gas_price: u64) -> Transaction {
    let recipient = Address::new([0x22; 20]);
    let mut tx = Transaction::transfer(
        recipient,
        U256::from_u64(1000),
        nonce,
        U256::from_u64(gas_price),
    );
    tx.public_key = keypair.public_key();
    let tx_hash = blake3_hash(&tx.to_bytes_without_signature());
    tx.signature = keypair.sign_hash(&tx_hash);
    tx
}

fn bench_mempool_add(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_add");

    for count in [100, 500, 1000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("txs", count), &count, |b, &size| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let pool = Mempool::default_pool();
                    let keypair = Keypair::generate();
                    let sender = qfc_crypto::address_from_public_key(&keypair.public_key());
                    let txs: Vec<_> = (0..size)
                        .map(|n| create_test_tx(&keypair, n, 2_000_000_000))
                        .collect();

                    let start = std::time::Instant::now();
                    for tx in &txs {
                        let _ = pool.add(tx.clone(), sender);
                    }
                    total += start.elapsed();
                }
                total
            })
        });
    }

    group.finish();
}

fn bench_mempool_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_select");

    for pool_size in [100, 1000, 5000] {
        // Pre-fill pool with transactions from multiple senders
        let pool = Mempool::default_pool();
        let num_senders = 10;
        for _s in 0..num_senders {
            let keypair = Keypair::generate();
            let sender = qfc_crypto::address_from_public_key(&keypair.public_key());
            let per_sender = pool_size / num_senders;
            for n in 0..per_sender {
                let gas_price = 1_000_000_000 + (n as u64 * 1_000_000); // varying gas prices
                let tx = create_test_tx(&keypair, n as u64, gas_price);
                let _ = pool.add(tx, sender);
            }
        }

        group.bench_function(format!("from_{}_txs", pool_size), |b| {
            b.iter(|| pool.select(black_box(30_000_000), black_box(500)))
        });
    }

    group.finish();
}

fn bench_mempool_get(c: &mut Criterion) {
    let pool = Mempool::default_pool();
    let keypair = Keypair::generate();
    let sender = qfc_crypto::address_from_public_key(&keypair.public_key());

    // Add 1000 transactions and collect their hashes
    let mut hashes = Vec::new();
    for n in 0..1000u64 {
        let tx = create_test_tx(&keypair, n, 2_000_000_000);
        if let Ok(hash) = pool.add(tx, sender) {
            hashes.push(hash);
        }
    }

    c.bench_function("mempool_get_by_hash", |b| {
        let mut i = 0;
        b.iter(|| {
            let hash = &hashes[i % hashes.len()];
            i += 1;
            pool.get(black_box(hash))
        })
    });
}

fn bench_mempool_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("mempool_remove");

    for count in [100, 500] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("txs", count), &count, |b, &size| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let pool = Mempool::default_pool();
                    let keypair = Keypair::generate();
                    let sender = qfc_crypto::address_from_public_key(&keypair.public_key());
                    let mut hashes = Vec::new();
                    for n in 0..size {
                        let tx = create_test_tx(&keypair, n, 2_000_000_000);
                        if let Ok(h) = pool.add(tx, sender) {
                            hashes.push(h);
                        }
                    }

                    let start = std::time::Instant::now();
                    for h in &hashes {
                        pool.remove(h);
                    }
                    total += start.elapsed();
                }
                total
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_mempool_add,
    bench_mempool_select,
    bench_mempool_get,
    bench_mempool_remove,
);
criterion_main!(benches);
