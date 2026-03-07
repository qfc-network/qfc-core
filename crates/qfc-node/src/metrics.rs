//! Prometheus metrics HTTP server
//!
//! Lightweight `/metrics` endpoint using `tiny_http` on a background `std::thread`.
//! Each scrape queries live state from shared `Arc` handles.

use parking_lot::RwLock;
use qfc_ai_coordinator::ProofPool;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};

pub struct MetricsServer {
    addr: SocketAddr,
    chain: Arc<Chain>,
    consensus: Arc<ConsensusEngine>,
    mempool: Arc<RwLock<Mempool>>,
    network: Option<Arc<NetworkService>>,
    proof_pool: Arc<RwLock<ProofPool>>,
    chain_id: u64,
}

impl MetricsServer {
    pub fn new(
        addr: SocketAddr,
        chain: Arc<Chain>,
        consensus: Arc<ConsensusEngine>,
        mempool: Arc<RwLock<Mempool>>,
        network: Option<Arc<NetworkService>>,
        proof_pool: Arc<RwLock<ProofPool>>,
        chain_id: u64,
    ) -> Self {
        Self {
            addr,
            chain,
            consensus,
            mempool,
            network,
            proof_pool,
            chain_id,
        }
    }

    /// Spawn a background thread running the metrics HTTP server.
    pub fn start(self) {
        let addr = self.addr;
        std::thread::spawn(move || {
            let server = match tiny_http::Server::http(addr) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to start metrics server on {}: {}", addr, e);
                    return;
                }
            };
            info!("Metrics server listening on http://{}/metrics", addr);

            for request in server.incoming_requests() {
                if request.url() != "/metrics" {
                    let resp = tiny_http::Response::from_string("Not Found\n")
                        .with_status_code(404)
                        .with_header(
                            "Content-Type: text/plain"
                                .parse::<tiny_http::Header>()
                                .unwrap(),
                        );
                    let _ = request.respond(resp);
                    continue;
                }

                let body = self.render_metrics();
                let resp = tiny_http::Response::from_string(&body).with_header(
                    "Content-Type: text/plain; version=0.0.4; charset=utf-8"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
                let _ = request.respond(resp);
            }
        });
    }

    fn render_metrics(&self) -> String {
        let mut out = String::with_capacity(2048);

        // --- chain ---
        let block_height = self.chain.block_number();

        let _ = writeln!(out, "# HELP qfc_block_height Current block height.");
        let _ = writeln!(out, "# TYPE qfc_block_height gauge");
        let _ = writeln!(out, "qfc_block_height {block_height}");

        let _ = writeln!(
            out,
            "# HELP qfc_blocks_produced_total Total blocks produced (same as height)."
        );
        let _ = writeln!(out, "# TYPE qfc_blocks_produced_total counter");
        let _ = writeln!(out, "qfc_blocks_produced_total {block_height}");

        // block time: diff between last two block timestamps
        let block_time = self.compute_block_time(block_height);
        let _ = writeln!(
            out,
            "# HELP qfc_block_time_seconds Seconds between the last two blocks."
        );
        let _ = writeln!(out, "# TYPE qfc_block_time_seconds gauge");
        let _ = writeln!(out, "qfc_block_time_seconds {block_time:.3}");

        // --- consensus ---
        let validators = self.consensus.get_validators();
        let active_validators = validators.len();
        let _ = writeln!(
            out,
            "# HELP qfc_active_validators Number of active validators."
        );
        let _ = writeln!(out, "# TYPE qfc_active_validators gauge");
        let _ = writeln!(out, "qfc_active_validators {active_validators}");

        let is_validator: u8 = if self.consensus.is_validator() { 1 } else { 0 };
        let _ = writeln!(
            out,
            "# HELP qfc_is_validator Whether this node is a validator (0/1)."
        );
        let _ = writeln!(out, "# TYPE qfc_is_validator gauge");
        let _ = writeln!(out, "qfc_is_validator {is_validator}");

        let epoch = self.consensus.get_epoch();
        let _ = writeln!(out, "# HELP qfc_epoch_number Current epoch number.");
        let _ = writeln!(out, "# TYPE qfc_epoch_number gauge");
        let _ = writeln!(out, "qfc_epoch_number {}", epoch.number);

        // --- network ---
        let peer_count = self
            .network
            .as_ref()
            .map(|n| n.peer_count())
            .unwrap_or(0);
        let _ = writeln!(out, "# HELP qfc_peer_count Number of connected peers.");
        let _ = writeln!(out, "# TYPE qfc_peer_count gauge");
        let _ = writeln!(out, "qfc_peer_count {peer_count}");

        // --- mempool ---
        let mempool_size = self.mempool.read().size();
        let _ = writeln!(
            out,
            "# HELP qfc_mempool_size Number of pending transactions."
        );
        let _ = writeln!(out, "# TYPE qfc_mempool_size gauge");
        let _ = writeln!(out, "qfc_mempool_size {mempool_size}");

        // --- chain id ---
        let _ = writeln!(out, "# HELP qfc_chain_id Chain ID of this node.");
        let _ = writeln!(out, "# TYPE qfc_chain_id gauge");
        let _ = writeln!(out, "qfc_chain_id {}", self.chain_id);

        // --- inference ---
        let pool = self.proof_pool.read();
        let accepted = pool.total_accepted();
        let submissions = pool.total_submissions();
        drop(pool);

        let _ = writeln!(
            out,
            "# HELP qfc_inference_tasks_completed Total inference proofs accepted."
        );
        let _ = writeln!(out, "# TYPE qfc_inference_tasks_completed gauge");
        let _ = writeln!(out, "qfc_inference_tasks_completed {accepted}");

        let pass_rate = if submissions > 0 {
            accepted as f64 / submissions as f64
        } else {
            1.0
        };
        let _ = writeln!(
            out,
            "# HELP qfc_inference_pass_rate Ratio of accepted to total inference submissions."
        );
        let _ = writeln!(out, "# TYPE qfc_inference_pass_rate gauge");
        let _ = writeln!(out, "qfc_inference_pass_rate {pass_rate:.6}");

        // --- node info ---
        let version = env!("CARGO_PKG_VERSION");
        let _ = writeln!(
            out,
            "# HELP qfc_node_info Node metadata as labels. Value is always 1."
        );
        let _ = writeln!(out, "# TYPE qfc_node_info gauge");
        let _ = writeln!(out, "qfc_node_info{{version=\"{version}\"}} 1");

        out
    }

    fn compute_block_time(&self, height: u64) -> f64 {
        if height < 2 {
            return 0.0;
        }
        let ts = |n: u64| -> Option<u64> {
            self.chain
                .get_block_by_number(n)
                .ok()
                .flatten()
                .map(|b| b.header.timestamp)
        };
        match (ts(height), ts(height - 1)) {
            (Some(cur), Some(prev)) if cur > prev => (cur - prev) as f64 / 1000.0,
            _ => 0.0,
        }
    }
}
