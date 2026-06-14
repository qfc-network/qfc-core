//! QFC B-2 spike: WAN pipeline-inference latency calculator.
//!
//! Pure-math decision tool. NO qfc-crate imports, pure std Rust, compiles with
//! default features (no candle). Run with:
//!     cargo run -p qfc-inference --example pipeline_latency
//!
//! ## Model
//! Pipeline inference splits a transformer across K miners (stages). Activations
//! (layer-boundary hidden states) flow miner->miner over K-1 network hops. This
//! tool computes the NETWORK-ONLY latency floor (compute deliberately excluded:
//! we are measuring whether the WAN alone makes pipeline inference viable).
//!
//! ## Network scenario provenance (cite these in the spike report)
//! Each scenario is (rtt_ms, bw_mbit_s):
//!
//! - lan_same_vpc      0.25 ms / 5000 Mbit/s
//!   MEASURED: A->B/C/D AWS us-east-1 intra-VPC RTT (~0.25 ms median).
//!   Bandwidth ASSUMED conservative: real intra-VPC is ~10-25 Gbit/s; we use
//!   5 Gbit/s as a deliberately pessimistic floor.
//! - regional_cross_az 5.0 ms / 1000 Mbit/s
//!   PUBLISHED: AWS cross-AZ / nearby-region typical RTT and bandwidth.
//! - continental       40.0 ms / 300 Mbit/s
//!   PUBLISHED: AWS cross-region same continent (e.g. us-east<->us-west is
//!   ~60 ms RTT); 40 ms is a mid-estimate. 300 Mbit/s single-stream typical.
//! - intercontinental  242.0 ms / 20 Mbit/s
//!   MEASURED: Singapore (Singtel) <-> AWS us-east-1 (Virginia). RTT 242 ms
//!   median, 20 Mbit/s sustained single-stream throughput.
//!
//! Derived:
//!   one_way_ms      = rtt_ms / 2
//!   transfer_ms(b)  = one_way_ms + (b * 8 / (bw_mbit_s * 1e6)) * 1000

// ----------------------------------------------------------------------------
// Network model
// ----------------------------------------------------------------------------

/// A measured/published network scenario between two pipeline stages.
struct Scenario {
    name: &'static str,
    rtt_ms: f64,
    bw_mbit_s: f64,
}

impl Scenario {
    /// One-way propagation latency (half the round trip).
    fn one_way_ms(&self) -> f64 {
        self.rtt_ms / 2.0
    }

    /// Time to move `bytes` across one hop: one-way latency + serialization.
    fn transfer_ms(&self, bytes: f64) -> f64 {
        self.one_way_ms() + (bytes * 8.0 / (self.bw_mbit_s * 1e6)) * 1000.0
    }
}

const SCENARIOS: [Scenario; 4] = [
    Scenario {
        name: "lan_same_vpc",
        rtt_ms: 0.25,
        bw_mbit_s: 5000.0,
    },
    Scenario {
        name: "regional_cross_az",
        rtt_ms: 5.0,
        bw_mbit_s: 1000.0,
    },
    Scenario {
        name: "continental",
        rtt_ms: 40.0,
        bw_mbit_s: 300.0,
    },
    Scenario {
        name: "intercontinental",
        rtt_ms: 242.0,
        bw_mbit_s: 20.0,
    },
];

// ----------------------------------------------------------------------------
// Model configs
// ----------------------------------------------------------------------------

/// fp16 activations: 2 bytes per element.
const DTYPE_BYTES: usize = 2;

struct Model {
    name: &'static str,
    hidden: usize,
    #[allow(dead_code)]
    layers: usize,
}

const MODELS: [Model; 4] = [
    Model {
        name: "bert-base",
        hidden: 768,
        layers: 12,
    },
    Model {
        name: "qwen2.5-0.5b",
        hidden: 896,
        layers: 24,
    },
    Model {
        name: "qwen2.5-3b",
        hidden: 2048,
        layers: 36,
    },
    Model {
        name: "qwen2.5-7b",
        hidden: 3584,
        layers: 28,
    },
];

/// Activation bytes crossing one layer boundary = batch * seq_len * hidden * dtype.
fn activation_bytes(batch: usize, seq_len: usize, hidden: usize) -> f64 {
    (batch * seq_len * hidden * DTYPE_BYTES) as f64
}

// ----------------------------------------------------------------------------
// Workload latency models
// ----------------------------------------------------------------------------

/// Interactive autoregressive decode: generate `tokens` tokens. Each token is a
/// full forward pass traversing all K stages -> K-1 hops, decode seq_len=1 so the
/// per-token activation is tiny (RTT-dominated). Returns network-floor latency in
/// milliseconds for the whole generation (compute excluded).
fn decode_latency_ms(model: &Model, sc: &Scenario, stages: usize, tokens: usize) -> f64 {
    let hops = (stages - 1) as f64;
    let act = activation_bytes(1, 1, model.hidden);
    tokens as f64 * hops * sc.transfer_ms(act)
}

/// Single-request prefill traversal latency (ms): one pass across K-1 hops.
fn prefill_request_ms(
    model: &Model,
    sc: &Scenario,
    stages: usize,
    batch: usize,
    seq: usize,
) -> f64 {
    let hops = (stages - 1) as f64;
    let act = activation_bytes(batch, seq, model.hidden);
    hops * sc.transfer_ms(act)
}

/// Per-hop transfer time (ms) for a prefill activation. Under pipelining this is
/// the steady-state throughput bottleneck (throughput ~ 1/max(stage_compute, per_hop_transfer)).
fn prefill_per_hop_ms(model: &Model, sc: &Scenario, batch: usize, seq: usize) -> f64 {
    sc.transfer_ms(activation_bytes(batch, seq, model.hidden))
}

// ----------------------------------------------------------------------------
// Sanity checks (run from main; panic on math regression)
// ----------------------------------------------------------------------------

fn sanity() {
    let m7b = &MODELS[3];
    assert_eq!(m7b.name, "qwen2.5-7b");
    let inter = &SCENARIOS[3];
    assert_eq!(inter.name, "intercontinental");
    let lan = &SCENARIOS[0];

    // intercontinental 7b K=4 decode > 30s
    let dec_inter = decode_latency_ms(m7b, inter, 4, 100) / 1000.0;
    assert!(
        dec_inter > 30.0,
        "intercontinental 7b K=4 decode should be >30s, got {dec_inter:.3}s"
    );

    // lan 7b K=4 decode < 0.1s
    let dec_lan = decode_latency_ms(m7b, lan, 4, 100) / 1000.0;
    assert!(
        dec_lan < 0.1,
        "lan 7b K=4 decode should be <0.1s, got {dec_lan:.4}s"
    );

    // intercontinental B8 S512 7b per-hop > 5000ms
    let hop_inter = prefill_per_hop_ms(m7b, inter, 8, 512);
    assert!(
        hop_inter > 5000.0,
        "intercontinental B8 S512 7b per-hop should be >5000ms, got {hop_inter:.1}ms"
    );
}

// ----------------------------------------------------------------------------
// Output
// ----------------------------------------------------------------------------

fn print_table_a() {
    println!("## Table A — Activation transfer size per layer boundary (KB)\n");
    println!("| model | decode (b1,s1) | prefill (b1,s512) | prefill (b8,s512) |");
    println!("|---|---:|---:|---:|");
    for m in &MODELS {
        let decode = activation_bytes(1, 1, m.hidden) / 1024.0;
        let p1 = activation_bytes(1, 512, m.hidden) / 1024.0;
        let p8 = activation_bytes(8, 512, m.hidden) / 1024.0;
        println!("| {} | {:.3} | {:.1} | {:.1} |", m.name, decode, p1, p8);
    }
    println!();
}

fn print_table_b() {
    println!("## Table B — Interactive decode: network-only latency for 100 tokens (seconds)\n");
    let ks = [2usize, 4, 8];
    // header
    print!("| model | K |");
    for sc in &SCENARIOS {
        print!(" {} |", sc.name);
    }
    println!();
    print!("|---|---:|");
    for _ in &SCENARIOS {
        print!("---:|");
    }
    println!();
    for m in &MODELS {
        for &k in &ks {
            print!("| {} | {} |", m.name, k);
            for sc in &SCENARIOS {
                let s = decode_latency_ms(m, sc, k, 100) / 1000.0;
                print!(" {s:.3} |");
            }
            println!();
        }
    }
    println!();
}

fn print_table_c() {
    println!("## Table C — Batch prefill (B=8, S=512): per-hop transfer (ms) and single-request K=4 latency (ms)\n");
    print!("| model |");
    for sc in &SCENARIOS {
        print!(" {} per-hop | {} K4-req |", sc.name, sc.name);
    }
    println!();
    print!("|---|");
    for _ in &SCENARIOS {
        print!("---:|---:|");
    }
    println!();
    for m in &MODELS {
        print!("| {} |", m.name);
        for sc in &SCENARIOS {
            let hop = prefill_per_hop_ms(m, sc, 8, 512);
            let req = prefill_request_ms(m, sc, 4, 8, 512);
            print!(" {hop:.1} | {req:.1} |");
        }
        println!();
    }
    println!();
}

fn print_verdicts() {
    let m7b = &MODELS[3];

    println!("## Summary verdicts\n");

    // Interactive viable? 100-token decode network-floor < 2000 ms, qwen2.5-7b K=4
    println!(
        "### Interactive viable? (qwen2.5-7b, K=4, 100-token decode network-floor < 2000 ms)\n"
    );
    println!("| scenario | network-floor (ms) | verdict |");
    println!("|---|---:|---|");
    for sc in &SCENARIOS {
        let ms = decode_latency_ms(m7b, sc, 4, 100);
        let verdict = if ms < 2000.0 { "PASS" } else { "FAIL" };
        println!("| {} | {:.1} | {} |", sc.name, ms, verdict);
    }
    println!();

    // Batch transfer-bound? per-hop prefill transfer (B8 S512, 7b) > 200 ms
    println!("### Batch transfer-bound? (qwen2.5-7b, B=8, S=512, per-hop transfer > 200 ms => bandwidth dominates)\n");
    println!("| scenario | per-hop (ms) | bandwidth-gated |");
    println!("|---|---:|---|");
    for sc in &SCENARIOS {
        let hop = prefill_per_hop_ms(m7b, sc, 8, 512);
        let flag = if hop > 200.0 { "YES" } else { "no" };
        println!("| {} | {:.1} | {} |", sc.name, hop, flag);
    }
    println!();
}

fn main() {
    // Guard against math regressions before emitting any numbers.
    sanity();

    println!("# QFC B-2 spike — WAN pipeline-inference latency\n");
    println!(
        "Network-only floor (compute excluded). dtype=fp16 (2B). \
         Decode T=100 tokens, batch=1, seq=1. Prefill S=512. \
         K (stages) = 4 representative; K in {{2,4,8}} for decode.\n"
    );

    print_table_a();
    print_table_b();
    print_table_c();
    print_verdicts();

    println!(
        "GO/NO-GO: interactive pipeline inference over real WAN = NO-GO \
         (RTT-dominated, 7B K=4 intercontinental ~{:.0}s for 100 tokens); \
         batch prefill viable but bandwidth-gated intercontinentally \
         (7B B8 S512 per-hop ~{:.0}s).",
        decode_latency_ms(&MODELS[3], &SCENARIOS[3], 4, 100) / 1000.0,
        prefill_per_hop_ms(&MODELS[3], &SCENARIOS[3], 8, 512) / 1000.0,
    );
}

// ----------------------------------------------------------------------------
// Unit tests (cargo test -p qfc-inference --example pipeline_latency)
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intercontinental_7b_k4_decode_over_30s() {
        let s = decode_latency_ms(&MODELS[3], &SCENARIOS[3], 4, 100) / 1000.0;
        assert!(s > 30.0, "got {s:.3}s");
    }

    #[test]
    fn lan_7b_k4_decode_under_100ms() {
        let s = decode_latency_ms(&MODELS[3], &SCENARIOS[0], 4, 100) / 1000.0;
        assert!(s < 0.1, "got {s:.4}s");
    }

    #[test]
    fn intercontinental_b8_s512_7b_per_hop_over_5000ms() {
        let ms = prefill_per_hop_ms(&MODELS[3], &SCENARIOS[3], 8, 512);
        assert!(ms > 5000.0, "got {ms:.1}ms");
    }

    #[test]
    fn sanity_runs() {
        sanity();
    }
}
