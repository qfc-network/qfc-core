//! Prometheus metrics HTTP server
//!
//! Lightweight `/metrics` endpoint using `tiny_http` on a background `std::thread`.
//! Each scrape queries live state from shared `Arc` handles.

use crate::snapshot_backup::SnapshotBackupMetrics;
use parking_lot::{Mutex, RwLock};
use qfc_ai_coordinator::ProofPool;
use qfc_chain::Chain;
use qfc_consensus::ConsensusEngine;
use qfc_crypto::blake3_hash;
use qfc_mempool::Mempool;
use qfc_network::NetworkService;
use qfc_rpc::metrics::RpcMetrics;
use qfc_rpc::SyncStatusProvider;
use qfc_storage::{cf, props, Database, Histogram, Ticker};
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
    /// T2.1: handle to the live RocksDB for statistics/property export
    storage: Option<Database>,
    /// T4.2: backup-freshness metrics, shared with the snapshot backup task.
    /// Only present when backups are enabled (--snapshot-interval-secs).
    snapshot_backup: Option<Arc<SnapshotBackupMetrics>>,
    /// T5: AI task pool, for quota/cost-attribution metrics.
    task_pool: Option<Arc<RwLock<qfc_ai_coordinator::TaskPool>>>,
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
            storage: None,
            snapshot_backup: None,
            task_pool: None,
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

    /// Attach the live database for RocksDB statistics/property export (T2.1).
    /// `Database` is a cheap `Arc` clone of the node's open DB.
    pub fn with_storage(mut self, storage: Database) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Attach the backup-freshness metrics shared with the snapshot backup
    /// task (T4.2). Only call when backups are enabled, so nodes without
    /// backups simply do not export `qfc_snapshot_*` (alerts go to no-data
    /// instead of firing on age 0).
    pub fn with_snapshot_backup(mut self, metrics: Arc<SnapshotBackupMetrics>) -> Self {
        self.snapshot_backup = Some(metrics);
        self
    }

    /// Attach the shared AI task pool for quota/cost-attribution metrics
    /// (T5). Cheap `Arc` clone of the pool shared with RPC and the producer.
    pub fn with_task_pool(mut self, task_pool: Arc<RwLock<qfc_ai_coordinator::TaskPool>>) -> Self {
        self.task_pool = Some(task_pool);
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

        // --- storage (T2.1) ---
        if let Some(db) = &self.storage {
            Self::render_storage_metrics(db, &mut out);
        }

        // --- snapshot backups (T4.2) ---
        if let Some(snap) = &self.snapshot_backup {
            Self::render_snapshot_backup_metrics(snap, &mut out);
        }

        // --- AI task pool quotas + cost attribution (T5) ---
        if let Some(pool) = &self.task_pool {
            let snapshot = pool.read().quota_metrics();
            Self::render_ai_quota_metrics(&snapshot, &mut out);
        }

        // --- hot-key / hot-account analytics (T8) ---
        self.render_hot_key_metrics(&mut out);

        out
    }

    /// T5: quota/cost-attribution metrics. Labeled by priority **tier**
    /// (`tenant_tier` ∈ 0/1/2) rather than tenant address — tenant
    /// cardinality is unbounded, tiers are fixed at three. Per-tenant
    /// detail is in the periodic cost report (structured log, target
    /// `qfc::ai_cost`) and the `TaskPool::last_cost_report` accessor.
    /// `qfc_ai_cost_report_last_timestamp_seconds` follows the T4.2
    /// freshness convention: 0 = never since start, alert on age in PromQL.
    fn render_ai_quota_metrics(snapshot: &qfc_ai_coordinator::AiQuotaMetrics, out: &mut String) {
        let enabled: u8 = if snapshot.quotas_enabled { 1 } else { 0 };
        let _ = writeln!(
            out,
            "# HELP qfc_ai_quotas_enabled Whether per-tenant AI task quotas are enforced (--ai-quotas). Accounting metrics below are exported regardless."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_quotas_enabled gauge");
        let _ = writeln!(out, "qfc_ai_quotas_enabled {enabled}");

        let _ = writeln!(
            out,
            "# HELP qfc_ai_pending_tasks Tasks waiting in the AI task pool (pool-pressure shedding input)."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_pending_tasks gauge");
        let _ = writeln!(out, "qfc_ai_pending_tasks {}", snapshot.pending_tasks);

        let _ = writeln!(
            out,
            "# HELP qfc_ai_tasks_submitted_total Public AI tasks admitted to the pool, by tenant priority tier (0=lowest/shed first, 2=highest)."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_tasks_submitted_total counter");
        for (tier, v) in snapshot.submitted_by_tier.iter().enumerate() {
            let _ = writeln!(
                out,
                "qfc_ai_tasks_submitted_total{{tenant_tier=\"{tier}\"}} {v}"
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_ai_tasks_rejected_total AI task submissions rejected by quota admission, by violated limit."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_tasks_rejected_total counter");
        for (reason, v) in snapshot.rejected_by_reason.iter() {
            let _ = writeln!(
                out,
                "qfc_ai_tasks_rejected_total{{reason=\"{reason}\"}} {v}"
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_ai_flops_metered_total estimated_flops metered on completed public tasks, by tenant priority tier."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_flops_metered_total counter");
        for (tier, v) in snapshot.flops_metered_by_tier.iter().enumerate() {
            let _ = writeln!(
                out,
                "qfc_ai_flops_metered_total{{tenant_tier=\"{tier}\"}} {v}"
            );
        }

        let _ = writeln!(
            out,
            "# HELP qfc_ai_tenant_inflight Public tasks currently Pending/Assigned, summed per tenant priority tier."
        );
        let _ = writeln!(out, "# TYPE qfc_ai_tenant_inflight gauge");
        for (tier, v) in snapshot.inflight_by_tier.iter().enumerate() {
            let _ = writeln!(out, "qfc_ai_tenant_inflight{{tenant_tier=\"{tier}\"}} {v}");
        }

        let ts_secs = snapshot.cost_report_unix_ms / 1000;
        let _ = writeln!(
            out,
            "# HELP qfc_ai_cost_report_last_timestamp_seconds Unix time of the last AI cost report. 0 = never since start; alert on age."
        );
        let _ = writeln!(
            out,
            "# TYPE qfc_ai_cost_report_last_timestamp_seconds gauge"
        );
        let _ = writeln!(out, "qfc_ai_cost_report_last_timestamp_seconds {ts_secs}");
    }

    /// T4.2: backup-freshness metrics. Only rendered when the snapshot
    /// backup task is enabled (`--snapshot-interval-secs`). The alertable
    /// signal is AGE — `time() - qfc_snapshot_last_success_timestamp_seconds`
    /// — not per-run success/failure (see docs/observability/alert-rules.yaml,
    /// group `qfc-backup-freshness`). Timestamps are 0 until the first
    /// attempt/success after process start, which PromQL evaluates as
    /// "infinitely stale" — exactly right for a node whose backups never ran.
    fn render_snapshot_backup_metrics(snap: &SnapshotBackupMetrics, out: &mut String) {
        let last_success = snap.last_success_unix.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP qfc_snapshot_last_success_timestamp_seconds Unix time of the last fully successful snapshot backup (snapshot + archive + upload). 0 = never since start."
        );
        let _ = writeln!(
            out,
            "# TYPE qfc_snapshot_last_success_timestamp_seconds gauge"
        );
        let _ = writeln!(
            out,
            "qfc_snapshot_last_success_timestamp_seconds {last_success}"
        );

        let last_attempt = snap.last_attempt_unix.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP qfc_snapshot_last_attempt_timestamp_seconds Unix time of the last snapshot backup attempt, successful or not. 0 = never since start."
        );
        let _ = writeln!(
            out,
            "# TYPE qfc_snapshot_last_attempt_timestamp_seconds gauge"
        );
        let _ = writeln!(
            out,
            "qfc_snapshot_last_attempt_timestamp_seconds {last_attempt}"
        );

        let failures = snap.failures_total.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP qfc_snapshot_failures_total Snapshot backup runs that failed (snapshot, archive, or upload step)."
        );
        let _ = writeln!(out, "# TYPE qfc_snapshot_failures_total counter");
        let _ = writeln!(out, "qfc_snapshot_failures_total {failures}");

        let duration = snap.last_duration_secs();
        let _ = writeln!(
            out,
            "# HELP qfc_snapshot_duration_seconds Duration of the last successful snapshot backup run (checkpoint + tar + upload)."
        );
        let _ = writeln!(out, "# TYPE qfc_snapshot_duration_seconds gauge");
        let _ = writeln!(out, "qfc_snapshot_duration_seconds {duration:.3}");

        let size = snap.last_size_bytes.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP qfc_snapshot_size_bytes Size of the last successfully produced snapshot archive (.tar.gz)."
        );
        let _ = writeln!(out, "# TYPE qfc_snapshot_size_bytes gauge");
        let _ = writeln!(out, "qfc_snapshot_size_bytes {size}");
    }

    /// T2.1: RocksDB metrics, two layers:
    ///
    /// 1. **Properties** (always exported): computed on demand from live
    ///    engine state, no statistics overhead. Per-CF memtable size,
    ///    pending-compaction bytes, immutable memtables, and L0 file count
    ///    across all 18 CFs, plus DB-wide block-cache usage and the two
    ///    write-stall condition gauges.
    /// 2. **Statistics tickers/histograms** (only when the DB was opened with
    ///    `enable_statistics`, i.e. `--db-statistics`): a deliberately small,
    ///    alertable set — stall time, block-cache and bloom effectiveness,
    ///    user/compaction/flush byte volume, and WAL sync count/bytes/latency.
    ///    `qfc_storage_statistics_enabled` says which mode the node is in.
    fn render_storage_metrics(db: &Database, out: &mut String) {
        // --- properties: per-CF gauges (work without statistics) ---
        let cf_props: &[(&str, &str, &str)] = &[
            (
                props::CUR_SIZE_ALL_MEM_TABLES,
                "qfc_rocksdb_memtable_bytes",
                "Approximate size of active and unflushed immutable memtables (bytes).",
            ),
            (
                props::ESTIMATE_PENDING_COMPACTION_BYTES,
                "qfc_rocksdb_pending_compaction_bytes",
                "Estimated bytes compaction must rewrite to catch up (compaction debt).",
            ),
            (
                props::NUM_IMMUTABLE_MEM_TABLE,
                "qfc_rocksdb_immutable_memtables",
                "Immutable memtables not yet flushed.",
            ),
            (
                props::NUM_FILES_AT_LEVEL0,
                "qfc_rocksdb_l0_files",
                "SST files at level 0 (write stalls trigger when this grows).",
            ),
        ];
        for (prop, name, help) in cf_props {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            for cf_name in cf::ALL {
                if let Ok(Some(v)) = db.property_int_cf(cf_name, prop) {
                    let _ = writeln!(out, "{name}{{cf=\"{cf_name}\"}} {v}");
                }
            }
        }

        // --- properties: DB-wide gauges ---
        let db_props: &[(&str, &str, &str)] = &[
            (
                props::BLOCK_CACHE_USAGE,
                "qfc_rocksdb_block_cache_usage_bytes",
                "Memory used by the shared block cache (bytes).",
            ),
            (
                props::IS_WRITE_STOPPED,
                "qfc_rocksdb_write_stopped",
                "1 if RocksDB has completely stopped writes (hard stall), else 0.",
            ),
            (
                props::ACTUAL_DELAYED_WRITE_RATE,
                "qfc_rocksdb_actual_delayed_write_rate",
                "Current delayed-write rate in bytes/s (0 = writes not delayed).",
            ),
        ];
        for (prop, name, help) in db_props {
            if let Ok(Some(v)) = db.property_int(prop) {
                let _ = writeln!(out, "# HELP {name} {help}");
                let _ = writeln!(out, "# TYPE {name} gauge");
                let _ = writeln!(out, "{name} {v}");
            }
        }

        // --- statistics (tickers + histograms, gated on --db-statistics) ---
        let enabled: u8 = if db.statistics_enabled() { 1 } else { 0 };
        let _ = writeln!(
            out,
            "# HELP qfc_storage_statistics_enabled Whether RocksDB statistics collection is on (--db-statistics). Ticker metrics below are absent when 0."
        );
        let _ = writeln!(out, "# TYPE qfc_storage_statistics_enabled gauge");
        let _ = writeln!(out, "qfc_storage_statistics_enabled {enabled}");

        if !db.statistics_enabled() {
            return;
        }

        let tickers: &[(Ticker, &str, &str)] = &[
            (
                Ticker::StallMicros,
                "qfc_rocksdb_stall_micros_total",
                "Cumulative microseconds writer threads spent stalled by compaction/flush backpressure.",
            ),
            (
                Ticker::BlockCacheHit,
                "qfc_rocksdb_block_cache_hits_total",
                "Block cache hits (data + index + filter blocks).",
            ),
            (
                Ticker::BlockCacheMiss,
                "qfc_rocksdb_block_cache_misses_total",
                "Block cache misses (data + index + filter blocks).",
            ),
            (
                Ticker::BloomFilterUseful,
                "qfc_rocksdb_bloom_filter_useful_total",
                "Point lookups where a bloom filter avoided a data-block read.",
            ),
            (
                Ticker::BytesWritten,
                "qfc_rocksdb_bytes_written_total",
                "Bytes written by user writes (WriteBatch payload).",
            ),
            (
                Ticker::BytesRead,
                "qfc_rocksdb_bytes_read_total",
                "Bytes of values returned to user reads (Get).",
            ),
            (
                Ticker::CompactReadBytes,
                "qfc_rocksdb_compaction_read_bytes_total",
                "Bytes read by compaction.",
            ),
            (
                Ticker::CompactWriteBytes,
                "qfc_rocksdb_compaction_write_bytes_total",
                "Bytes written by compaction.",
            ),
            (
                Ticker::FlushWriteBytes,
                "qfc_rocksdb_flush_write_bytes_total",
                "Bytes written by memtable flushes.",
            ),
            (
                Ticker::WalFileSynced,
                "qfc_rocksdb_wal_syncs_total",
                "Number of WAL fsyncs.",
            ),
            (
                Ticker::WalFileBytes,
                "qfc_rocksdb_wal_bytes_total",
                "Bytes written to the WAL.",
            ),
        ];
        for (ticker, name, help) in tickers {
            if let Some(v) = db.statistics_ticker(*ticker) {
                let _ = writeln!(out, "# HELP {name} {help}");
                let _ = writeln!(out, "# TYPE {name} counter");
                let _ = writeln!(out, "{name} {v}");
            }
        }

        // WAL sync latency: cumulative sum/count (rate(sum)/rate(count) gives
        // the windowed mean) plus the engine-side p99 since DB open.
        if let Some(h) = db.statistics_histogram(Histogram::WalFileSyncMicros) {
            let sum_secs = h.sum as f64 / 1e6;
            let p99_secs = h.p99 / 1e6;
            let _ = writeln!(
                out,
                "# HELP qfc_rocksdb_wal_sync_duration_seconds_sum Cumulative time spent in WAL fsync."
            );
            let _ = writeln!(
                out,
                "# TYPE qfc_rocksdb_wal_sync_duration_seconds_sum counter"
            );
            let _ = writeln!(
                out,
                "qfc_rocksdb_wal_sync_duration_seconds_sum {sum_secs:.6}"
            );
            let _ = writeln!(
                out,
                "# HELP qfc_rocksdb_wal_sync_duration_seconds_count Number of WAL fsyncs measured."
            );
            let _ = writeln!(
                out,
                "# TYPE qfc_rocksdb_wal_sync_duration_seconds_count counter"
            );
            let _ = writeln!(
                out,
                "qfc_rocksdb_wal_sync_duration_seconds_count {}",
                h.count
            );
            let _ = writeln!(
                out,
                "# HELP qfc_rocksdb_wal_sync_duration_seconds_p99 p99 WAL fsync latency since DB open (engine-side histogram)."
            );
            let _ = writeln!(
                out,
                "# TYPE qfc_rocksdb_wal_sync_duration_seconds_p99 gauge"
            );
            let _ = writeln!(
                out,
                "qfc_rocksdb_wal_sync_duration_seconds_p99 {p99_secs:.6}"
            );
        }
    }

    /// T8: hot-key / hot-account analytics.
    ///
    /// **Cardinality:** the top-N hot *identities* (raw keys, account
    /// addresses, code hashes) are deliberately NOT exported as Prometheus
    /// labels — the hot set churns window-to-window, which would leak
    /// unbounded, short-lived time series (same reasoning as T5's tier-only
    /// labels). What we export is bounded and stable: per-CF traffic
    /// estimates (one series per `cf::ALL` entry) and per-CF / DB-wide skew
    /// gauges (the hottest entry's *count*, without its identity). The actual
    /// top-N identities live in `Database::hot_key_report` /
    /// `StateDB::hot_account_report` and the findings in
    /// `docs/profiling/HOT-KEYS.md`.
    ///
    /// **Accuracy caveat:** per-CF key stats are complete (the storage
    /// sampler is shared across every `Database` clone). Per-account stats
    /// cover only the chain's persistent state handle (`chain.state()`); the
    /// sync path's transient `state_at` handles carry their own trackers and
    /// are not aggregated here.
    fn render_hot_key_metrics(&self, out: &mut String) {
        const TOP_N: usize = 16;

        let storage_report = self
            .storage
            .as_ref()
            .and_then(|db| db.hot_key_report(TOP_N));

        let enabled: u8 = if storage_report.is_some() { 1 } else { 0 };
        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_sampling_enabled 1 if T8 hot-key sampling is active, else 0."
        );
        let _ = writeln!(out, "# TYPE qfc_hot_key_sampling_enabled gauge");
        let _ = writeln!(out, "qfc_hot_key_sampling_enabled {enabled}");

        let Some(report) = storage_report else {
            return;
        };

        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_sampling_rate Effective 1-in-N sampling rate (power of two)."
        );
        let _ = writeln!(out, "# TYPE qfc_hot_key_sampling_rate gauge");
        let _ = writeln!(out, "qfc_hot_key_sampling_rate {}", report.sampling_rate);

        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_window_start_timestamp_seconds Unix time the current sampling window opened (0 = never reset)."
        );
        let _ = writeln!(
            out,
            "# TYPE qfc_hot_key_window_start_timestamp_seconds gauge"
        );
        let _ = writeln!(
            out,
            "qfc_hot_key_window_start_timestamp_seconds {}",
            report.window_start_unix_ms / 1000
        );

        // Per-CF estimated traffic (bounded by cf::ALL). Gauges: windowed
        // estimates that reset with the sampling window.
        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_estimated_reads Estimated read ops per CF this window (sampled * rate)."
        );
        let _ = writeln!(out, "# TYPE qfc_hot_key_estimated_reads gauge");
        for cf in &report.cfs {
            let _ = writeln!(
                out,
                "qfc_hot_key_estimated_reads{{cf=\"{}\"}} {}",
                cf.cf, cf.estimated_reads
            );
        }
        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_estimated_writes Estimated write ops per CF this window (sampled * rate)."
        );
        let _ = writeln!(out, "# TYPE qfc_hot_key_estimated_writes gauge");
        for cf in &report.cfs {
            let _ = writeln!(
                out,
                "qfc_hot_key_estimated_writes{{cf=\"{}\"}} {}",
                cf.cf, cf.estimated_writes
            );
        }

        // Per-CF skew: the hottest single key's estimated count (no identity).
        let _ = writeln!(
            out,
            "# HELP qfc_hot_key_top_estimated_count Estimated access count of the hottest key in each CF (skew indicator)."
        );
        let _ = writeln!(out, "# TYPE qfc_hot_key_top_estimated_count gauge");
        for cf in &report.cfs {
            if let Some(top) = cf.top_keys.first() {
                let _ = writeln!(
                    out,
                    "qfc_hot_key_top_estimated_count{{cf=\"{}\"}} {}",
                    cf.cf, top.estimated_count
                );
            }
        }

        // Hot-account analytics from the chain's persistent state handle.
        if let Some(acct) = self.chain.state().hot_account_report(TOP_N) {
            let aggregates: &[(&str, &str, u64)] = &[
                (
                    "qfc_hot_account_estimated_reads",
                    "Estimated account reads this window (sampled * rate).",
                    acct.estimated_account_reads,
                ),
                (
                    "qfc_hot_account_estimated_writes",
                    "Estimated account writes this window (sampled * rate).",
                    acct.estimated_account_writes,
                ),
                (
                    "qfc_hot_account_estimated_code_reads",
                    "Estimated contract-code reads this window (sampled * rate).",
                    acct.estimated_code_reads,
                ),
            ];
            for (name, help, value) in aggregates {
                let _ = writeln!(out, "# HELP {name} {help}");
                let _ = writeln!(out, "# TYPE {name} gauge");
                let _ = writeln!(out, "{name} {value}");
            }

            let _ = writeln!(
                out,
                "# HELP qfc_hot_account_top_estimated_count Estimated access count of the hottest account (skew indicator)."
            );
            let _ = writeln!(out, "# TYPE qfc_hot_account_top_estimated_count gauge");
            let top_account = acct.top_accounts.first().map_or(0, |a| a.estimated_count);
            let _ = writeln!(out, "qfc_hot_account_top_estimated_count {top_account}");

            let _ = writeln!(
                out,
                "# HELP qfc_hot_code_top_estimated_count Estimated read count of the hottest contract bytecode (skew indicator)."
            );
            let _ = writeln!(out, "# TYPE qfc_hot_code_top_estimated_count gauge");
            let top_code = acct.top_code.first().map_or(0, |c| c.estimated_count);
            let _ = writeln!(out, "qfc_hot_code_top_estimated_count {top_code}");
        }
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

    /// Build a `MetricsServer` over a temp DB (optionally with RocksDB
    /// statistics) wired the same way `main.rs` does it. The returned
    /// `TempDir` must stay alive as long as the server.
    fn test_server(
        enable_statistics: bool,
        hot_key_sampling: Option<u32>,
    ) -> (MetricsServer, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Database::open(StorageConfig {
            path: tmp.path().join("db"),
            create_if_missing: true,
            enable_statistics,
            hot_key_sampling,
            ..Default::default()
        })
        .unwrap();
        let consensus = Arc::new(ConsensusEngine::new(ConsensusConfig::default()));
        let chain = Arc::new(
            Chain::new(
                db.clone(),
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

        // T5: task pool with quota/cost activity (one submission admitted,
        // one rejected by QPS, one completion metered, one cost report).
        let task_pool = {
            let mut pool = qfc_ai_coordinator::TaskPool::new();
            let cfg = qfc_ai_coordinator::QuotaConfig::from_json_str(
                r#"{ "default_tier": { "max_qps": 1.0, "burst": 1 } }"#,
            )
            .unwrap();
            pool.set_quota_config(Some(cfg));
            let make_task = |seed: &[u8]| {
                let input_hash = qfc_crypto::blake3_hash(seed);
                qfc_inference::InferenceTask::new(
                    qfc_crypto::blake3_hash(&[seed, b"id"].concat()),
                    1,
                    qfc_inference::task::ComputeTaskType::Embedding {
                        model_id: qfc_inference::task::ModelId::new("test", "v1"),
                        input_hash,
                    },
                    vec![],
                    0,
                    u64::MAX,
                )
            };
            let tenant = qfc_types::Address::new([0xAB; 20]);
            let task = make_task(b"metrics-t5");
            let task_id = task.task_id;
            pool.try_submit_public_task(tenant, task, 1_000, 0).unwrap();
            // Second submission at the same instant exceeds the 1-token burst.
            assert!(pool
                .try_submit_public_task(tenant, make_task(b"metrics-t5-b"), 1_000, 0)
                .is_err());
            pool.complete_public_task(
                &task_id,
                qfc_ai_coordinator::task_pool::ResultStorage::Inline(vec![]),
                qfc_types::Address::new([0xCD; 20]),
                7,
            );
            pool.generate_cost_report(1_765_000_000_000);
            Arc::new(RwLock::new(pool))
        };

        // T4.2: backup-freshness metrics as the backup task would update them.
        let snapshot_metrics = Arc::new(SnapshotBackupMetrics::default());
        snapshot_metrics
            .last_attempt_unix
            .store(1_765_000_000, Ordering::Relaxed);
        snapshot_metrics
            .last_success_unix
            .store(1_765_000_000, Ordering::Relaxed);
        snapshot_metrics
            .last_size_bytes
            .store(4096, Ordering::Relaxed);

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
        .with_rpc_metrics(rpc_metrics)
        .with_storage(db)
        .with_snapshot_backup(snapshot_metrics)
        .with_task_pool(task_pool);
        (server, tmp)
    }

    /// T2.2/T2.1: the exporter output must contain every metric name
    /// referenced by docs/observability/ (dashboard + alert rules).
    #[test]
    fn render_metrics_includes_expected_metric_names() {
        let (server, _tmp) = test_server(true, Some(1));
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
            // storage (T2.1) — properties, exported unconditionally
            "qfc_rocksdb_memtable_bytes",
            "qfc_rocksdb_pending_compaction_bytes",
            "qfc_rocksdb_immutable_memtables",
            "qfc_rocksdb_l0_files",
            "qfc_rocksdb_block_cache_usage_bytes",
            "qfc_rocksdb_write_stopped",
            "qfc_rocksdb_actual_delayed_write_rate",
            "qfc_storage_statistics_enabled",
            // storage (T2.1) — statistics tickers (--db-statistics on)
            "qfc_rocksdb_stall_micros_total",
            "qfc_rocksdb_block_cache_hits_total",
            "qfc_rocksdb_block_cache_misses_total",
            "qfc_rocksdb_bloom_filter_useful_total",
            "qfc_rocksdb_bytes_written_total",
            "qfc_rocksdb_bytes_read_total",
            "qfc_rocksdb_compaction_read_bytes_total",
            "qfc_rocksdb_compaction_write_bytes_total",
            "qfc_rocksdb_flush_write_bytes_total",
            "qfc_rocksdb_wal_syncs_total",
            "qfc_rocksdb_wal_bytes_total",
            "qfc_rocksdb_wal_sync_duration_seconds_sum",
            "qfc_rocksdb_wal_sync_duration_seconds_count",
            "qfc_rocksdb_wal_sync_duration_seconds_p99",
            // snapshot backups (T4.2) — backup-freshness metrics
            "qfc_snapshot_last_success_timestamp_seconds",
            "qfc_snapshot_last_attempt_timestamp_seconds",
            "qfc_snapshot_failures_total",
            "qfc_snapshot_duration_seconds",
            "qfc_snapshot_size_bytes",
            // AI task pool quotas + cost attribution (T5)
            "qfc_ai_quotas_enabled",
            "qfc_ai_pending_tasks",
            "qfc_ai_tasks_submitted_total",
            "qfc_ai_tasks_rejected_total",
            "qfc_ai_flops_metered_total",
            "qfc_ai_tenant_inflight",
            "qfc_ai_cost_report_last_timestamp_seconds",
            // hot-key / hot-account analytics (T8)
            "qfc_hot_key_sampling_enabled",
            "qfc_hot_key_sampling_rate",
            "qfc_hot_key_window_start_timestamp_seconds",
            "qfc_hot_key_estimated_reads",
            "qfc_hot_key_estimated_writes",
            "qfc_hot_key_top_estimated_count",
            "qfc_hot_account_estimated_reads",
            "qfc_hot_account_estimated_writes",
            "qfc_hot_account_estimated_code_reads",
            "qfc_hot_account_top_estimated_count",
            "qfc_hot_code_top_estimated_count",
        ] {
            assert!(
                out.contains(name),
                "missing metric `{name}` in exporter output:\n{out}"
            );
        }

        assert!(out.contains("qfc_storage_statistics_enabled 1"));
        // T4.2: freshness gauges carry the values the backup task stored.
        assert!(out.contains("qfc_snapshot_last_success_timestamp_seconds 1765000000"));
        assert!(out.contains("qfc_snapshot_failures_total 0"));
        assert!(out.contains("qfc_snapshot_size_bytes 4096"));
        // T5: tier/reason labels carry the quota activity seeded above
        // (default tier = priority 1; one QPS rejection; one completed
        // Embedding task = 1 GFLOP metered; report at 1_765_000_000_000 ms).
        assert!(out.contains("qfc_ai_quotas_enabled 1"));
        assert!(out.contains("qfc_ai_tasks_submitted_total{tenant_tier=\"1\"} 1"));
        assert!(out.contains("qfc_ai_tasks_submitted_total{tenant_tier=\"0\"} 0"));
        assert!(out.contains("qfc_ai_tasks_rejected_total{reason=\"qps\"} 1"));
        assert!(out.contains("qfc_ai_tasks_rejected_total{reason=\"pool_pressure\"} 0"));
        assert!(out.contains("qfc_ai_tasks_rejected_total{reason=\"in_flight\"} 0"));
        assert!(out.contains("qfc_ai_tasks_rejected_total{reason=\"flops_budget\"} 0"));
        assert!(out.contains("qfc_ai_flops_metered_total{tenant_tier=\"1\"} 1000000000"));
        assert!(out.contains("qfc_ai_tenant_inflight{tenant_tier=\"1\"} 0"));
        assert!(out.contains("qfc_ai_cost_report_last_timestamp_seconds 1765000000"));
        // T8: sampling enabled at 1-in-1; genesis writes populate the reports.
        assert!(out.contains("qfc_hot_key_sampling_enabled 1"));
        assert!(out.contains("qfc_hot_key_sampling_rate 1"));
        // Per-CF metrics carry a {cf="..."} label for every CF in cf::ALL.
        for cf_name in cf::ALL {
            assert!(
                out.contains(&format!("qfc_rocksdb_memtable_bytes{{cf=\"{cf_name}\"}}")),
                "missing per-CF memtable metric for `{cf_name}`"
            );
        }
    }

    /// T2.1: with statistics off (the default), property metrics are still
    /// exported but ticker/histogram metrics degrade to a single
    /// `qfc_storage_statistics_enabled 0` gauge.
    #[test]
    fn render_metrics_degrades_without_statistics() {
        let (server, _tmp) = test_server(false, Some(1));
        let out = server.render_metrics();

        assert!(out.contains("qfc_storage_statistics_enabled 0"));

        // Property-based metrics do not need statistics.
        for name in [
            "qfc_rocksdb_memtable_bytes",
            "qfc_rocksdb_pending_compaction_bytes",
            "qfc_rocksdb_immutable_memtables",
            "qfc_rocksdb_l0_files",
            "qfc_rocksdb_block_cache_usage_bytes",
            "qfc_rocksdb_write_stopped",
        ] {
            assert!(
                out.contains(name),
                "property metric `{name}` should not be gated"
            );
        }

        // Ticker/histogram metrics must be absent.
        for name in [
            "qfc_rocksdb_stall_micros_total",
            "qfc_rocksdb_block_cache_hits_total",
            "qfc_rocksdb_wal_sync_duration_seconds_sum",
        ] {
            assert!(
                !out.contains(name),
                "ticker metric `{name}` must not render without statistics"
            );
        }
    }

    /// T8: with sampling off, only the single `enabled 0` gauge renders; the
    /// detail/skew metrics must be absent (no zero-valued noise, no panic
    /// from the `None` reports).
    #[test]
    fn render_metrics_hot_keys_disabled() {
        let (server, _tmp) = test_server(false, None);
        let out = server.render_metrics();

        assert!(out.contains("qfc_hot_key_sampling_enabled 0"));
        for name in [
            "qfc_hot_key_sampling_rate",
            "qfc_hot_key_estimated_reads",
            "qfc_hot_key_top_estimated_count",
            "qfc_hot_account_estimated_reads",
            "qfc_hot_account_top_estimated_count",
            "qfc_hot_code_top_estimated_count",
        ] {
            assert!(
                !out.contains(name),
                "hot-key detail metric `{name}` must not render when sampling is off"
            );
        }
    }
}
