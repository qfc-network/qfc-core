//! Benchmarks for inference proof verification

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use qfc_ai_coordinator::verification::{should_spot_check, verify_basic};
use qfc_inference::model::ModelRegistry;
use qfc_inference::proof::InferenceProof;
use qfc_inference::task::{ComputeTaskType, ModelId};
use qfc_inference::BackendType;
use qfc_types::{Address, Hash};

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn make_proof(output_byte: u8, flops: u64) -> InferenceProof {
    InferenceProof::new(
        Address::new([0x11; 20]),
        1,
        ComputeTaskType::Embedding {
            model_id: ModelId::new("qfc-embed-small", "v1.0"),
            input_hash: Hash::ZERO,
        },
        Hash::ZERO,
        Hash::new([output_byte; 32]),
        100,
        flops,
        BackendType::Cpu,
        qfc_inference::CanonicalFormat::SafetensorsFp32,
        now_secs(),
    )
}

fn bench_verify_basic(c: &mut Criterion) {
    let registry = ModelRegistry::default_v2();
    let proof = make_proof(0xAB, 1_000_000_000);
    let now = now_secs();

    let mut group = c.benchmark_group("verify_basic");
    group.throughput(Throughput::Elements(1));

    group.bench_function("single_proof", |b| {
        b.iter(|| verify_basic(black_box(&proof), black_box(now), black_box(&registry)))
    });

    group.finish();
}

fn bench_verify_basic_batch(c: &mut Criterion) {
    let registry = ModelRegistry::default_v2();
    let now = now_secs();

    let mut group = c.benchmark_group("verify_basic_batch");

    for count in [100, 500, 1000] {
        let proofs: Vec<_> = (0..count)
            .map(|i| make_proof((i % 256) as u8, 1_000_000_000))
            .collect();

        group.throughput(Throughput::Elements(count));
        group.bench_function(format!("{}_proofs", count), |b| {
            b.iter(|| {
                for proof in &proofs {
                    let _ = verify_basic(black_box(proof), black_box(now), black_box(&registry));
                }
            })
        });
    }

    group.finish();
}

fn bench_should_spot_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("spot_check");
    group.throughput(Throughput::Elements(1000));

    let proofs: Vec<_> = (0..1000u64)
        .map(|i| make_proof((i % 256) as u8, 1_000_000_000))
        .collect();

    group.bench_function("1000_decisions", |b| {
        b.iter(|| {
            let mut checked = 0u32;
            for proof in &proofs {
                if should_spot_check(black_box(proof)) {
                    checked += 1;
                }
            }
            checked
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_verify_basic,
    bench_verify_basic_batch,
    bench_should_spot_check,
);
criterion_main!(benches);
