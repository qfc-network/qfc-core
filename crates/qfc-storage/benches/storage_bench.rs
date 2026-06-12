//! Benchmarks for QFC RocksDB storage layer

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use qfc_storage::{cf, Database, StorageConfig, WriteBatch};

/// Open a temporary database with hot-key sampling enabled at the default
/// 1-in-64 rate (SRE T8 overhead measurement). The TempDir must be kept
/// alive for the duration of the benchmark.
fn open_temp_sampled() -> (Database, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        path: dir.path().to_path_buf(),
        create_if_missing: true,
        hot_key_sampling: Some(64),
        ..Default::default()
    };
    (Database::open(config).unwrap(), dir)
}

fn bench_storage_put(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_put");

    for value_size in [32, 256, 1024, 4096] {
        let value = vec![0xABu8; value_size];
        group.throughput(Throughput::Bytes(value_size as u64));
        group.bench_function(format!("{}b", value_size), |b| {
            let db = Database::open_temp().unwrap();
            let mut i = 0u64;
            b.iter(|| {
                let key = i.to_be_bytes();
                i += 1;
                db.put(cf::STATE, black_box(&key), black_box(&value))
                    .unwrap()
            })
        });
    }

    group.finish();
}

fn bench_storage_get(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_get");

    for count in [100, 1000, 10000] {
        let db = Database::open_temp().unwrap();
        let value = vec![0xABu8; 256];

        // Pre-populate
        for i in 0..count as u64 {
            db.put(cf::STATE, &i.to_be_bytes(), &value).unwrap();
        }

        group.bench_function(format!("from_{}_keys", count), |b| {
            let mut i = 0u64;
            b.iter(|| {
                let key = (i % count as u64).to_be_bytes();
                i += 1;
                db.get(cf::STATE, black_box(&key)).unwrap()
            })
        });
    }

    group.finish();
}

fn bench_storage_get_miss(c: &mut Criterion) {
    let db = Database::open_temp().unwrap();

    c.bench_function("storage_get_miss", |b| {
        let mut i = 1_000_000u64;
        b.iter(|| {
            let key = i.to_be_bytes();
            i += 1;
            db.get(cf::STATE, black_box(&key)).unwrap()
        })
    });
}

fn bench_storage_batch_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_batch_write");

    for batch_size in [10, 100, 500, 1000] {
        group.throughput(Throughput::Elements(batch_size));
        group.bench_with_input(
            BenchmarkId::new("ops", batch_size),
            &batch_size,
            |b, &size| {
                let db = Database::open_temp().unwrap();
                let value = vec![0xCDu8; 256];
                let mut counter = 0u64;

                b.iter(|| {
                    let mut batch = WriteBatch::new();
                    for _ in 0..size {
                        let key = counter.to_be_bytes().to_vec();
                        counter += 1;
                        batch.put(cf::STATE, key, value.clone());
                    }
                    db.write_batch(black_box(batch)).unwrap()
                })
            },
        );
    }

    group.finish();
}

fn bench_storage_delete(c: &mut Criterion) {
    let db = Database::open_temp().unwrap();
    let value = vec![0xABu8; 256];

    // Pre-populate
    for i in 0..10_000u64 {
        db.put(cf::STATE, &i.to_be_bytes(), &value).unwrap();
    }

    let mut i = 0u64;
    c.bench_function("storage_delete", |b| {
        b.iter(|| {
            let key = i.to_be_bytes();
            i += 1;
            db.delete(cf::STATE, black_box(&key)).unwrap()
        })
    });
}

/// Same as the 256b case of `bench_storage_put`, but with hot-key sampling
/// at the default 1-in-64 rate. Compare against `storage_put/256b` from the
/// same run to quantify the enabled-sampling overhead.
fn bench_storage_put_sampled(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_put_sampled");
    let value = vec![0xABu8; 256];
    group.throughput(Throughput::Bytes(256));
    group.bench_function("256b", |b| {
        let (db, _dir) = open_temp_sampled();
        let mut i = 0u64;
        b.iter(|| {
            let key = i.to_be_bytes();
            i += 1;
            db.put(cf::STATE, black_box(&key), black_box(&value))
                .unwrap()
        })
    });
    group.finish();
}

/// Same as the 10k-key case of `bench_storage_get`, with hot-key sampling at
/// the default 1-in-64 rate.
fn bench_storage_get_sampled(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_get_sampled");
    let (db, _dir) = open_temp_sampled();
    let value = vec![0xABu8; 256];
    for i in 0..10_000u64 {
        db.put(cf::STATE, &i.to_be_bytes(), &value).unwrap();
    }
    group.bench_function("from_10000_keys", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let key = (i % 10_000).to_be_bytes();
            i += 1;
            db.get(cf::STATE, black_box(&key)).unwrap()
        })
    });
    group.finish();
}

/// Same as the 500-op case of `bench_storage_batch_write`, with hot-key
/// sampling at the default 1-in-64 rate.
fn bench_storage_batch_write_sampled(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_batch_write_sampled");
    group.throughput(Throughput::Elements(500));
    group.bench_function("ops/500", |b| {
        let (db, _dir) = open_temp_sampled();
        let value = vec![0xCDu8; 256];
        let mut counter = 0u64;
        b.iter(|| {
            let mut batch = WriteBatch::new();
            for _ in 0..500 {
                let key = counter.to_be_bytes().to_vec();
                counter += 1;
                batch.put(cf::STATE, key, value.clone());
            }
            db.write_batch(black_box(batch)).unwrap()
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_storage_put,
    bench_storage_get,
    bench_storage_get_miss,
    bench_storage_batch_write,
    bench_storage_delete,
    bench_storage_put_sampled,
    bench_storage_get_sampled,
    bench_storage_batch_write_sampled,
);
criterion_main!(benches);
