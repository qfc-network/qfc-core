//! Prometheus metrics HTTP server
//!
//! Lightweight `/metrics` endpoint using `tiny_http` on a background `std::thread`.
//! Each scrape queries live state from shared `Arc` handles.

use parking_lot::{Mutex, RwLock};
use qfc_ai_coordinator::ProofPool;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_rpc::metrics::RpcMetrics;
use qfc_rpc::SyncStatusProvider;
use qfc_types::Hash;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Cached total transaction count (accumulated across scrapes)
    tx_total: AtomicU64,
    /// Last block height we counted transactions up to
    tx_counted_to: AtomicU64,
    /// T2.2: sync status (lag vs peers, syncing flag, pending queue depth)
    sync_status: Option<Arc<dyn SyncStatusProvider>>,
    /// T2.2: RPC metrics registry, shared with the jsonrpsee middleware
    rpc_metrics: Option<Arc<RpcMetrics>>,
    /// T2.2: head observed at the previous scrape, for reorg detection
    last_head: Mutex<Option<(u64, Hash)>>,
    /// T2.2: reorgs detected at scrape granularity (see `update_reorg_detection`)
    reorgs_detected: AtomicU64,
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
            tx_total: AtomicU64::new(0),
            tx_counted_to: AtomicU64::new(0),
            sync_status: None,
            rpc_metrics: None,
            last_head: Mutex::new(None),
            reorgs_detected: AtomicU64::new(0),
        }
    }

    /// Attach a sync status provider (T2.2: sync lag / syncing / pending blocks).
    pub fn with_sync_status(mut self, sync_status: Arc<dyn SyncStatusProvider>) -> Self {
        self.sync_status = Some(sync_status);
        self
    }

    /// Attach the RPC metrics registry shared with the RPC middleware (T2.2).
    pub fn with_rpc_metrics(mut self, rpc_metrics: Arc<RpcMetrics>) -> Self {
        self.rpc_metrics = Some(rpc_metrics);
        self
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

    /// Accumulate transaction count for new blocks since last scrape.
    fn update_tx_total(&self, current_height: u64) -> u64 {
        let counted_to = self.tx_counted_to.load(Ordering::Relaxed);
        if current_height > counted_to {
            let mut added = 0u64;
            for h in (counted_to + 1)..=current_height {
                if let Ok(Some(block)) = self.chain.get_block_by_number(h) {
                    added += block.transactions.len() as u64;
                }
            }
            self.tx_total.fetch_add(added, Ordering::Relaxed);
            self.tx_counted_to.store(current_height, Ordering::Relaxed);
        }
        self.tx_total.load(Ordering::Relaxed)
    }

    fn render_metrics(&self) -> String {
        let mut out = String::with_capacity(4096);

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

        // total transactions (accumulated counter)
        let tx_total = self.update_tx_total(block_height);
        let _ = writeln!(
            out,
            "# HELP qfc_transactions_total Total transactions processed."
        );
        let _ = writeln!(out, "# TYPE qfc_transactions_total counter");
        let _ = writeln!(out, "qfc_transactions_total {tx_total}");

        // block time: diff between last two block timestamps
        let block_time = self.compute_block_time(block_height);
        let _ = writeln!(
            out,
            "# HELP qfc_block_time_seconds Seconds between the last two blocks."
        );
        let _ = writeln!(out, "# TYPE qfc_block_time_seconds gauge");
        let _ = writeln!(out, "qfc_block_time_seconds {block_time:.3}");

        // T2.2: wall-clock seconds since the head block's timestamp — the
        // primary block-production-stall signal (alerting SLI).
        let time_since_last_block = self.compute_time_since_last_block();
        let _ = writeln!(
            out,
            "# HELP qfc_time_since_last_block_seconds Seconds since the head block timestamp."
        );
        let _ = writeln!(out, "# TYPE qfc_time_since_last_block_seconds gauge");
        let _ = writeln!(
            out,
            "qfc_time_since_last_block_seconds {time_since_last_block:.3}"
        );

        // T2.2: reorg detection at scrape granularity (no chain-internal hook
        // yet — a precise import-time counter is part of the T2.1/T3 follow-up
        // since it touches qfc-chain/src/chain.rs).
        let reorgs = self.update_reorg_detection(block_height);
        let _ = writeln!(
            out,
            "# HELP qfc_reorgs_detected_total Reorgs detected by the exporter (scrape-granularity: counted when a previously-seen block hash changes)."
        );
        let _ = writeln!(out, "# TYPE qfc_reorgs_detected_total counter");
        let _ = writeln!(out, "qfc_reorgs_detected_total {reorgs}");

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

        // --- per-validator metrics ---
        let _ = writeln!(
            out,
            "# HELP qfc_contribution_score Per-validator PoC contribution score."
        );
        let _ = writeln!(out, "# TYPE qfc_contribution_score gauge");
        for v in &validators {
            let addr = v.address;
            let _ = writeln!(
                out,
                "qfc_contribution_score{{validator=\"{addr}\"}} {}",
                v.contribution_score
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_inference_flops_total Cumulative FLOPS executed by validator."
        );
        let _ = writeln!(out, "# TYPE qfc_inference_flops_total counter");
        for v in &validators {
            let addr = v.address;
            let _ = writeln!(
                out,
                "qfc_inference_flops_total{{validator=\"{addr}\"}} {}",
                v.inference_score
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_inference_score Per-validator inference quality score."
        );
        let _ = writeln!(out, "# TYPE qfc_inference_score gauge");
        for v in &validators {
            let addr = v.address;
            // inference_score already accumulates FLOPS; combine with tasks and pass rate
            // for a quality metric consistent with scoring.rs
            let score = if v.tasks_completed > 0 {
                let pass_ratio = v.verification_pass_rate as f64 / 10000.0;
                let flops_norm = v.inference_score as f64 / 1_000_000_000.0; // per GFLOPS
                (flops_norm * (v.tasks_completed as f64).sqrt() * pass_ratio * pass_ratio * 1000.0)
                    as u64
            } else {
                0
            };
            let _ = writeln!(out, "qfc_inference_score{{validator=\"{addr}\"}} {score}");
        }

        // --- network ---
        let peer_count = self.network.as_ref().map(|n| n.peer_count()).unwrap_or(0);
        let _ = writeln!(out, "# HELP qfc_peer_count Number of connected peers.");
        let _ = writeln!(out, "# TYPE qfc_peer_count gauge");
        let _ = writeln!(out, "qfc_peer_count {peer_count}");

        // --- mempool ---
        let (mempool_size, oldest_tx_age) = {
            let pool = self.mempool.read();
            (pool.size(), pool.oldest_tx_age())
        };
        let _ = writeln!(
            out,
            "# HELP qfc_mempool_size Number of pending transactions."
        );
        let _ = writeln!(out, "# TYPE qfc_mempool_size gauge");
        let _ = writeln!(out, "qfc_mempool_size {mempool_size}");

        // T2.2: age of the oldest pending transaction (0 when the pool is
        // empty) — rising age with stable depth means stuck inclusion.
        let oldest_age_secs = oldest_tx_age.map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let _ = writeln!(
            out,
            "# HELP qfc_mempool_oldest_tx_age_seconds Age of the oldest pending transaction (0 if empty)."
        );
        let _ = writeln!(out, "# TYPE qfc_mempool_oldest_tx_age_seconds gauge");
        let _ = writeln!(
            out,
            "qfc_mempool_oldest_tx_age_seconds {oldest_age_secs:.3}"
        );

        // --- sync (T2.2) ---
        if let Some(sync) = &self.sync_status {
            let syncing: u8 = if sync.is_syncing() { 1 } else { 0 };
            let highest_peer = sync.highest_peer_block();
            let pending = sync.pending_count();
            let lag = highest_peer.saturating_sub(block_height);

            let _ = writeln!(
                out,
                "# HELP qfc_sync_syncing Whether the node is actively syncing (0/1)."
            );
            let _ = writeln!(out, "# TYPE qfc_sync_syncing gauge");
            let _ = writeln!(out, "qfc_sync_syncing {syncing}");

            let _ = writeln!(
                out,
                "# HELP qfc_sync_highest_peer_block Highest block number known from peers."
            );
            let _ = writeln!(out, "# TYPE qfc_sync_highest_peer_block gauge");
            let _ = writeln!(out, "qfc_sync_highest_peer_block {highest_peer}");

            let _ = writeln!(
                out,
                "# HELP qfc_sync_lag_blocks Blocks behind the highest known peer (0 when ahead or in sync)."
            );
            let _ = writeln!(out, "# TYPE qfc_sync_lag_blocks gauge");
            let _ = writeln!(out, "qfc_sync_lag_blocks {lag}");

            let _ = writeln!(
                out,
                "# HELP qfc_sync_pending_blocks Blocks queued waiting for missing parents."
            );
            let _ = writeln!(out, "# TYPE qfc_sync_pending_blocks gauge");
            let _ = writeln!(out, "qfc_sync_pending_blocks {pending}");
        }

        // --- chain id ---
        let _ = writeln!(out, "# HELP qfc_chain_id Chain ID of this node.");
        let _ = writeln!(out, "# TYPE qfc_chain_id gauge");
        let _ = writeln!(out, "qfc_chain_id {}", self.chain_id);

        // --- inference (network-wide from proof pool) ---
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

        // --- rpc (T2.2, recorded by qfc-rpc middleware) ---
        if let Some(rpc) = &self.rpc_metrics {
            rpc.render_prometheus(&mut out);
        }

        // --- storage (T2.1, deferred) ---
        // TODO(T2.1): RocksDB statistics export — compaction-stall counters,
        // pending-compaction bytes, block-cache hit/miss, memtable size,
        // per-CF read/write volume, WAL sync latency. Blocked on the
        // qfc-storage statistics hook landing on a parallel branch (touches
        // crates/qfc-storage/src/db.rs, owned by the T3 work). When it lands,
        // render it here:
        //   self.render_storage_metrics(&mut out);

        out
    }

    /// Wall-clock seconds since the head block's timestamp (block timestamps
    /// are unix milliseconds). Returns 0 when the chain is empty or clocks
    /// disagree.
    fn compute_time_since_last_block(&self) -> f64 {
        let head_ts_ms = match self.chain.head() {
            Some(sealed) => sealed.block.header.timestamp,
            None => return 0.0,
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now_ms.saturating_sub(head_ts_ms) as f64 / 1000.0
    }

    /// Detect reorgs at scrape granularity: remember the head (height, hash)
    /// from the previous scrape; if the block now stored at that height has a
    /// different hash, the canonical chain was rewritten below the old head.
    /// Misses reorgs that fully revert between scrapes — a precise counter
    /// needs an import-time hook in qfc-chain (T2.1/T3 follow-up).
    fn update_reorg_detection(&self, current_height: u64) -> u64 {
        let hash_at = |n: u64| -> Option<Hash> {
            self.chain
                .get_block_by_number(n)
                .ok()
                .flatten()
                .map(|b| blake3_hash(&b.header_bytes()))
        };

        let mut last = self.last_head.lock();
        if let Some((prev_height, prev_hash)) = *last {
            if let Some(now_hash) = hash_at(prev_height) {
                if now_hash != prev_hash {
                    self.reorgs_detected.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let Some(cur_hash) = hash_at(current_height) {
            *last = Some((current_height, cur_hash));
        }
        self.reorgs_detected.load(Ordering::Relaxed)
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

#[cfg(test)]
mod tests {
    use super::*;
    use qfc_chain::{ChainConfig, GenesisConfig};
    use qfc_consensus::ConsensusConfig;
    use qfc_mempool::MempoolConfig;
    use qfc_storage::{Database, StorageConfig};

    struct FakeSync;

    impl SyncStatusProvider for FakeSync {
        fn is_syncing(&self) -> bool {
            false
        }
        fn highest_peer_block(&self) -> u64 {
            42
        }
        fn pending_count(&self) -> usize {
            0
        }
    }

    /// T2.2: the exporter output must contain every metric name referenced by
    /// docs/observability/ (dashboard + alert rules).
    #[test]
    fn render_metrics_includes_expected_metric_names() {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(StorageConfig {
            path: tmp.path().join("db"),
            create_if_missing: true,
            ..Default::default()
        })
        .unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let chain = Arc::new(
            Chain::new(
                db,
                ChainConfig {
                    chain_id: 9000,
                    genesis: GenesisConfig::dev(),
                },
                consensus.clone(),
            )
            .unwrap(),
        );
        let mempool = Arc::new(RwLock::new(Mempool::new(MempoolConfig::default())));
        let proof_pool = Arc::new(RwLock::new(ProofPool::new()));

        let rpc_metrics = Arc::new(RpcMetrics::new());
        rpc_metrics.request_started();
        rpc_metrics.request_finished("eth_blockNumber", std::time::Duration::from_millis(5), None);

        let server = MetricsServer::new(
            "127.0.0.1:0".parse().unwrap(),
            chain,
            consensus,
            mempool,
            None,
            proof_pool,
            9000,
        )
        .with_sync_status(Arc::new(FakeSync))
        .with_rpc_metrics(rpc_metrics);

        let out = server.render_metrics();

        for name in [
            // chain
            "qfc_block_height",
            "qfc_block_time_seconds",
            "qfc_time_since_last_block_seconds",
            "qfc_reorgs_detected_total",
            "qfc_transactions_total",
            // mempool
            "qfc_mempool_size",
            "qfc_mempool_oldest_tx_age_seconds",
            // network / sync
            "qfc_peer_count",
            "qfc_sync_syncing",
            "qfc_sync_highest_peer_block",
            "qfc_sync_lag_blocks",
            "qfc_sync_pending_blocks",
            // rpc (rendered from the shared registry)
            "qfc_rpc_requests_in_flight",
            "qfc_rpc_requests_total",
            "qfc_rpc_request_duration_seconds_bucket",
            "qfc_rpc_errors_total",
        ] {
            assert!(
                out.contains(name),
                "missing metric `{name}` in exporter output:\n{out}"
            );
        }
    }
}
