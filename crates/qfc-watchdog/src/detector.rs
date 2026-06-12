//! Detector engine: turns successive metric samples into firing/cleared
//! detector states plus a 0–100 health score.
//!
//! Pure state machine — the clock is injected (`now_secs`) and samples are
//! plain structs, so every detector and transition is unit-testable with
//! synthetic time (no sleeps). The binary feeds it real scrapes on a poll
//! interval.
//!
//! Sliding windows are evaluated against an in-memory history of successful
//! scrapes. When a scrape fails, the three node-state detectors
//! (block-production stall, stuck sync, compaction stall) **freeze**: they
//! keep their last firing state but their consecutive-poll counters stop
//! advancing, so action gating cannot make progress while the watchdog is
//! blind. Availability detectors (metrics-down, RPC-down) cover that case.

use crate::prom::MetricsText;
use std::collections::VecDeque;
use std::fmt;

/// Identity of each detector. Order in [`DetectorId::ALL`] is the priority
/// order used when several action-eligible detectors fire at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectorId {
    BlockProductionStall,
    StuckSync,
    CompactionStall,
    MetricsDown,
    RpcDown,
}

impl DetectorId {
    pub const ALL: [DetectorId; 5] = [
        DetectorId::BlockProductionStall,
        DetectorId::StuckSync,
        DetectorId::CompactionStall,
        DetectorId::MetricsDown,
        DetectorId::RpcDown,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            DetectorId::BlockProductionStall => "block_production_stall",
            DetectorId::StuckSync => "stuck_sync",
            DetectorId::CompactionStall => "compaction_stall",
            DetectorId::MetricsDown => "metrics_down",
            DetectorId::RpcDown => "rpc_down",
        }
    }

    /// Weight subtracted from the 100-point health score while firing.
    /// Weights sum to more than 100 on purpose; the score clamps at 0.
    pub fn weight(&self) -> u32 {
        match self {
            DetectorId::BlockProductionStall => 40,
            DetectorId::RpcDown => 30,
            DetectorId::MetricsDown => 30,
            DetectorId::CompactionStall => 25,
            DetectorId::StuckSync => 15,
        }
    }
}

impl fmt::Display for DetectorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DetectorId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DetectorId::ALL
            .iter()
            .find(|id| id.as_str() == s)
            .copied()
            .ok_or_else(|| {
                let known: Vec<&str> = DetectorId::ALL.iter().map(|d| d.as_str()).collect();
                format!("unknown detector `{s}` (known: {})", known.join(", "))
            })
    }
}

/// The watchdog-relevant slice of one successful `/metrics` scrape.
/// Every field is optional: nodes export different series depending on
/// flags (e.g. `qfc_snapshot_*` only with `--snapshot-interval-secs`).
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub block_height: Option<u64>,
    pub time_since_last_block_secs: Option<f64>,
    pub syncing: Option<bool>,
    pub sync_lag_blocks: Option<u64>,
    pub write_stopped: Option<bool>,
    pub delayed_write_rate: Option<f64>,
    /// Summed across all column families.
    pub pending_compaction_bytes: Option<u64>,
    pub snapshot_last_attempt_unix: Option<u64>,
    pub snapshot_last_success_unix: Option<u64>,
}

impl MetricsSnapshot {
    /// Parse the qfc-node exporter output (metric names per
    /// crates/qfc-node/src/metrics.rs and docs/observability/README.md).
    pub fn from_prometheus(text: &str) -> Self {
        let m = MetricsText::parse(text);
        let as_u64 = |v: f64| v.max(0.0) as u64;
        Self {
            block_height: m.value("qfc_block_height").map(as_u64),
            time_since_last_block_secs: m.value("qfc_time_since_last_block_seconds"),
            syncing: m.value("qfc_sync_syncing").map(|v| v != 0.0),
            sync_lag_blocks: m.value("qfc_sync_lag_blocks").map(as_u64),
            write_stopped: m.value("qfc_rocksdb_write_stopped").map(|v| v != 0.0),
            delayed_write_rate: m.value("qfc_rocksdb_actual_delayed_write_rate"),
            pending_compaction_bytes: m.sum("qfc_rocksdb_pending_compaction_bytes").map(as_u64),
            snapshot_last_attempt_unix: m
                .value("qfc_snapshot_last_attempt_timestamp_seconds")
                .map(as_u64),
            snapshot_last_success_unix: m
                .value("qfc_snapshot_last_success_timestamp_seconds")
                .map(as_u64),
        }
    }
}

/// Detector thresholds. All overridable from the CLI; defaults documented
/// in docs/WATCHDOG.md.
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// Block-production stall: head age must exceed this (seconds) while the
    /// height is unchanged over `stall_window_secs`. Default 120 — matches
    /// the QfcBlockProductionStalled page alert.
    pub stall_threshold_secs: f64,
    /// Window over which the height must be unchanged. Default 120.
    pub stall_window_secs: u64,
    /// Stuck sync: syncing=1 at both ends of this window and lag has not
    /// decreased. Default 300.
    pub sync_window_secs: u64,
    /// Compaction: delayed-write rate must be >0 continuously for this long.
    /// Default 120.
    pub delayed_write_sustain_secs: u64,
    /// Compaction: total pending-compaction bytes must exceed this AND be
    /// growing over `compaction_window_secs`. Default 8 GiB — matches the
    /// QfcRocksdbCompactionBacklogGrowing floor.
    pub compaction_debt_bytes: u64,
    /// Window for the debt-growth comparison. Default 600.
    pub compaction_window_secs: u64,
    /// Consecutive failed `/metrics` scrapes before metrics-down fires.
    /// Default 3.
    pub metrics_down_consecutive: u32,
    /// Consecutive failed RPC probes before rpc-down fires. Default 3.
    pub rpc_down_consecutive: u32,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            stall_threshold_secs: 120.0,
            stall_window_secs: 120,
            sync_window_secs: 300,
            delayed_write_sustain_secs: 120,
            compaction_debt_bytes: 8 * 1024 * 1024 * 1024,
            compaction_window_secs: 600,
            metrics_down_consecutive: 3,
            rpc_down_consecutive: 3,
        }
    }
}

/// One detector's state after a poll.
#[derive(Debug, Clone)]
pub struct DetectorStatus {
    pub id: DetectorId,
    pub firing: bool,
    /// Consecutive evaluated polls this detector has been firing (frozen
    /// while the watchdog cannot scrape, for the node-state detectors).
    pub consecutive_firing: u32,
    pub evidence: String,
}

/// A fire→clear or clear→fire edge, for structured logging.
#[derive(Debug, Clone)]
pub struct Transition {
    pub id: DetectorId,
    pub firing: bool,
    pub evidence: String,
}

/// Result of one poll: detector states, health score, transitions, and the
/// node context the action gate needs.
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub at_secs: u64,
    pub detectors: Vec<DetectorStatus>,
    /// 0–100. 100 = no detector firing; each firing detector subtracts its
    /// weight; clamped at 0.
    pub health_score: u32,
    pub transitions: Vec<Transition>,
    /// Last-known syncing flag (carried across scrape failures).
    pub syncing: Option<bool>,
    /// Last-known snapshot-backup timestamps (carried across failures) —
    /// the gate uses these to avoid acting while a snapshot is in flight.
    pub snapshot_last_attempt_unix: Option<u64>,
    pub snapshot_last_success_unix: Option<u64>,
}

impl Evaluation {
    pub fn status(&self, id: DetectorId) -> &DetectorStatus {
        self.detectors
            .iter()
            .find(|d| d.id == id)
            .expect("all detectors present")
    }

    pub fn firing(&self, id: DetectorId) -> bool {
        self.status(id).firing
    }

    /// One-line dump of everything firing — attached to action logs.
    pub fn evidence_summary(&self) -> String {
        let firing: Vec<String> = self
            .detectors
            .iter()
            .filter(|d| d.firing)
            .map(|d| format!("{}: {}", d.id, d.evidence))
            .collect();
        if firing.is_empty() {
            format!("health={} (no detector firing)", self.health_score)
        } else {
            format!("health={} [{}]", self.health_score, firing.join("; "))
        }
    }
}

#[derive(Debug, Clone)]
struct HistoryPoint {
    at: u64,
    height: Option<u64>,
    syncing: Option<bool>,
    lag: Option<u64>,
    delayed_rate: Option<f64>,
    pending_compaction: Option<u64>,
}

struct DetectorSlot {
    id: DetectorId,
    firing: bool,
    consecutive_firing: u32,
    evidence: String,
}

/// The detector state machine. Feed it one sample per poll via
/// [`Engine::evaluate`].
pub struct Engine {
    cfg: DetectorConfig,
    history: VecDeque<HistoryPoint>,
    scrape_failures: u32,
    rpc_failures: u32,
    slots: Vec<DetectorSlot>,
    last_syncing: Option<bool>,
    last_snapshot_attempt: Option<u64>,
    last_snapshot_success: Option<u64>,
}

impl Engine {
    pub fn new(cfg: DetectorConfig) -> Self {
        let slots = DetectorId::ALL
            .iter()
            .map(|id| DetectorSlot {
                id: *id,
                firing: false,
                consecutive_firing: 0,
                evidence: String::new(),
            })
            .collect();
        Self {
            cfg,
            history: VecDeque::new(),
            scrape_failures: 0,
            rpc_failures: 0,
            slots,
            last_syncing: None,
            last_snapshot_attempt: None,
            last_snapshot_success: None,
        }
    }

    /// Evaluate one poll. `scrape` is `None` when the `/metrics` endpoint
    /// could not be scraped; `rpc_ok` reports the `eth_blockNumber` probe.
    pub fn evaluate(
        &mut self,
        now_secs: u64,
        scrape: Option<&MetricsSnapshot>,
        rpc_ok: bool,
    ) -> Evaluation {
        // Availability counters.
        if scrape.is_some() {
            self.scrape_failures = 0;
        } else {
            self.scrape_failures = self.scrape_failures.saturating_add(1);
        }
        if rpc_ok {
            self.rpc_failures = 0;
        } else {
            self.rpc_failures = self.rpc_failures.saturating_add(1);
        }

        if let Some(s) = scrape {
            self.history.push_back(HistoryPoint {
                at: now_secs,
                height: s.block_height,
                syncing: s.syncing,
                lag: s.sync_lag_blocks,
                delayed_rate: s.delayed_write_rate,
                pending_compaction: s.pending_compaction_bytes,
            });
            self.prune(now_secs);
            if s.syncing.is_some() {
                self.last_syncing = s.syncing;
            }
            if s.snapshot_last_attempt_unix.is_some() {
                self.last_snapshot_attempt = s.snapshot_last_attempt_unix;
            }
            if s.snapshot_last_success_unix.is_some() {
                self.last_snapshot_success = s.snapshot_last_success_unix;
            }
        }

        // Desired state per detector; `None` = freeze (no fresh data).
        let block = scrape.map(|s| self.eval_block_stall(now_secs, s));
        let sync = scrape.map(|s| self.eval_stuck_sync(now_secs, s));
        let compaction = scrape.map(|s| self.eval_compaction_stall(now_secs, s));
        let metrics_down = Some((
            self.scrape_failures >= self.cfg.metrics_down_consecutive,
            format!(
                "{} consecutive /metrics scrape failures (threshold {})",
                self.scrape_failures, self.cfg.metrics_down_consecutive
            ),
        ));
        let rpc_down = Some((
            self.rpc_failures >= self.cfg.rpc_down_consecutive,
            format!(
                "{} consecutive eth_blockNumber probe failures (threshold {})",
                self.rpc_failures, self.cfg.rpc_down_consecutive
            ),
        ));

        let desired = [block, sync, compaction, metrics_down, rpc_down];
        let mut transitions = Vec::new();
        for (slot, want) in self.slots.iter_mut().zip(desired) {
            let Some((firing, evidence)) = want else {
                continue; // frozen: keep state, do not advance counters
            };
            if firing != slot.firing {
                transitions.push(Transition {
                    id: slot.id,
                    firing,
                    evidence: evidence.clone(),
                });
            }
            slot.consecutive_firing = if firing {
                slot.consecutive_firing.saturating_add(1)
            } else {
                0
            };
            slot.firing = firing;
            slot.evidence = evidence;
        }

        let penalty: u32 = self
            .slots
            .iter()
            .filter(|s| s.firing)
            .map(|s| s.id.weight())
            .sum();
        let health_score = 100u32.saturating_sub(penalty);

        Evaluation {
            at_secs: now_secs,
            detectors: self
                .slots
                .iter()
                .map(|s| DetectorStatus {
                    id: s.id,
                    firing: s.firing,
                    consecutive_firing: s.consecutive_firing,
                    evidence: s.evidence.clone(),
                })
                .collect(),
            health_score,
            transitions,
            syncing: self.last_syncing,
            snapshot_last_attempt_unix: self.last_snapshot_attempt,
            snapshot_last_success_unix: self.last_snapshot_success,
        }
    }

    /// Most recent history point at or before `cutoff` (the sliding-window
    /// baseline). `None` while the window is not yet covered.
    fn point_at_or_before(&self, cutoff: u64) -> Option<&HistoryPoint> {
        self.history.iter().rev().find(|p| p.at <= cutoff)
    }

    fn prune(&mut self, now: u64) {
        let retention = self
            .cfg
            .stall_window_secs
            .max(self.cfg.sync_window_secs)
            .max(self.cfg.compaction_window_secs)
            .max(self.cfg.delayed_write_sustain_secs)
            + 120;
        while let Some(front) = self.history.front() {
            if front.at + retention < now && self.history.len() > 2 {
                self.history.pop_front();
            } else {
                break;
            }
        }
        // Hard cap so a tiny poll interval cannot grow memory unboundedly.
        while self.history.len() > 100_000 {
            self.history.pop_front();
        }
    }

    /// Block-production stall: height unchanged across the window AND head
    /// age above threshold, while not syncing (a syncing node legitimately
    /// has an old head).
    fn eval_block_stall(&self, now: u64, s: &MetricsSnapshot) -> (bool, String) {
        if s.syncing == Some(true) {
            return (false, "node is syncing".into());
        }
        let (Some(height), Some(age)) = (s.block_height, s.time_since_last_block_secs) else {
            return (false, "height/head-age series missing".into());
        };
        let Some(base) = self.point_at_or_before(now.saturating_sub(self.cfg.stall_window_secs))
        else {
            return (false, "warming up (window not covered)".into());
        };
        match base.height {
            Some(base_height) if base_height == height && age > self.cfg.stall_threshold_secs => (
                true,
                format!(
                    "height {height} unchanged for >={}s, head age {age:.0}s (threshold {}s), syncing=0",
                    now.saturating_sub(base.at),
                    self.cfg.stall_threshold_secs
                ),
            ),
            _ => (
                false,
                format!("height {height}, head age {age:.0}s"),
            ),
        }
    }

    /// Stuck sync: syncing at both ends of the window and lag has not
    /// decreased (lag > 0).
    fn eval_stuck_sync(&self, now: u64, s: &MetricsSnapshot) -> (bool, String) {
        if s.syncing != Some(true) {
            return (false, "not syncing".into());
        }
        let Some(lag_now) = s.sync_lag_blocks else {
            return (false, "lag series missing".into());
        };
        let Some(base) = self.point_at_or_before(now.saturating_sub(self.cfg.sync_window_secs))
        else {
            return (false, "warming up (window not covered)".into());
        };
        match (base.syncing, base.lag) {
            (Some(true), Some(lag_then)) if lag_now >= lag_then && lag_now > 0 => (
                true,
                format!(
                    "syncing=1 and lag not decreasing over {}s: {lag_then} -> {lag_now} blocks",
                    now.saturating_sub(base.at)
                ),
            ),
            _ => (false, format!("syncing, lag {lag_now} blocks")),
        }
    }

    /// Compaction stall, three triggers in order of severity:
    /// 1. hard: `qfc_rocksdb_write_stopped == 1` (immediate);
    /// 2. soft: delayed-write rate > 0 continuously for the sustain window;
    /// 3. debt: pending-compaction bytes above the floor AND growing over
    ///    the compaction window.
    fn eval_compaction_stall(&self, now: u64, s: &MetricsSnapshot) -> (bool, String) {
        if s.write_stopped == Some(true) {
            return (
                true,
                "qfc_rocksdb_write_stopped=1 (hard write stall, block import blocked)".into(),
            );
        }

        if let Some(rate_now) = s.delayed_write_rate {
            if rate_now > 0.0 {
                let cutoff = now.saturating_sub(self.cfg.delayed_write_sustain_secs);
                if let Some(base) = self.point_at_or_before(cutoff) {
                    let sustained = base.delayed_rate.is_some_and(|r| r > 0.0)
                        && self
                            .history
                            .iter()
                            .filter(|p| p.at >= base.at)
                            .all(|p| p.delayed_rate.is_some_and(|r| r > 0.0));
                    if sustained {
                        return (
                            true,
                            format!(
                                "delayed write rate {rate_now:.0} B/s sustained for >={}s",
                                self.cfg.delayed_write_sustain_secs
                            ),
                        );
                    }
                }
            }
        }

        if let Some(debt) = s.pending_compaction_bytes {
            if debt > self.cfg.compaction_debt_bytes {
                if let Some(base) =
                    self.point_at_or_before(now.saturating_sub(self.cfg.compaction_window_secs))
                {
                    if let Some(debt_then) = base.pending_compaction {
                        if debt > debt_then {
                            return (
                                true,
                                format!(
                                    "pending compaction bytes growing past threshold: \
                                     {debt_then} -> {debt} (> {})",
                                    self.cfg.compaction_debt_bytes
                                ),
                            );
                        }
                    }
                }
            }
        }

        (false, "no write stall / compaction debt signal".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Healthy baseline snapshot; tests mutate the relevant fields.
    fn snap() -> MetricsSnapshot {
        MetricsSnapshot {
            block_height: Some(100),
            time_since_last_block_secs: Some(4.0),
            syncing: Some(false),
            sync_lag_blocks: Some(0),
            write_stopped: Some(false),
            delayed_write_rate: Some(0.0),
            pending_compaction_bytes: Some(0),
            snapshot_last_attempt_unix: None,
            snapshot_last_success_unix: None,
        }
    }

    fn engine() -> Engine {
        Engine::new(DetectorConfig::default())
    }

    const T0: u64 = 1_000_000;

    #[test]
    fn from_prometheus_reads_node_exporter_names() {
        let text = "\
qfc_block_height 42
qfc_time_since_last_block_seconds 7.500
qfc_sync_syncing 1
qfc_sync_lag_blocks 13
qfc_rocksdb_write_stopped 0
qfc_rocksdb_actual_delayed_write_rate 1048576
qfc_rocksdb_pending_compaction_bytes{cf=\"state\"} 1000
qfc_rocksdb_pending_compaction_bytes{cf=\"blocks\"} 500
qfc_snapshot_last_attempt_timestamp_seconds 1765000100
qfc_snapshot_last_success_timestamp_seconds 1765000000
";
        let s = MetricsSnapshot::from_prometheus(text);
        assert_eq!(s.block_height, Some(42));
        assert_eq!(s.time_since_last_block_secs, Some(7.5));
        assert_eq!(s.syncing, Some(true));
        assert_eq!(s.sync_lag_blocks, Some(13));
        assert_eq!(s.write_stopped, Some(false));
        assert_eq!(s.delayed_write_rate, Some(1_048_576.0));
        assert_eq!(s.pending_compaction_bytes, Some(1500)); // summed per-CF
        assert_eq!(s.snapshot_last_attempt_unix, Some(1_765_000_100));
        assert_eq!(s.snapshot_last_success_unix, Some(1_765_000_000));
    }

    #[test]
    fn from_prometheus_missing_series_are_none() {
        let s = MetricsSnapshot::from_prometheus("qfc_block_height 1\n");
        assert_eq!(s.block_height, Some(1));
        assert_eq!(s.syncing, None);
        assert_eq!(s.snapshot_last_attempt_unix, None);
        assert_eq!(s.pending_compaction_bytes, None);
    }

    #[test]
    fn healthy_node_scores_100_and_nothing_fires() {
        let mut e = engine();
        for i in 0..20 {
            let mut s = snap();
            s.block_height = Some(100 + i); // producing
            let eval = e.evaluate(T0 + i * 15, Some(&s), true);
            assert_eq!(eval.health_score, 100);
            assert!(eval.detectors.iter().all(|d| !d.firing));
        }
    }

    #[test]
    fn block_stall_fires_after_window_and_clears_on_new_block() {
        let mut e = engine();
        // Height frozen at 100, head age growing past the threshold.
        let mut fired_at = None;
        for i in 0..20u64 {
            let now = T0 + i * 15;
            let mut s = snap();
            s.time_since_last_block_secs = Some(4.0 + (i * 15) as f64);
            let eval = e.evaluate(now, Some(&s), true);
            if eval.firing(DetectorId::BlockProductionStall) && fired_at.is_none() {
                fired_at = Some(i);
                // fire transition is reported exactly once
                assert!(eval
                    .transitions
                    .iter()
                    .any(|t| t.id == DetectorId::BlockProductionStall && t.firing));
                assert_eq!(eval.health_score, 60);
            }
        }
        // Needs BOTH height-unchanged window (120s) and head age > 120s:
        // age passes 120 at i=8 (4+120=124), window covered well before.
        assert_eq!(fired_at, Some(8));

        // New block arrives: height advances, head age resets -> clears.
        let mut s = snap();
        s.block_height = Some(101);
        s.time_since_last_block_secs = Some(1.0);
        let eval = e.evaluate(T0 + 20 * 15, Some(&s), true);
        assert!(!eval.firing(DetectorId::BlockProductionStall));
        assert!(eval
            .transitions
            .iter()
            .any(|t| t.id == DetectorId::BlockProductionStall && !t.firing));
        assert_eq!(eval.health_score, 100);
    }

    #[test]
    fn block_stall_does_not_fire_while_syncing() {
        let mut e = engine();
        for i in 0..20u64 {
            let mut s = snap();
            s.syncing = Some(true);
            s.sync_lag_blocks = Some(500u64.saturating_sub(i * 50)); // catching up
            s.time_since_last_block_secs = Some(10_000.0); // old head is normal during sync
            let eval = e.evaluate(T0 + i * 15, Some(&s), true);
            assert!(
                !eval.firing(DetectorId::BlockProductionStall),
                "must not fire while syncing (poll {i})"
            );
        }
    }

    #[test]
    fn block_stall_requires_covered_window() {
        let mut e = engine();
        let mut s = snap();
        s.time_since_last_block_secs = Some(500.0); // way past threshold
                                                    // First poll: no baseline yet -> warming up, must not fire.
        let eval = e.evaluate(T0, Some(&s), true);
        assert!(!eval.firing(DetectorId::BlockProductionStall));
    }

    #[test]
    fn stuck_sync_fires_when_lag_flat_and_clears_when_decreasing() {
        let mut e = engine();
        // Lag pinned at 200 while syncing: fires once the 300s window is covered.
        let mut fired = false;
        for i in 0..30u64 {
            let mut s = snap();
            s.syncing = Some(true);
            s.sync_lag_blocks = Some(200);
            s.time_since_last_block_secs = Some(600.0);
            let eval = e.evaluate(T0 + i * 15, Some(&s), true);
            if i * 15 >= 300 {
                assert!(eval.firing(DetectorId::StuckSync), "poll {i}");
                fired = true;
            } else {
                assert!(!eval.firing(DetectorId::StuckSync), "poll {i}");
            }
        }
        assert!(fired);

        // Lag starts decreasing -> clears immediately.
        let mut s = snap();
        s.syncing = Some(true);
        s.sync_lag_blocks = Some(150);
        let eval = e.evaluate(T0 + 30 * 15, Some(&s), true);
        assert!(!eval.firing(DetectorId::StuckSync));
    }

    #[test]
    fn stuck_sync_never_fires_when_not_syncing() {
        let mut e = engine();
        for i in 0..30u64 {
            let mut s = snap();
            s.sync_lag_blocks = Some(200); // lag without syncing flag
            let eval = e.evaluate(T0 + i * 15, Some(&s), true);
            assert!(!eval.firing(DetectorId::StuckSync));
        }
    }

    #[test]
    fn compaction_stall_fires_immediately_on_write_stopped() {
        let mut e = engine();
        let mut s = snap();
        s.write_stopped = Some(true);
        // Immediate: no window needed for the hard-stop signal.
        let eval = e.evaluate(T0, Some(&s), true);
        assert!(eval.firing(DetectorId::CompactionStall));
        assert!(eval
            .status(DetectorId::CompactionStall)
            .evidence
            .contains("write_stopped=1"));
    }

    #[test]
    fn compaction_stall_fires_on_sustained_delayed_writes_only() {
        let mut e = engine();
        // Blip: delayed for one poll, then clean -> never fires.
        let mut s = snap();
        s.delayed_write_rate = Some(1000.0);
        assert!(!e
            .evaluate(T0, Some(&s), true)
            .firing(DetectorId::CompactionStall));
        for i in 1..10u64 {
            let eval = e.evaluate(T0 + i * 15, Some(&snap()), true);
            assert!(!eval.firing(DetectorId::CompactionStall));
        }

        // Sustained: delayed continuously for >= 120s -> fires.
        let base = T0 + 1000;
        let mut fired_at = None;
        for i in 0..12u64 {
            let mut s = snap();
            s.delayed_write_rate = Some(2_000_000.0);
            let eval = e.evaluate(base + i * 15, Some(&s), true);
            if eval.firing(DetectorId::CompactionStall) && fired_at.is_none() {
                fired_at = Some(i * 15);
            }
        }
        assert_eq!(
            fired_at,
            Some(120),
            "fires once the sustain window is covered"
        );
    }

    #[test]
    fn compaction_stall_fires_on_growing_debt_past_threshold() {
        let cfg = DetectorConfig {
            compaction_debt_bytes: 1_000_000,
            compaction_window_secs: 60,
            ..Default::default()
        };
        let mut e = Engine::new(cfg);
        // Debt high but flat -> no fire (steady-state big DB is fine).
        for i in 0..10u64 {
            let mut s = snap();
            s.pending_compaction_bytes = Some(2_000_000);
            let eval = e.evaluate(T0 + i * 15, Some(&s), true);
            assert!(
                !eval.firing(DetectorId::CompactionStall),
                "flat debt, poll {i}"
            );
        }
        // Debt growing while above threshold -> fires.
        let mut s = snap();
        s.pending_compaction_bytes = Some(3_000_000);
        let eval = e.evaluate(T0 + 10 * 15, Some(&s), true);
        assert!(eval.firing(DetectorId::CompactionStall));
        assert!(eval
            .status(DetectorId::CompactionStall)
            .evidence
            .contains("growing"));
    }

    #[test]
    fn metrics_down_after_n_consecutive_failures_and_clears() {
        let mut e = engine();
        e.evaluate(T0, Some(&snap()), true);
        let eval = e.evaluate(T0 + 15, None, true);
        assert!(!eval.firing(DetectorId::MetricsDown), "1 failure");
        let eval = e.evaluate(T0 + 30, None, true);
        assert!(!eval.firing(DetectorId::MetricsDown), "2 failures");
        let eval = e.evaluate(T0 + 45, None, true);
        assert!(
            eval.firing(DetectorId::MetricsDown),
            "3rd consecutive failure"
        );
        assert_eq!(eval.health_score, 70);
        // One success resets.
        let eval = e.evaluate(T0 + 60, Some(&snap()), true);
        assert!(!eval.firing(DetectorId::MetricsDown));
    }

    #[test]
    fn rpc_down_after_n_consecutive_failures() {
        let mut e = engine();
        let mut eval = e.evaluate(T0, Some(&snap()), false);
        for i in 1..3u64 {
            eval = e.evaluate(T0 + i * 15, Some(&snap()), false);
        }
        assert!(eval.firing(DetectorId::RpcDown));
        eval = e.evaluate(T0 + 45, Some(&snap()), true);
        assert!(!eval.firing(DetectorId::RpcDown));
    }

    #[test]
    fn node_state_detectors_freeze_during_scrape_failures() {
        let mut e = engine();
        // Drive block stall to firing with consecutive count.
        for i in 0..10u64 {
            let mut s = snap();
            s.time_since_last_block_secs = Some(4.0 + (i * 30) as f64);
            e.evaluate(T0 + i * 30, Some(&s), true);
        }
        let mut s = snap();
        s.time_since_last_block_secs = Some(400.0);
        let eval = e.evaluate(T0 + 300, Some(&s), true);
        assert!(eval.firing(DetectorId::BlockProductionStall));
        let consec = eval
            .status(DetectorId::BlockProductionStall)
            .consecutive_firing;
        assert!(consec >= 1);

        // Scrape failures: still firing, but the counter must not advance.
        let eval = e.evaluate(T0 + 315, None, true);
        assert!(eval.firing(DetectorId::BlockProductionStall));
        assert_eq!(
            eval.status(DetectorId::BlockProductionStall)
                .consecutive_firing,
            consec,
            "consecutive count frozen while blind"
        );
        assert!(eval.transitions.is_empty(), "no transitions while frozen");
    }

    #[test]
    fn health_score_clamps_at_zero_with_everything_firing() {
        let mut e = engine();
        // Stalled + write-stopped node, both probes dead after warmup.
        for i in 0..10u64 {
            let mut s = snap();
            s.time_since_last_block_secs = Some(4.0 + (i * 60) as f64);
            s.write_stopped = Some(true);
            e.evaluate(T0 + i * 60, Some(&s), true);
        }
        for i in 0..3u64 {
            e.evaluate(T0 + 600 + i * 15, None, false);
        }
        let eval = e.evaluate(T0 + 645, None, false);
        assert!(eval.firing(DetectorId::BlockProductionStall)); // frozen firing
        assert!(eval.firing(DetectorId::CompactionStall)); // frozen firing
        assert!(eval.firing(DetectorId::MetricsDown));
        assert!(eval.firing(DetectorId::RpcDown));
        assert_eq!(eval.health_score, 0); // 40+25+30+30 > 100, clamped
    }

    #[test]
    fn snapshot_and_syncing_context_carry_across_scrape_failures() {
        let mut e = engine();
        let mut s = snap();
        s.snapshot_last_attempt_unix = Some(T0 - 10);
        s.snapshot_last_success_unix = Some(T0 - 100);
        s.syncing = Some(true);
        e.evaluate(T0, Some(&s), true);
        let eval = e.evaluate(T0 + 15, None, true);
        assert_eq!(eval.snapshot_last_attempt_unix, Some(T0 - 10));
        assert_eq!(eval.snapshot_last_success_unix, Some(T0 - 100));
        assert_eq!(eval.syncing, Some(true));
    }

    #[test]
    fn detector_id_round_trips_from_str() {
        for id in DetectorId::ALL {
            assert_eq!(id.as_str().parse::<DetectorId>().unwrap(), id);
        }
        assert!("bogus".parse::<DetectorId>().is_err());
    }
}
