//! qfc-watchdog binary — out-of-process, detection-first watchdog for one
//! qfc-node (SRE T6). See docs/WATCHDOG.md for the design, detector catalog,
//! gate semantics, and deployment examples.
//!
//! One watchdog watches one node; run N watchdogs for N nodes.

use anyhow::{Context, Result};
use clap::Parser;
use qfc_watchdog::detector::{DetectorConfig, DetectorId, Engine, MetricsSnapshot};
use qfc_watchdog::exporter::{self, WatchdogMetrics};
use qfc_watchdog::gate::{Decision, Gate, GateConfig};
use qfc_watchdog::probe;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Out-of-process, detection-first watchdog for one qfc-node (SRE T6).
///
/// Watches a node through its Prometheus /metrics endpoint and RPC port,
/// exports a health score + per-detector gauges on its own /metrics
/// endpoint, and — only when --action-cmd is set — may execute a single
/// remediation command behind strict safety gates.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Metrics endpoint of the watched node (qfc-node --metrics-addr).
    #[arg(
        long,
        default_value = "127.0.0.1:6060",
        env = "QFC_WATCHDOG_NODE_METRICS_ADDR"
    )]
    node_metrics_addr: SocketAddr,

    /// RPC endpoint of the watched node (qfc-node --rpc-addr), probed with
    /// eth_blockNumber.
    #[arg(
        long,
        default_value = "127.0.0.1:8545",
        env = "QFC_WATCHDOG_NODE_RPC_ADDR"
    )]
    node_rpc_addr: SocketAddr,

    /// The watchdog's OWN Prometheus listen address.
    #[arg(
        long,
        default_value = "0.0.0.0:6061",
        env = "QFC_WATCHDOG_METRICS_ADDR"
    )]
    watchdog_metrics_addr: SocketAddr,

    /// Seconds between polls.
    #[arg(long, default_value_t = 15, env = "QFC_WATCHDOG_POLL_INTERVAL_SECS")]
    poll_interval_secs: u64,

    /// Timeout for each scrape / RPC probe.
    #[arg(long, default_value_t = 5, env = "QFC_WATCHDOG_PROBE_TIMEOUT_SECS")]
    probe_timeout_secs: u64,

    /// Log level (error/warn/info/debug/trace).
    #[arg(long, default_value = "info", env = "QFC_WATCHDOG_LOG_LEVEL")]
    log_level: String,

    // --- detector thresholds (defaults documented in docs/WATCHDOG.md) ---
    /// Block-production stall: head age (s) above which a height-frozen,
    /// non-syncing node counts as stalled.
    #[arg(
        long,
        default_value_t = 120.0,
        env = "QFC_WATCHDOG_STALL_THRESHOLD_SECS"
    )]
    stall_threshold_secs: f64,

    /// Block-production stall: window (s) over which the height must be
    /// unchanged.
    #[arg(long, default_value_t = 120, env = "QFC_WATCHDOG_STALL_WINDOW_SECS")]
    stall_window_secs: u64,

    /// Stuck sync: window (s) over which sync lag must not decrease while
    /// syncing.
    #[arg(long, default_value_t = 300, env = "QFC_WATCHDOG_SYNC_WINDOW_SECS")]
    sync_window_secs: u64,

    /// Compaction stall: delayed-write rate must be >0 continuously for this
    /// many seconds.
    #[arg(
        long,
        default_value_t = 120,
        env = "QFC_WATCHDOG_DELAYED_WRITE_SUSTAIN_SECS"
    )]
    delayed_write_sustain_secs: u64,

    /// Compaction stall: pending-compaction debt floor in bytes (default
    /// 8 GiB); debt must exceed this AND be growing.
    #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024, env = "QFC_WATCHDOG_COMPACTION_DEBT_BYTES")]
    compaction_debt_bytes: u64,

    /// Compaction stall: window (s) for the debt-growth comparison.
    #[arg(
        long,
        default_value_t = 600,
        env = "QFC_WATCHDOG_COMPACTION_WINDOW_SECS"
    )]
    compaction_window_secs: u64,

    /// Consecutive failed /metrics scrapes before metrics-down fires.
    #[arg(
        long,
        default_value_t = 3,
        env = "QFC_WATCHDOG_METRICS_DOWN_CONSECUTIVE"
    )]
    metrics_down_consecutive: u32,

    /// Consecutive failed RPC probes before rpc-down fires.
    #[arg(long, default_value_t = 3, env = "QFC_WATCHDOG_RPC_DOWN_CONSECUTIVE")]
    rpc_down_consecutive: u32,

    // --- action mode (OFF by default: pure detection) ---
    /// Shell command to run when an action-eligible detector passes ALL
    /// safety gates, e.g. "docker restart qfc-node-1". Unset = detection-only
    /// mode (the default and the recommended starting point).
    #[arg(long, env = "QFC_WATCHDOG_ACTION_CMD")]
    action_cmd: Option<String>,

    /// Detectors allowed to trigger the action (comma-separated). Known:
    /// block_production_stall, stuck_sync, compaction_stall, metrics_down,
    /// rpc_down. Broaden only after the default has earned trust.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "block_production_stall",
        env = "QFC_WATCHDOG_ACTION_ELIGIBLE"
    )]
    action_eligible: Vec<DetectorId>,

    /// Act only after the eligible detector has been firing for this many
    /// consecutive polls.
    #[arg(
        long,
        default_value_t = 3,
        env = "QFC_WATCHDOG_ACTION_AFTER_CONSECUTIVE"
    )]
    action_after_consecutive: u32,

    /// Sliding-window action budget. When spent, the watchdog logs loudly
    /// and keeps alerting only.
    #[arg(long, default_value_t = 2, env = "QFC_WATCHDOG_MAX_ACTIONS_PER_HOUR")]
    max_actions_per_hour: u32,

    /// Quiet period (s) after any action before another may run.
    #[arg(long, default_value_t = 600, env = "QFC_WATCHDOG_ACTION_COOLDOWN_SECS")]
    action_cooldown_secs: u64,

    /// Never act within this many seconds of watchdog start.
    #[arg(long, default_value_t = 120, env = "QFC_WATCHDOG_STARTUP_GRACE_SECS")]
    startup_grace_secs: u64,

    /// How recent a not-yet-succeeded snapshot-backup attempt must be to
    /// count as in-flight (and veto actions).
    #[arg(
        long,
        default_value_t = 1800,
        env = "QFC_WATCHDOG_SNAPSHOT_INFLIGHT_GRACE_SECS"
    )]
    snapshot_inflight_grace_secs: u64,
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn main() -> Result<()> {
    let args = Args::parse();

    let level: Level = args.log_level.parse().unwrap_or(Level::INFO);
    let subscriber = FmtSubscriber::builder().with_max_level(level).finish();
    tracing::subscriber::set_global_default(subscriber).context("setting tracing subscriber")?;

    info!(
        node_metrics = %args.node_metrics_addr,
        node_rpc = %args.node_rpc_addr,
        poll_interval_secs = args.poll_interval_secs,
        "qfc-watchdog starting (one watchdog watches one node)"
    );
    match &args.action_cmd {
        Some(cmd) => warn!(
            action_cmd = %cmd,
            eligible = %args
                .action_eligible
                .iter()
                .map(|d| d.as_str())
                .collect::<Vec<_>>()
                .join(","),
            after_consecutive = args.action_after_consecutive,
            max_actions_per_hour = args.max_actions_per_hour,
            cooldown_secs = args.action_cooldown_secs,
            startup_grace_secs = args.startup_grace_secs,
            "ACTION MODE ENABLED — remediation command will run behind safety gates"
        ),
        None => info!("detection-only mode (no --action-cmd): alert + health score, never acts"),
    }

    let metrics = Arc::new(WatchdogMetrics::new(args.action_cmd.is_some()));
    exporter::serve(args.watchdog_metrics_addr, metrics.clone());

    let detector_cfg = DetectorConfig {
        stall_threshold_secs: args.stall_threshold_secs,
        stall_window_secs: args.stall_window_secs,
        sync_window_secs: args.sync_window_secs,
        delayed_write_sustain_secs: args.delayed_write_sustain_secs,
        compaction_debt_bytes: args.compaction_debt_bytes,
        compaction_window_secs: args.compaction_window_secs,
        metrics_down_consecutive: args.metrics_down_consecutive,
        rpc_down_consecutive: args.rpc_down_consecutive,
    };
    let mut engine = Engine::new(detector_cfg);

    let mut gate = args.action_cmd.as_ref().map(|_| {
        Gate::new(
            GateConfig {
                eligible: args.action_eligible.clone(),
                after_consecutive: args.action_after_consecutive,
                max_actions_per_hour: args.max_actions_per_hour,
                cooldown_secs: args.action_cooldown_secs,
                startup_grace_secs: args.startup_grace_secs,
                snapshot_inflight_grace_secs: args.snapshot_inflight_grace_secs,
            },
            now_unix_secs(),
        )
    });

    let timeout = Duration::from_secs(args.probe_timeout_secs.max(1));
    let poll_interval = Duration::from_secs(args.poll_interval_secs.max(1));

    loop {
        poll_once(&args, &mut engine, gate.as_mut(), &metrics, timeout);
        std::thread::sleep(poll_interval);
    }
}

fn poll_once(
    args: &Args,
    engine: &mut Engine,
    gate: Option<&mut Gate>,
    metrics: &WatchdogMetrics,
    timeout: Duration,
) {
    let now = now_unix_secs();
    metrics.polls_total.fetch_add(1, Ordering::Relaxed);

    let scrape = match probe::scrape_metrics(args.node_metrics_addr, timeout) {
        Ok(text) => Some(MetricsSnapshot::from_prometheus(&text)),
        Err(e) => {
            metrics
                .scrape_failures_total
                .fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "failed to scrape node /metrics");
            None
        }
    };
    let rpc_ok = match probe::rpc_probe(args.node_rpc_addr, timeout) {
        Ok(_) => true,
        Err(e) => {
            metrics
                .rpc_probe_failures_total
                .fetch_add(1, Ordering::Relaxed);
            warn!(error = %e, "RPC probe (eth_blockNumber) failed");
            false
        }
    };

    let eval = engine.evaluate(now, scrape.as_ref(), rpc_ok);

    // Structured log lines on every fire/clear edge.
    for t in &eval.transitions {
        if t.firing {
            warn!(
                detector = %t.id,
                evidence = %t.evidence,
                health_score = eval.health_score,
                "detector FIRED"
            );
        } else {
            info!(
                detector = %t.id,
                evidence = %t.evidence,
                health_score = eval.health_score,
                "detector cleared"
            );
        }
    }

    metrics.record_evaluation(&eval);

    let Some(gate) = gate else {
        return; // detection-only mode
    };

    match gate.decide(now, &eval) {
        Decision::Idle => {
            metrics.action_budget_exhausted.store(0, Ordering::Relaxed);
        }
        Decision::Hold {
            gate: which,
            reason,
        } => {
            metrics.action_budget_exhausted.store(0, Ordering::Relaxed);
            info!(gate = which, reason = %reason, "action wanted but held by safety gate");
        }
        Decision::BudgetExhausted { evidence } => {
            metrics.action_budget_exhausted.store(1, Ordering::Relaxed);
            error!(
                evidence = %evidence,
                max_actions_per_hour = args.max_actions_per_hour,
                "ACTION BUDGET EXHAUSTED while node is unhealthy — automation is \
                 standing down, a human must look (qfc_watchdog_action_budget_exhausted=1)"
            );
        }
        Decision::Act { detector, evidence } => {
            metrics.action_budget_exhausted.store(0, Ordering::Relaxed);
            let cmd = args
                .action_cmd
                .as_deref()
                .expect("gate exists only with action_cmd");
            warn!(
                detector = %detector,
                action_cmd = %cmd,
                evidence = %evidence,
                "executing remediation action"
            );
            gate.record_action(now); // spent even if the command fails
            metrics.actions_total.fetch_add(1, Ordering::Relaxed);
            metrics.action_last_unix.store(now, Ordering::Relaxed);
            match run_action(cmd) {
                Ok((status_ok, output)) if status_ok => {
                    info!(output = %output, "remediation action completed");
                }
                Ok((_, output)) => {
                    metrics
                        .action_failures_total
                        .fetch_add(1, Ordering::Relaxed);
                    error!(output = %output, "remediation action exited non-zero");
                }
                Err(e) => {
                    metrics
                        .action_failures_total
                        .fetch_add(1, Ordering::Relaxed);
                    error!(error = %e, "remediation action failed to spawn");
                }
            }
        }
    }
}

/// Run the action command through `sh -c`; returns (exit-ok, trimmed output).
fn run_action(cmd: &str) -> Result<(bool, String)> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .context("spawning action command")?;
    let mut text = String::new();
    text.push_str(String::from_utf8_lossy(&out.stdout).trim());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push_str(" | stderr: ");
        } else {
            text.push_str("stderr: ");
        }
        text.push_str(stderr);
    }
    if text.len() > 2000 {
        let mut end = 2000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Ok((out.status.success(), text))
}
