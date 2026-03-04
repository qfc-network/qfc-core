//! Benchmarks for QFC cryptographic operations

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use qfc_crypto::{blake3_hash, merkle_root, Keypair};
use qfc_types::Hash;

fn bench_blake3_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("blake3");

    // Benchmark different data sizes
    for size in [32, 256, 1024, 4096, 65536] {
        let data = vec![0u8; size];
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("hash_{}b", size), |b| {
            b.iter(|| blake3_hash(black_box(&data)))
        });
    }

    group.finish();
}

fn bench_merkle_root(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle");

    // Benchmark different tree sizes
    for count in [4, 16, 64, 256, 1024] {
        let hashes: Vec<Hash> = (0u64..count)
            .map(|i| blake3_hash(&i.to_le_bytes()))
            .collect();

        group.bench_function(format!("root_{}_leaves", count), |b| {
            b.iter(|| merkle_root(black_box(&hashes)))
        });
    }

    group.finish();
}

fn bench_ed25519_sign(c: &mut Criterion) {
    let mut group = c.benchmark_group("ed25519");

    let keypair = Keypair::generate();
    let message = [0u8; 32];
    let hash = blake3_hash(&message);

    group.bench_function("sign_32b", |b| b.iter(|| keypair.sign(black_box(&message))));

    group.bench_function("sign_hash", |b| {
        b.iter(|| keypair.sign_hash(black_box(&hash)))
    });

    group.finish();
}

fn bench_ed25519_verify(c: &mut Criterion) {
    let mut group = c.benchmark_group("ed25519");

    let keypair = Keypair::generate();
    let message = [0u8; 32];
    let signature = keypair.sign(&message);
    let public_key = keypair.public_key();

    group.bench_function("verify_32b", |b| {
        b.iter(|| {
            qfc_crypto::verify_signature(
                black_box(&public_key),
                black_box(&message),
                black_box(&signature),
            )
        })
    });

    group.finish();
}

fn bench_keypair_generation(c: &mut Criterion) {
    c.bench_function("keypair_generate", |b| b.iter(Keypair::generate));
}

criterion_group!(
    benches,
    bench_blake3_hash,
    bench_merkle_root,
    bench_ed25519_sign,
    bench_ed25519_verify,
    bench_keypair_generation,
);
criterion_main!(benches);
