//! Benchmarks for QFC Merkle Patricia Trie

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use qfc_crypto::blake3_hash;
use qfc_storage::Database;
use qfc_trie::Trie;

fn make_key(i: u64) -> Vec<u8> {
    blake3_hash(&i.to_le_bytes()).as_bytes().to_vec()
}

fn make_value(i: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(&i.to_le_bytes());
    v.extend_from_slice(&[0xABu8; 56]);
    v
}

fn bench_trie_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_insert");

    for count in [100, 1000, 5000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("keys", count), &count, |b, &size| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let db = Database::open_temp().unwrap();
                    let mut trie = Trie::new(db);

                    let start = std::time::Instant::now();
                    for i in 0..size {
                        trie.insert(&make_key(i), make_value(i)).unwrap();
                    }
                    total += start.elapsed();
                }
                total
            })
        });
    }

    group.finish();
}

fn bench_trie_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_get");

    for count in [100, 1000, 5000] {
        let db = Database::open_temp().unwrap();
        let mut trie = Trie::new(db);

        // Pre-populate
        for i in 0..count {
            trie.insert(&make_key(i), make_value(i)).unwrap();
        }
        trie.commit().unwrap();

        group.bench_function(format!("from_{}_keys", count), |b| {
            let mut i = 0u64;
            b.iter(|| {
                let key = make_key(i % count);
                i += 1;
                trie.get(black_box(&key)).unwrap()
            })
        });
    }

    group.finish();
}

fn bench_trie_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_commit");

    for count in [10, 100, 500] {
        group.bench_with_input(
            BenchmarkId::new("dirty_nodes", count),
            &count,
            |b, &size| {
                b.iter_custom(|iters| {
                    let mut total = std::time::Duration::ZERO;
                    for _ in 0..iters {
                        let db = Database::open_temp().unwrap();
                        let mut trie = Trie::new(db);

                        for i in 0..size {
                            trie.insert(&make_key(i), make_value(i)).unwrap();
                        }

                        let start = std::time::Instant::now();
                        trie.commit().unwrap();
                        total += start.elapsed();
                    }
                    total
                })
            },
        );
    }

    group.finish();
}

fn bench_trie_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("trie_delete");

    for count in [100, 500] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(BenchmarkId::new("keys", count), &count, |b, &size| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let db = Database::open_temp().unwrap();
                    let mut trie = Trie::new(db);

                    // Insert first
                    for i in 0..size {
                        trie.insert(&make_key(i), make_value(i)).unwrap();
                    }
                    trie.commit().unwrap();

                    // Then delete
                    let start = std::time::Instant::now();
                    for i in 0..size {
                        trie.delete(&make_key(i)).unwrap();
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
    bench_trie_insert,
    bench_trie_get,
    bench_trie_commit,
    bench_trie_delete,
);
criterion_main!(benches);
