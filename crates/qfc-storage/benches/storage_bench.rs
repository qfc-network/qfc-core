//! Benchmarks for QFC RocksDB storage layer

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use qfc_storage::{cf, Database, WriteBatch};

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
                db.put(cf::STATE, black_box(&key), black_box(&value)).unwrap()
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

criterion_group!(
    benches,
    bench_storage_put,
    bench_storage_get,
    bench_storage_get_miss,
    bench_storage_batch_write,
    bench_storage_delete,
);
criterion_main!(benches);
