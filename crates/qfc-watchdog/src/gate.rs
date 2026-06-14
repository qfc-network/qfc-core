//! Action gating — the part of T6 that keeps "self-healing" honest.
//!
//! Action mode is OFF unless `--action-cmd` is set, and even then every
//! execution must pass ALL gates, checked in this order:
//!
//! 1. **trigger** — an action-eligible detector (default: block-production
//!    stall only) has been firing for `--action-after-consecutive` evaluated
//!    polls (default 3). No trigger → `Idle`.
//! 2. **startup grace** — never within `--startup-grace-secs` of watchdog
//!    start (default 120): give a freshly (re)started node/watchdog pair
//!    time to settle.
//! 3. **snapshot in-flight** — never while a snapshot backup appears to be
//!    running (`last_attempt > last_success` and the attempt is recent):
//!    restarting mid-checkpoint risks a corrupt archive upload.
//! 4. **sync progressing** — never while the node is syncing and making
//!    progress (syncing flag set and the stuck-sync detector NOT firing):
//!    a catching-up node looks unhealthy but is healing itself.
//! 5. **cooldown** — at least `--action-cooldown-secs` (default 600) after
//!    any action: give the restarted node time to come back before judging.
//! 6. **hourly budget** — at most `--max-actions-per-hour` (default 2) over
//!    a sliding 1h window. Exhausted budget → `BudgetExhausted`: log loudly,
//!    keep alerting, take no action. A flapping node needs a human.
//!
//! The clock is injected (`now_secs`) — gate logic is unit-tested with
//! synthetic time, never sleeps.

use crate::detector::{DetectorId, Evaluation};
use std::collections::VecDeque;

const BUDGET_WINDOW_SECS: u64 = 3600;

/// Gate thresholds. All overridable from the CLI; defaults documented in
/// docs/WATCHDOG.md.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// Detectors allowed to trigger an action. Default: block-production
    /// stall only — broaden it as automation earns trust.
    pub eligible: Vec<DetectorId>,
    /// Trigger only after this many consecutive firing polls. Default 3.
    pub after_consecutive: u32,
    /// Sliding-window action budget. Default 2 per hour.
    pub max_actions_per_hour: u32,
    /// Quiet period after any action. Default 600 s.
    pub cooldown_secs: u64,
    /// Quiet period after watchdog start. Default 120 s.
    pub startup_grace_secs: u64,
    /// How recent a not-yet-succeeded snapshot attempt must be to count as
    /// "in flight". Default 1800 s.
    pub snapshot_inflight_grace_secs: u64,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            eligible: vec![DetectorId::BlockProductionStall],
            after_consecutive: 3,
            max_actions_per_hour: 2,
            cooldown_secs: 600,
            startup_grace_secs: 120,
            snapshot_inflight_grace_secs: 1800,
        }
    }
}

/// Outcome of one gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No eligible detector is past its consecutive-poll threshold.
    Idle,
    /// A trigger exists but a safety gate vetoed the action.
    Hold { gate: &'static str, reason: String },
    /// A trigger exists, every safety gate passed, but the hourly budget is
    /// spent. Caller must log loudly and keep alerting only.
    BudgetExhausted { evidence: String },
    /// All gates passed — caller may execute the action, then must call
    /// [`Gate::record_action`].
    Act {
        detector: DetectorId,
        evidence: String,
    },
}

/// Sliding-window action accounting + gate evaluation.
pub struct Gate {
    cfg: GateConfig,
    started_at_secs: u64,
    /// Times of executed actions within the budget window.
    recent_actions: VecDeque<u64>,
    last_action_at: Option<u64>,
}

impl Gate {
    pub fn new(cfg: GateConfig, now_secs: u64) -> Self {
        Self {
            cfg,
            started_at_secs: now_secs,
            recent_actions: VecDeque::new(),
            last_action_at: None,
        }
    }

    pub fn decide(&mut self, now_secs: u64, eval: &Evaluation) -> Decision {
        // Gate 1: trigger — eligible detector firing long enough.
        let trigger = self.cfg.eligible.iter().find_map(|id| {
            let st = eval.status(*id);
            (st.firing && st.consecutive_firing >= self.cfg.after_consecutive)
                .then(|| (st.id, st.evidence.clone()))
        });
        let Some((detector, evidence)) = trigger else {
            return Decision::Idle;
        };
        let evidence = format!("{detector}: {evidence} | {}", eval.evidence_summary());

        // Gate 2: startup grace.
        let since_start = now_secs.saturating_sub(self.started_at_secs);
        if since_start < self.cfg.startup_grace_secs {
            return Decision::Hold {
                gate: "startup-grace",
                reason: format!(
                    "watchdog started {since_start}s ago (< {}s grace)",
                    self.cfg.startup_grace_secs
                ),
            };
        }

        // Gate 3: snapshot backup in flight.
        if let Some(attempt) = eval.snapshot_last_attempt_unix {
            let success = eval.snapshot_last_success_unix.unwrap_or(0);
            let attempt_age = now_secs.saturating_sub(attempt);
            if attempt > success && attempt_age < self.cfg.snapshot_inflight_grace_secs {
                return Decision::Hold {
                    gate: "snapshot-in-flight",
                    reason: format!(
                        "snapshot attempt {attempt_age}s ago has not succeeded yet \
                         (last_attempt {attempt} > last_success {success})"
                    ),
                };
            }
        }

        // Gate 4: sync progressing (syncing and NOT detectably stuck).
        if eval.syncing == Some(true) && !eval.firing(DetectorId::StuckSync) {
            return Decision::Hold {
                gate: "sync-progressing",
                reason: "node is syncing and not stuck — let it heal itself".to_string(),
            };
        }

        // Gate 5: cooldown after the previous action.
        if let Some(last) = self.last_action_at {
            let since = now_secs.saturating_sub(last);
            if since < self.cfg.cooldown_secs {
                return Decision::Hold {
                    gate: "cooldown",
                    reason: format!(
                        "last action {since}s ago (< {}s cooldown)",
                        self.cfg.cooldown_secs
                    ),
                };
            }
        }

        // Gate 6: sliding-window hourly budget.
        self.prune(now_secs);
        if self.recent_actions.len() as u32 >= self.cfg.max_actions_per_hour {
            return Decision::BudgetExhausted { evidence };
        }

        Decision::Act { detector, evidence }
    }

    /// Record an executed action (call after running the command, success or
    /// not — a failed restart attempt still spent a budget slot).
    pub fn record_action(&mut self, now_secs: u64) {
        self.recent_actions.push_back(now_secs);
        self.last_action_at = Some(now_secs);
    }

    pub fn actions_in_window(&mut self, now_secs: u64) -> u32 {
        self.prune(now_secs);
        self.recent_actions.len() as u32
    }

    fn prune(&mut self, now_secs: u64) {
        while let Some(&front) = self.recent_actions.front() {
            if now_secs.saturating_sub(front) >= BUDGET_WINDOW_SECS {
                self.recent_actions.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::{DetectorStatus, Evaluation};

    const T0: u64 = 1_000_000;

    /// Synthetic evaluation: `firing` detectors fire with the given
    /// consecutive count; everything else is clear.
    fn eval_with(firing: &[(DetectorId, u32)]) -> Evaluation {
        let detectors = DetectorId::ALL
            .iter()
            .map(|id| {
                let hit = firing.iter().find(|(f, _)| f == id);
                DetectorStatus {
                    id: *id,
                    firing: hit.is_some(),
                    consecutive_firing: hit.map(|(_, c)| *c).unwrap_or(0),
                    evidence: format!("synthetic evidence for {id}"),
                }
            })
            .collect();
        Evaluation {
            at_secs: T0,
            detectors,
            health_score: 100,
            transitions: vec![],
            syncing: Some(false),
            snapshot_last_attempt_unix: None,
            snapshot_last_success_unix: None,
        }
    }

    /// Gate whose startup grace is already over at T0.
    fn warm_gate(cfg: GateConfig) -> Gate {
        let grace = cfg.startup_grace_secs;
        Gate::new(cfg, T0.saturating_sub(grace))
    }

    fn stall(consecutive: u32) -> Evaluation {
        eval_with(&[(DetectorId::BlockProductionStall, consecutive)])
    }

    #[test]
    fn idle_when_nothing_fires() {
        let mut g = warm_gate(GateConfig::default());
        assert_eq!(g.decide(T0, &eval_with(&[])), Decision::Idle);
    }

    #[test]
    fn idle_until_consecutive_threshold() {
        let mut g = warm_gate(GateConfig::default());
        assert_eq!(g.decide(T0, &stall(1)), Decision::Idle);
        assert_eq!(g.decide(T0, &stall(2)), Decision::Idle);
        assert!(matches!(g.decide(T0, &stall(3)), Decision::Act { .. }));
    }

    #[test]
    fn idle_when_firing_detector_is_not_eligible() {
        // Default eligible set is block-production stall ONLY: even a hard
        // rocksdb stop or rpc-down must not trigger an action.
        let mut g = warm_gate(GateConfig::default());
        let eval = eval_with(&[
            (DetectorId::CompactionStall, 100),
            (DetectorId::RpcDown, 100),
            (DetectorId::MetricsDown, 100),
            (DetectorId::StuckSync, 100),
        ]);
        assert_eq!(g.decide(T0, &eval), Decision::Idle);
    }

    #[test]
    fn configurable_eligible_set() {
        let cfg = GateConfig {
            eligible: vec![
                DetectorId::BlockProductionStall,
                DetectorId::CompactionStall,
            ],
            ..Default::default()
        };
        let mut g = warm_gate(cfg);
        let d = g.decide(T0, &eval_with(&[(DetectorId::CompactionStall, 5)]));
        match d {
            Decision::Act { detector, .. } => assert_eq!(detector, DetectorId::CompactionStall),
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn startup_grace_holds() {
        let mut g = Gate::new(GateConfig::default(), T0); // started at T0
        match g.decide(T0 + 60, &stall(10)) {
            Decision::Hold { gate, .. } => assert_eq!(gate, "startup-grace"),
            other => panic!("expected Hold, got {other:?}"),
        }
        // Grace over -> acts.
        assert!(matches!(
            g.decide(T0 + 120, &stall(10)),
            Decision::Act { .. }
        ));
    }

    #[test]
    fn snapshot_in_flight_holds() {
        let mut g = warm_gate(GateConfig::default());
        let mut eval = stall(10);
        // Attempt newer than success and recent -> in flight.
        eval.snapshot_last_attempt_unix = Some(T0 - 60);
        eval.snapshot_last_success_unix = Some(T0 - 4000);
        match g.decide(T0, &eval) {
            Decision::Hold { gate, .. } => assert_eq!(gate, "snapshot-in-flight"),
            other => panic!("expected Hold, got {other:?}"),
        }

        // Attempt succeeded (success >= attempt) -> not in flight.
        eval.snapshot_last_success_unix = Some(T0 - 60);
        assert!(matches!(g.decide(T0, &eval), Decision::Act { .. }));

        // Stale failed attempt (older than the grace) -> not in flight:
        // a snapshot that has been "running" for 30+ min is itself wedged
        // and must not veto recovery forever.
        eval.snapshot_last_attempt_unix = Some(T0 - 2000);
        eval.snapshot_last_success_unix = Some(T0 - 9000);
        assert!(matches!(g.decide(T0, &eval), Decision::Act { .. }));
    }

    #[test]
    fn no_snapshot_metrics_means_gate_passes() {
        // Nodes without --snapshot-interval-secs export no qfc_snapshot_*.
        let mut g = warm_gate(GateConfig::default());
        assert!(matches!(g.decide(T0, &stall(3)), Decision::Act { .. }));
    }

    #[test]
    fn sync_progressing_holds_but_stuck_sync_does_not() {
        let cfg = GateConfig {
            eligible: vec![DetectorId::BlockProductionStall, DetectorId::StuckSync],
            ..Default::default()
        };
        let mut g = warm_gate(cfg);

        // Syncing and not stuck: hold even with an eligible trigger.
        let mut eval = stall(10);
        eval.syncing = Some(true);
        match g.decide(T0, &eval) {
            Decision::Hold { gate, .. } => assert_eq!(gate, "sync-progressing"),
            other => panic!("expected Hold, got {other:?}"),
        }

        // Syncing but provably stuck: the gate does not veto.
        let mut eval = eval_with(&[(DetectorId::StuckSync, 10)]);
        eval.syncing = Some(true);
        assert!(matches!(g.decide(T0, &eval), Decision::Act { .. }));
    }

    #[test]
    fn cooldown_holds_after_action() {
        let mut g = warm_gate(GateConfig::default());
        assert!(matches!(g.decide(T0, &stall(5)), Decision::Act { .. }));
        g.record_action(T0);

        match g.decide(T0 + 599, &stall(5)) {
            Decision::Hold { gate, .. } => assert_eq!(gate, "cooldown"),
            other => panic!("expected Hold, got {other:?}"),
        }
        // Cooldown elapsed (and budget has one slot left) -> acts again.
        assert!(matches!(
            g.decide(T0 + 600, &stall(5)),
            Decision::Act { .. }
        ));
    }

    #[test]
    fn budget_exhausts_and_recovers_with_sliding_window() {
        let mut g = warm_gate(GateConfig::default());

        // Two actions spend the default budget (cooldown spaced).
        assert!(matches!(g.decide(T0, &stall(5)), Decision::Act { .. }));
        g.record_action(T0);
        assert!(matches!(
            g.decide(T0 + 600, &stall(5)),
            Decision::Act { .. }
        ));
        g.record_action(T0 + 600);
        assert_eq!(g.actions_in_window(T0 + 600), 2);

        // Past cooldown, still inside the hour -> budget exhausted.
        match g.decide(T0 + 1300, &stall(5)) {
            Decision::BudgetExhausted { evidence } => {
                assert!(evidence.contains("block_production_stall"))
            }
            other => panic!("expected BudgetExhausted, got {other:?}"),
        }

        // First action slides out of the window at T0+3600 -> one slot back.
        assert!(matches!(
            g.decide(T0 + 3600, &stall(5)),
            Decision::Act { .. }
        ));
        assert_eq!(g.actions_in_window(T0 + 3600), 1);
    }

    #[test]
    fn act_carries_detector_and_evidence() {
        let mut g = warm_gate(GateConfig::default());
        match g.decide(T0, &stall(3)) {
            Decision::Act { detector, evidence } => {
                assert_eq!(detector, DetectorId::BlockProductionStall);
                assert!(evidence.contains("synthetic evidence"));
                assert!(evidence.contains("health="));
            }
            other => panic!("expected Act, got {other:?}"),
        }
    }

    #[test]
    fn gate_order_trigger_before_everything() {
        // With no trigger, even inside startup grace the decision is Idle
        // (gates are only consulted when something wants to act).
        let mut g = Gate::new(GateConfig::default(), T0);
        assert_eq!(g.decide(T0 + 1, &eval_with(&[])), Decision::Idle);
    }
}
