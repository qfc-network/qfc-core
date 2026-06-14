//! Optional integration test: point the watchdog engine at a live dev node.
//!
//! Like the qfc-node integration tests, this needs the release binary
//! (`cargo build --release -p qfc-node`). When it is not present the test
//! skips instead of failing, so plain `cargo test -p qfc-watchdog` stays
//! green on a fresh checkout.

use qfc_watchdog::detector::{DetectorConfig, Engine, MetricsSnapshot};
use qfc_watchdog::probe;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RPC_PORT: u16 = 19720;
const METRICS_PORT: u16 = 20720;

fn release_node_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .unwrap()
        .parent() // workspace root
        .unwrap()
        .join("target/release/qfc-node")
}

struct NodeGuard {
    child: Child,
    _datadir: tempfile::TempDir,
}

impl Drop for NodeGuard {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn watchdog_observes_live_dev_node_as_healthy() {
    let binary = release_node_binary();
    if !binary.exists() {
        eprintln!(
            "skipping live-node integration test: {} not built \
             (run `cargo build --release -p qfc-node`)",
            binary.display()
        );
        return;
    }

    let datadir = tempfile::tempdir().unwrap();
    let child = Command::new(&binary)
        .args([
            "--dev",
            "--no-network",
            "--datadir",
            datadir.path().to_str().unwrap(),
            "--rpc-addr",
            &format!("127.0.0.1:{RPC_PORT}"),
            "--metrics-addr",
            &format!("127.0.0.1:{METRICS_PORT}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn qfc-node");
    let _guard = NodeGuard {
        child,
        _datadir: datadir,
    };

    let metrics_addr: SocketAddr = format!("127.0.0.1:{METRICS_PORT}").parse().unwrap();
    let rpc_addr: SocketAddr = format!("127.0.0.1:{RPC_PORT}").parse().unwrap();
    let timeout = Duration::from_secs(2);

    // Wait for both surfaces to come up.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let metrics_up = probe::scrape_metrics(metrics_addr, timeout).is_ok();
        let rpc_up = probe::rpc_probe(rpc_addr, timeout).is_ok();
        if metrics_up && rpc_up {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "node endpoints did not come up within 30s (metrics_up={metrics_up}, rpc_up={rpc_up})"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // A few real polls: a freshly started dev node must score 100 with no
    // detector firing (windows not covered + node actually healthy).
    let mut engine = Engine::new(DetectorConfig::default());
    let mut last = None;
    for _ in 0..3 {
        let text = probe::scrape_metrics(metrics_addr, timeout).expect("scrape metrics");
        assert!(
            text.contains("qfc_block_height"),
            "exporter output missing qfc_block_height:\n{text}"
        );
        let snap = MetricsSnapshot::from_prometheus(&text);
        assert!(snap.block_height.is_some());
        let rpc_ok = probe::rpc_probe(rpc_addr, timeout).is_ok();
        last = Some(engine.evaluate(now_secs(), Some(&snap), rpc_ok));
        std::thread::sleep(Duration::from_secs(1));
    }
    let eval = last.unwrap();
    assert_eq!(
        eval.health_score,
        100,
        "evidence: {}",
        eval.evidence_summary()
    );
    assert!(eval.detectors.iter().all(|d| !d.firing));
}
