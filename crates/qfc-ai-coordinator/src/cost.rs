//! T5: FLOPs cost attribution per tenant and per miner + treasury hook.
//!
//! Every completed public task is metered with its `estimated_flops`
//! (from [`crate::task_types::task_requirements`] — the same per-task FLOPs
//! estimate used for fee pricing) and its fee, attributed to **both** the
//! tenant (submitter) that paid for it and the miner that executed it.
//!
//! A periodic [`CostReport`] (driven by qfc-node,
//! `--ai-cost-report-interval-secs`) aggregates the interval since the last
//! report plus cumulative totals, is handed to the configured
//! [`TreasuryHook`], and stays queryable in memory
//! (`TaskPool::last_cost_report`).
//!
//! The real treasury integration (charging tenants / crediting miners
//! on-chain) is explicitly out of scope here; [`TreasuryHook`] is the
//! contract it will implement. The default [`LoggingTreasuryHook`] only
//! emits structured logs.

use std::collections::HashMap;

use qfc_types::Address;
use serde::Serialize;

/// Metered totals for one party (tenant or miner).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CostEntry {
    /// Completed tasks.
    pub tasks: u64,
    /// Sum of `estimated_flops` over those tasks.
    pub flops: u128,
    /// Sum of task fees (max_fee, wei).
    pub fees_wei: u128,
}

impl CostEntry {
    fn add(&mut self, flops: u64, fee_wei: u128) {
        self.tasks += 1;
        self.flops = self.flops.saturating_add(flops as u128);
        self.fees_wei = self.fees_wei.saturating_add(fee_wei);
    }
}

/// One line of a cost report.
#[derive(Clone, Debug, Serialize)]
pub struct CostReportLine {
    /// `0x`-hex address of the tenant or miner.
    pub address: String,
    /// Interval (since previous report) totals.
    pub interval: CostEntry,
    /// Cumulative totals since process start.
    pub cumulative: CostEntry,
}

/// Periodic per-tenant / per-miner cost attribution report.
#[derive(Clone, Debug, Serialize)]
pub struct CostReport {
    /// When this report was generated (unix ms).
    pub generated_at_ms: u64,
    /// Start of the covered interval (previous report time or meter start).
    pub period_start_ms: u64,
    /// Per-tenant lines, sorted by interval FLOPs descending.
    pub tenants: Vec<CostReportLine>,
    /// Per-miner lines, sorted by interval FLOPs descending.
    pub miners: Vec<CostReportLine>,
    /// Interval totals across all tenants.
    pub interval_total: CostEntry,
    /// Cumulative totals since process start.
    pub cumulative_total: CostEntry,
}

/// Treasury integration contract (T5 deliverable — the hook, not the
/// integration). Implementations must be cheap and non-blocking: both
/// callbacks run while the `TaskPool` lock is held.
///
/// - [`TreasuryHook::on_task_charged`] fires once per completed public task
///   with the metered cost (per-task charge callback).
/// - [`TreasuryHook::on_cost_report`] fires once per periodic report
///   (batch settlement callback).
///
/// The default implementations are no-ops so an integrator can pick either
/// granularity.
pub trait TreasuryHook: Send + Sync {
    /// A public task completed: `tenant` owes for `flops`/`fee_wei`,
    /// `miner` earned it.
    fn on_task_charged(&self, _tenant: &Address, _miner: &Address, _flops: u64, _fee_wei: u128) {}

    /// A periodic cost report was generated.
    fn on_cost_report(&self, _report: &CostReport) {}
}

/// Default hook: structured logging only (target `qfc::ai_cost`), no
/// treasury side effects.
pub struct LoggingTreasuryHook;

impl TreasuryHook for LoggingTreasuryHook {
    fn on_task_charged(&self, tenant: &Address, miner: &Address, flops: u64, fee_wei: u128) {
        tracing::debug!(
            target: "qfc::ai_cost",
            tenant = %tenant,
            miner = %miner,
            flops,
            fee_wei,
            "AI task cost metered"
        );
    }

    fn on_cost_report(&self, report: &CostReport) {
        let json =
            serde_json::to_string(report).unwrap_or_else(|e| format!("<serialize error: {e}>"));
        tracing::info!(
            target: "qfc::ai_cost",
            tenants = report.tenants.len(),
            miners = report.miners.len(),
            interval_tasks = report.interval_total.tasks,
            interval_flops = %report.interval_total.flops,
            report = %json,
            "AI cost report"
        );
    }
}

/// Accumulates per-tenant and per-miner cost attribution.
#[derive(Default)]
pub struct CostMeter {
    interval_tenants: HashMap<Address, CostEntry>,
    interval_miners: HashMap<Address, CostEntry>,
    cumulative_tenants: HashMap<Address, CostEntry>,
    cumulative_miners: HashMap<Address, CostEntry>,
    interval_total: CostEntry,
    cumulative_total: CostEntry,
    period_start_ms: u64,
    last_report: Option<CostReport>,
}

impl CostMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Meter one completed task.
    pub fn record(&mut self, tenant: &Address, miner: &Address, flops: u64, fee_wei: u128) {
        self.interval_tenants
            .entry(*tenant)
            .or_default()
            .add(flops, fee_wei);
        self.interval_miners
            .entry(*miner)
            .or_default()
            .add(flops, fee_wei);
        self.cumulative_tenants
            .entry(*tenant)
            .or_default()
            .add(flops, fee_wei);
        self.cumulative_miners
            .entry(*miner)
            .or_default()
            .add(flops, fee_wei);
        self.interval_total.add(flops, fee_wei);
        self.cumulative_total.add(flops, fee_wei);
    }

    /// Cumulative FLOPs metered (all tenants).
    pub fn cumulative_flops(&self) -> u128 {
        self.cumulative_total.flops
    }

    /// Cumulative totals for one tenant.
    pub fn tenant_total(&self, tenant: &Address) -> CostEntry {
        self.cumulative_tenants
            .get(tenant)
            .copied()
            .unwrap_or_default()
    }

    /// Cumulative totals for one miner.
    pub fn miner_total(&self, miner: &Address) -> CostEntry {
        self.cumulative_miners
            .get(miner)
            .copied()
            .unwrap_or_default()
    }

    /// Generate a report covering the interval since the previous one and
    /// reset the interval accumulators. The report is retained and
    /// retrievable via [`CostMeter::last_report`].
    pub fn generate_report(&mut self, now_ms: u64) -> CostReport {
        fn lines(
            interval: &HashMap<Address, CostEntry>,
            cumulative: &HashMap<Address, CostEntry>,
        ) -> Vec<CostReportLine> {
            // Include every party seen this interval, plus cumulative-only
            // parties are omitted (they appear in the report where they were
            // active; cumulative state is still queryable via accessors).
            let mut out: Vec<CostReportLine> = interval
                .iter()
                .map(|(addr, entry)| CostReportLine {
                    address: format!("{addr}"),
                    interval: *entry,
                    cumulative: cumulative.get(addr).copied().unwrap_or_default(),
                })
                .collect();
            out.sort_by_key(|line| std::cmp::Reverse(line.interval.flops));
            out
        }

        let report = CostReport {
            generated_at_ms: now_ms,
            period_start_ms: self.period_start_ms,
            tenants: lines(&self.interval_tenants, &self.cumulative_tenants),
            miners: lines(&self.interval_miners, &self.cumulative_miners),
            interval_total: self.interval_total,
            cumulative_total: self.cumulative_total,
        };

        self.interval_tenants.clear();
        self.interval_miners.clear();
        self.interval_total = CostEntry::default();
        self.period_start_ms = now_ms;
        self.last_report = Some(report.clone());
        report
    }

    /// The most recent report, if any.
    pub fn last_report(&self) -> Option<&CostReport> {
        self.last_report.as_ref()
    }

    /// Unix ms of the last report (0 = never). Exported as
    /// `qfc_ai_cost_report_last_timestamp_seconds` (0 = never, matching the
    /// T4.2 snapshot-freshness convention).
    pub fn last_report_unix_ms(&self) -> u64 {
        self.last_report
            .as_ref()
            .map(|r| r.generated_at_ms)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    #[test]
    fn metering_accumulates_per_tenant_and_miner() {
        let mut meter = CostMeter::new();
        let (t1, t2, m1, m2) = (addr(1), addr(2), addr(10), addr(20));

        meter.record(&t1, &m1, 1_000, 100);
        meter.record(&t1, &m2, 2_000, 200);
        meter.record(&t2, &m1, 5_000, 500);

        assert_eq!(
            meter.tenant_total(&t1),
            CostEntry {
                tasks: 2,
                flops: 3_000,
                fees_wei: 300
            }
        );
        assert_eq!(
            meter.tenant_total(&t2),
            CostEntry {
                tasks: 1,
                flops: 5_000,
                fees_wei: 500
            }
        );
        assert_eq!(
            meter.miner_total(&m1),
            CostEntry {
                tasks: 2,
                flops: 6_000,
                fees_wei: 600
            }
        );
        assert_eq!(
            meter.miner_total(&m2),
            CostEntry {
                tasks: 1,
                flops: 2_000,
                fees_wei: 200
            }
        );
        assert_eq!(meter.cumulative_flops(), 8_000);
    }

    #[test]
    fn report_covers_interval_and_resets() {
        let mut meter = CostMeter::new();
        let (t1, m1) = (addr(1), addr(10));

        meter.record(&t1, &m1, 1_000, 100);
        let r1 = meter.generate_report(5_000);
        assert_eq!(r1.period_start_ms, 0);
        assert_eq!(r1.generated_at_ms, 5_000);
        assert_eq!(r1.tenants.len(), 1);
        assert_eq!(r1.miners.len(), 1);
        assert_eq!(r1.interval_total.flops, 1_000);
        assert_eq!(r1.cumulative_total.flops, 1_000);

        // Second interval: only new activity appears in interval lines,
        // cumulative keeps growing.
        meter.record(&t1, &m1, 2_000, 200);
        let r2 = meter.generate_report(10_000);
        assert_eq!(r2.period_start_ms, 5_000);
        assert_eq!(r2.interval_total.flops, 2_000);
        assert_eq!(r2.cumulative_total.flops, 3_000);
        assert_eq!(r2.tenants[0].interval.flops, 2_000);
        assert_eq!(r2.tenants[0].cumulative.flops, 3_000);

        // Empty interval → empty lines but cumulative preserved.
        let r3 = meter.generate_report(15_000);
        assert!(r3.tenants.is_empty());
        assert_eq!(r3.cumulative_total.flops, 3_000);

        assert_eq!(meter.last_report_unix_ms(), 15_000);
        assert!(meter.last_report().is_some());
    }

    #[test]
    fn report_sorted_by_interval_flops_desc() {
        let mut meter = CostMeter::new();
        meter.record(&addr(1), &addr(10), 100, 0);
        meter.record(&addr(2), &addr(10), 900, 0);
        let r = meter.generate_report(1);
        assert_eq!(r.tenants[0].address, format!("{}", addr(2)));
        assert_eq!(r.tenants[1].address, format!("{}", addr(1)));
    }

    #[test]
    fn report_serializes_to_json() {
        let mut meter = CostMeter::new();
        meter.record(&addr(1), &addr(10), 1_000, u128::MAX / 2);
        let r = meter.generate_report(1);
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"tenants\""));
        assert!(json.contains("\"miners\""));
    }
}
