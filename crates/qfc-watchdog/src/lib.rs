//! qfc-watchdog — out-of-process, detection-first watchdog for one qfc-node
//! (SRE roadmap item T6, docs/ROADMAP-SRE.md).
//!
//! Why out-of-process: a stalled or wedged node cannot restart itself — the
//! watcher must live outside the process (and ideally outside the container)
//! it watches. The watchdog observes a node purely through its public
//! surfaces: the Prometheus metrics endpoint (`--metrics-addr`, default
//! `:6060`) and the JSON-RPC port (`eth_blockNumber` probe). It never links
//! against node internals, so a node that is alive enough to serve metrics
//! is observed exactly as Prometheus would see it.
//!
//! Detection-first philosophy (per the roadmap's non-goals): the watchdog
//! ships with action mode **off**. It detects, scores, exports its own
//! metrics, and logs. Automated restarts only happen when explicitly enabled
//! with `--action-cmd`, and every execution is gated by all of the safety
//! gates in [`gate`]. Automation earns trust incrementally — see
//! docs/WATCHDOG.md for the trust-escalation path.
//!
//! Module map:
//! - [`prom`] — minimal Prometheus text-format parsing (no metrics library).
//! - [`probe`] — plain-std HTTP probes for `/metrics` and the RPC port.
//! - [`detector`] — the detector state machine + health score. Pure logic,
//!   injectable clock (`now_secs`), unit-testable without sleeping.
//! - [`gate`] — action gating (eligible set, consecutive polls, hourly
//!   budget, cooldown, snapshot-in-flight, sync-progress, startup grace).
//! - [`exporter`] — the watchdog's own `/metrics` endpoint, hand-rendered
//!   text exactly like qfc-node's exporter.

pub mod detector;
pub mod exporter;
pub mod gate;
pub mod probe;
pub mod prom;
