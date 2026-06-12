//! The watchdog's own Prometheus endpoint (`--watchdog-metrics-addr`).
//!
//! Hand-rendered text format on a tiny_http background thread, exactly like
//! qfc-node's exporter — so the watchdog watching a node is itself watchable
//! (and the qfc-watchdog alert group in docs/observability/alert-rules.yaml
//! has something to scrape).

use crate::detector::{DetectorId, Evaluation};
use parking_lot::Mutex;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{error, info};

#[derive(Default)]
pub struct WatchdogMetrics {
    /// 0–100 aggregate health of the watched node (100 until the first poll).
    pub health_score: AtomicU64,
    pub polls_total: AtomicU64,
    pub scrape_failures_total: AtomicU64,
    pub rpc_probe_failures_total: AtomicU64,
    pub actions_total: AtomicU64,
    pub action_failures_total: AtomicU64,
    /// 1 while an action is wanted but the hourly budget is spent.
    pub action_budget_exhausted: AtomicU64,
    /// Unix time of the last executed action (0 = none since start).
    pub action_last_unix: AtomicU64,
    /// 1 when --action-cmd is configured (set once at startup).
    pub action_mode_enabled: AtomicU64,
    detector_firing: Mutex<Vec<(&'static str, bool)>>,
}

impl WatchdogMetrics {
    pub fn new(action_mode_enabled: bool) -> Self {
        Self {
            health_score: AtomicU64::new(100),
            action_mode_enabled: AtomicU64::new(u64::from(action_mode_enabled)),
            detector_firing: Mutex::new(
                DetectorId::ALL
                    .iter()
                    .map(|d| (d.as_str(), false))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    /// Push the result of one poll into the exported gauges.
    pub fn record_evaluation(&self, eval: &Evaluation) {
        self.health_score
            .store(u64::from(eval.health_score), Ordering::Relaxed);
        let mut firing = self.detector_firing.lock();
        firing.clear();
        firing.extend(eval.detectors.iter().map(|d| (d.id.as_str(), d.firing)));
    }

    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);

        let score = self.health_score.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "# HELP qfc_watchdog_health_score Aggregate health of the watched node, 0-100 (100 = no detector firing)."
        );
        let _ = writeln!(out, "# TYPE qfc_watchdog_health_score gauge");
        let _ = writeln!(out, "qfc_watchdog_health_score {score}");

        let _ = writeln!(
            out,
            "# HELP qfc_watchdog_detector_firing Whether each watchdog detector is currently firing (0/1)."
        );
        let _ = writeln!(out, "# TYPE qfc_watchdog_detector_firing gauge");
        for (name, firing) in self.detector_firing.lock().iter() {
            let _ = writeln!(
                out,
                "qfc_watchdog_detector_firing{{detector=\"{name}\"}} {}",
                u8::from(*firing)
            );
        }

        let counters: &[(&str, &AtomicU64, &str)] = &[
            (
                "qfc_watchdog_polls_total",
                &self.polls_total,
                "Poll cycles executed.",
            ),
            (
                "qfc_watchdog_scrape_failures_total",
                &self.scrape_failures_total,
                "Failed scrapes of the watched node's /metrics endpoint.",
            ),
            (
                "qfc_watchdog_rpc_probe_failures_total",
                &self.rpc_probe_failures_total,
                "Failed eth_blockNumber probes against the watched node's RPC port.",
            ),
            (
                "qfc_watchdog_actions_total",
                &self.actions_total,
                "Remediation actions executed (action mode only; every one is also logged with evidence).",
            ),
            (
                "qfc_watchdog_action_failures_total",
                &self.action_failures_total,
                "Remediation actions whose command exited non-zero or failed to spawn.",
            ),
        ];
        for (name, v, help) in counters {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {}", v.load(Ordering::Relaxed));
        }

        let gauges: &[(&str, &AtomicU64, &str)] = &[
            (
                "qfc_watchdog_action_budget_exhausted",
                &self.action_budget_exhausted,
                "1 while an action is wanted but --max-actions-per-hour is spent (human needed).",
            ),
            (
                "qfc_watchdog_action_last_timestamp_seconds",
                &self.action_last_unix,
                "Unix time of the last executed action (0 = none since watchdog start).",
            ),
            (
                "qfc_watchdog_action_mode_enabled",
                &self.action_mode_enabled,
                "1 when --action-cmd is configured, 0 in detection-only mode.",
            ),
        ];
        for (name, v, help) in gauges {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} gauge");
            let _ = writeln!(out, "{name} {}", v.load(Ordering::Relaxed));
        }

        let version = env!("CARGO_PKG_VERSION");
        let _ = writeln!(
            out,
            "# HELP qfc_watchdog_info Watchdog metadata as labels. Value is always 1."
        );
        let _ = writeln!(out, "# TYPE qfc_watchdog_info gauge");
        let _ = writeln!(out, "qfc_watchdog_info{{version=\"{version}\"}} 1");

        out
    }
}

/// Spawn the watchdog's own metrics HTTP server on a background thread
/// (same pattern as qfc-node's MetricsServer::start).
pub fn serve(addr: SocketAddr, metrics: Arc<WatchdogMetrics>) {
    std::thread::spawn(move || {
        let server = match tiny_http::Server::http(addr) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to start watchdog metrics server on {}: {}", addr, e);
                return;
            }
        };
        info!(
            "Watchdog metrics server listening on http://{}/metrics",
            addr
        );

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
            let body = metrics.render();
            let resp = tiny_http::Response::from_string(&body).with_header(
                "Content-Type: text/plain; version=0.0.4; charset=utf-8"
                    .parse::<tiny_http::Header>()
                    .unwrap(),
            );
            let _ = request.respond(resp);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::{DetectorConfig, Engine};

    /// The exporter output must contain every metric name referenced by the
    /// qfc-watchdog group in docs/observability/alert-rules.yaml.
    #[test]
    fn render_includes_expected_metric_names() {
        let m = WatchdogMetrics::new(true);
        let out = m.render();
        for name in [
            "qfc_watchdog_health_score",
            "qfc_watchdog_detector_firing",
            "qfc_watchdog_polls_total",
            "qfc_watchdog_scrape_failures_total",
            "qfc_watchdog_rpc_probe_failures_total",
            "qfc_watchdog_actions_total",
            "qfc_watchdog_action_failures_total",
            "qfc_watchdog_action_budget_exhausted",
            "qfc_watchdog_action_last_timestamp_seconds",
            "qfc_watchdog_action_mode_enabled",
            "qfc_watchdog_info",
        ] {
            assert!(out.contains(name), "missing metric `{name}`:\n{out}");
        }
        // One firing gauge per detector, all initially 0.
        for id in DetectorId::ALL {
            assert!(out.contains(&format!(
                "qfc_watchdog_detector_firing{{detector=\"{id}\"}} 0"
            )));
        }
        assert!(out.contains("qfc_watchdog_health_score 100"));
        assert!(out.contains("qfc_watchdog_action_mode_enabled 1"));
    }

    #[test]
    fn record_evaluation_updates_score_and_detector_gauges() {
        let m = WatchdogMetrics::new(false);
        let mut engine = Engine::new(DetectorConfig::default());
        // Three consecutive total failures: metrics_down + rpc_down fire.
        engine.evaluate(1_000_000, None, false);
        engine.evaluate(1_000_015, None, false);
        let eval = engine.evaluate(1_000_030, None, false);
        m.record_evaluation(&eval);

        let out = m.render();
        assert!(out.contains("qfc_watchdog_health_score 40")); // 100 - 30 - 30
        assert!(out.contains("qfc_watchdog_detector_firing{detector=\"metrics_down\"} 1"));
        assert!(out.contains("qfc_watchdog_detector_firing{detector=\"rpc_down\"} 1"));
        assert!(out.contains("qfc_watchdog_detector_firing{detector=\"block_production_stall\"} 0"));
        assert!(out.contains("qfc_watchdog_action_mode_enabled 0"));
    }
}
