//! T5: Per-tenant quotas on the AI task pool.
//!
//! A **tenant** is a submitter address. Each tenant maps to a [`TierQuota`]
//! (either an explicit per-tenant entry or the configured default tier) that
//! bounds:
//!
//! - **QPS** — submissions/second via a token bucket (`max_qps` + `burst`);
//! - **in-flight tasks** — public tasks in `Pending`/`Assigned` state;
//! - **FLOPs budget** — `estimated_flops` admitted per fixed window
//!   (`window_secs`), a fixed-window approximation of a rolling budget.
//!
//! Tenants also carry a **priority** (0..=2, 2 = highest). Under pool
//! pressure, lower-priority tenants are shed first: priority 0 is rejected
//! once the pending queue reaches 50% of `max_pending`, priority 1 at 75%,
//! and priority 2 only at the hard cap. See `docs/AI-QUOTAS.md`.
//!
//! Configuration is JSON (`--ai-quotas <path>` on qfc-node). JSON rather than
//! TOML because `serde_json` is already in this crate's dependency tree and
//! the workspace has no `toml` dependency. When no config is supplied,
//! quotas are **off**: admission always succeeds (the fee escrow on the RPC
//! submission path remains the economic backpressure), but in-flight counts
//! are still tracked so the exporter gauges stay meaningful.

use std::collections::HashMap;
use std::path::Path;

use qfc_types::Address;
use serde::{Deserialize, Serialize};

/// Number of priority tiers (0 = lowest, shed first; 2 = highest).
pub const PRIORITY_TIERS: usize = 3;

/// Rejection reason labels, in documented degradation/checking order.
/// These are the exact `reason` label values on
/// `qfc_ai_tasks_rejected_total`.
pub const REJECT_REASONS: [&str; 4] = ["pool_pressure", "qps", "in_flight", "flops_budget"];

fn default_max_qps() -> f64 {
    10.0
}

fn default_max_inflight() -> u64 {
    100
}

fn default_priority() -> u8 {
    1
}

fn default_window_secs() -> u64 {
    3600
}

fn default_max_pending() -> usize {
    10_000
}

/// Quota parameters for one tier (the default tier or a per-tenant override).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TierQuota {
    /// Sustained submission rate in requests/second (token-bucket refill).
    /// `0` disables QPS limiting for this tier.
    #[serde(default = "default_max_qps")]
    pub max_qps: f64,
    /// Token-bucket capacity (burst size). Defaults to `ceil(max_qps)`,
    /// minimum 1.
    #[serde(default)]
    pub burst: Option<u32>,
    /// Maximum tasks in flight (`Pending` or `Assigned`). `0` = unlimited.
    #[serde(default = "default_max_inflight")]
    pub max_inflight: u64,
    /// FLOPs budget (`estimated_flops` sum) admitted per `window_secs`
    /// window. `0` = unlimited.
    #[serde(default)]
    pub flops_per_window: u64,
    /// Priority tier 0..=2; values above 2 are clamped. Lowest is shed
    /// first under pool pressure.
    #[serde(default = "default_priority")]
    pub priority: u8,
}

impl Default for TierQuota {
    /// Generous defaults so an operator can enable quota *accounting*
    /// (and pool-pressure shedding) without immediately throttling
    /// well-behaved tenants.
    fn default() -> Self {
        Self {
            max_qps: default_max_qps(),
            burst: None,
            max_inflight: default_max_inflight(),
            flops_per_window: 0,
            priority: default_priority(),
        }
    }
}

impl TierQuota {
    /// Effective bucket capacity.
    fn capacity(&self) -> f64 {
        match self.burst {
            Some(b) => (b as f64).max(1.0),
            None => self.max_qps.ceil().max(1.0),
        }
    }

    /// Priority clamped to the valid range.
    pub fn clamped_priority(&self) -> u8 {
        self.priority.min((PRIORITY_TIERS - 1) as u8)
    }
}

/// Quota configuration: a default tier plus per-tenant overrides.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaConfig {
    /// FLOPs-budget window length in seconds.
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// Hard cap on the pending queue. Pool-pressure shedding thresholds are
    /// derived from this: priority 0 sheds at 50%, priority 1 at 75%,
    /// priority 2 at 100%.
    #[serde(default = "default_max_pending")]
    pub max_pending: usize,
    /// Quota applied to tenants without an explicit entry.
    #[serde(default)]
    pub default_tier: TierQuota,
    /// Per-tenant overrides, keyed by `0x`-prefixed hex address.
    #[serde(default)]
    pub tenants: HashMap<String, TierQuota>,
    /// Parsed tenant table (built by [`QuotaConfig::finalize`]).
    #[serde(skip)]
    tenants_parsed: HashMap<Address, TierQuota>,
}

impl Default for QuotaConfig {
    /// Matches the serde field defaults (an empty JSON object `{}` parses
    /// to exactly this).
    fn default() -> Self {
        Self {
            window_secs: default_window_secs(),
            max_pending: default_max_pending(),
            default_tier: TierQuota::default(),
            tenants: HashMap::new(),
            tenants_parsed: HashMap::new(),
        }
    }
}

/// Errors loading/parsing a quota config file.
#[derive(Debug, thiserror::Error)]
pub enum QuotaConfigError {
    #[error("failed to read quota config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse quota config JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid tenant address in quota config: {0}")]
    InvalidAddress(String),
}

impl QuotaConfig {
    /// Parse a JSON config string and build the tenant lookup table.
    pub fn from_json_str(s: &str) -> Result<Self, QuotaConfigError> {
        let mut cfg: QuotaConfig = serde_json::from_str(s)?;
        cfg.finalize()?;
        Ok(cfg)
    }

    /// Load a JSON config file (`--ai-quotas <path>`).
    pub fn from_file(path: &Path) -> Result<Self, QuotaConfigError> {
        let raw = std::fs::read_to_string(path)?;
        Self::from_json_str(&raw)
    }

    /// Build the parsed tenant table; validates addresses.
    pub fn finalize(&mut self) -> Result<(), QuotaConfigError> {
        self.tenants_parsed.clear();
        for (key, quota) in &self.tenants {
            let hex_part = key.strip_prefix("0x").unwrap_or(key);
            let bytes =
                hex::decode(hex_part).map_err(|_| QuotaConfigError::InvalidAddress(key.clone()))?;
            let addr = Address::from_slice(&bytes)
                .ok_or_else(|| QuotaConfigError::InvalidAddress(key.clone()))?;
            self.tenants_parsed.insert(addr, quota.clone());
        }
        Ok(())
    }

    /// Quota for a tenant: explicit entry or the default tier.
    pub fn tier_for(&self, tenant: &Address) -> &TierQuota {
        self.tenants_parsed
            .get(tenant)
            .unwrap_or(&self.default_tier)
    }

    /// Pending-queue threshold above which `priority` is shed.
    /// Degradation order: priority 0 at 50%, 1 at 75%, 2 at 100%.
    pub fn shed_threshold(&self, priority: u8) -> usize {
        match priority {
            0 => self.max_pending / 2,
            1 => self.max_pending * 3 / 4,
            _ => self.max_pending,
        }
    }
}

/// Typed admission rejection. Surfaced over JSON-RPC as error code `-32029`
/// with the limit name and a retry-after hint.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum QuotaError {
    #[error("pool pressure: pending queue at {pending} >= shed threshold {threshold} for priority {priority}; retry after {retry_after_ms}ms")]
    PoolPressure {
        tenant: Address,
        priority: u8,
        pending: usize,
        threshold: usize,
        retry_after_ms: u64,
    },
    #[error("QPS limit exceeded for tenant {tenant}: limit {limit} req/s; retry after {retry_after_ms}ms")]
    QpsExceeded {
        tenant: Address,
        limit: f64,
        retry_after_ms: u64,
    },
    #[error("in-flight task limit exceeded for tenant {tenant}: {in_flight}/{limit} tasks outstanding; retry after {retry_after_ms}ms")]
    InFlightExceeded {
        tenant: Address,
        limit: u64,
        in_flight: u64,
        retry_after_ms: u64,
    },
    #[error("FLOPs budget exceeded for tenant {tenant}: {used}+{requested} > {budget} per {window_secs}s window; retry after {retry_after_ms}ms")]
    FlopsBudgetExceeded {
        tenant: Address,
        budget: u64,
        used: u64,
        requested: u64,
        window_secs: u64,
        retry_after_ms: u64,
    },
}

impl QuotaError {
    /// Stable reason label (matches [`REJECT_REASONS`] /
    /// `qfc_ai_tasks_rejected_total{reason=...}`).
    pub fn reason(&self) -> &'static str {
        match self {
            QuotaError::PoolPressure { .. } => "pool_pressure",
            QuotaError::QpsExceeded { .. } => "qps",
            QuotaError::InFlightExceeded { .. } => "in_flight",
            QuotaError::FlopsBudgetExceeded { .. } => "flops_budget",
        }
    }

    /// Hint for the client: milliseconds until a retry has a chance.
    pub fn retry_after_ms(&self) -> u64 {
        match self {
            QuotaError::PoolPressure { retry_after_ms, .. }
            | QuotaError::QpsExceeded { retry_after_ms, .. }
            | QuotaError::InFlightExceeded { retry_after_ms, .. }
            | QuotaError::FlopsBudgetExceeded { retry_after_ms, .. } => *retry_after_ms,
        }
    }
}

/// Per-tenant runtime state (token bucket + budget window + in-flight count).
#[derive(Clone, Debug)]
struct TenantState {
    /// Token bucket level. Initialized to capacity on first submission.
    tokens: f64,
    /// Last refill timestamp (ms).
    last_refill_ms: u64,
    /// Public tasks in Pending/Assigned state.
    in_flight: u64,
    /// Start of the current FLOPs-budget window (ms).
    window_start_ms: u64,
    /// estimated_flops admitted in the current window.
    flops_used: u64,
}

impl TenantState {
    fn new(now_ms: u64, capacity: f64) -> Self {
        Self {
            tokens: capacity,
            last_refill_ms: now_ms,
            in_flight: 0,
            window_start_ms: now_ms,
            flops_used: 0,
        }
    }
}

/// Admission control + per-tenant runtime accounting.
///
/// Checks run in documented order: pool pressure → QPS → in-flight →
/// FLOPs budget. State (token, budget) is only committed when every check
/// passes, so a rejected request consumes nothing.
pub struct QuotaEnforcer {
    config: Option<QuotaConfig>,
    tenants: HashMap<Address, TenantState>,
}

impl QuotaEnforcer {
    pub fn new() -> Self {
        Self {
            config: None,
            tenants: HashMap::new(),
        }
    }

    /// Install (or clear) the quota configuration. Runtime state for known
    /// tenants is kept; bucket capacities adjust on the next refill.
    pub fn set_config(&mut self, config: Option<QuotaConfig>) {
        self.config = config;
    }

    pub fn enabled(&self) -> bool {
        self.config.is_some()
    }

    pub fn config(&self) -> Option<&QuotaConfig> {
        self.config.as_ref()
    }

    /// Priority tier for a tenant (default tier when quotas are off).
    pub fn priority_for(&self, tenant: &Address) -> u8 {
        match &self.config {
            Some(cfg) => cfg.tier_for(tenant).clamped_priority(),
            None => default_priority(),
        }
    }

    /// Admission check for a submission of `estimated_flops` while the
    /// pending queue holds `pending` tasks. Commits one QPS token and the
    /// FLOPs charge only when admitted. Does NOT bump in-flight — call
    /// [`QuotaEnforcer::task_started`] when the task is actually enqueued.
    pub fn admit(
        &mut self,
        tenant: &Address,
        estimated_flops: u64,
        pending: usize,
        now_ms: u64,
    ) -> Result<(), QuotaError> {
        let Some(cfg) = &self.config else {
            return Ok(()); // quotas off
        };
        let quota = cfg.tier_for(tenant).clone();
        let priority = quota.clamped_priority();

        // 1. Pool pressure: shed lowest-priority tenants first.
        let threshold = cfg.shed_threshold(priority);
        if pending >= threshold {
            return Err(QuotaError::PoolPressure {
                tenant: *tenant,
                priority,
                pending,
                threshold,
                retry_after_ms: 1_000,
            });
        }

        let window_ms = cfg.window_secs.saturating_mul(1000);
        let capacity = quota.capacity();
        let state = self
            .tenants
            .entry(*tenant)
            .or_insert_with(|| TenantState::new(now_ms, capacity));

        // Refill the token bucket.
        if quota.max_qps > 0.0 {
            let elapsed_ms = now_ms.saturating_sub(state.last_refill_ms);
            state.tokens =
                (state.tokens + elapsed_ms as f64 / 1000.0 * quota.max_qps).min(capacity);
            state.last_refill_ms = now_ms;
        }

        // Roll the FLOPs window forward if it has elapsed.
        if window_ms > 0 && now_ms.saturating_sub(state.window_start_ms) >= window_ms {
            state.window_start_ms = now_ms;
            state.flops_used = 0;
        }

        // 2. QPS (peek; consume only on full success).
        if quota.max_qps > 0.0 && state.tokens < 1.0 {
            let deficit = 1.0 - state.tokens;
            let retry_after_ms = (deficit / quota.max_qps * 1000.0).ceil() as u64;
            return Err(QuotaError::QpsExceeded {
                tenant: *tenant,
                limit: quota.max_qps,
                retry_after_ms,
            });
        }

        // 3. In-flight.
        if quota.max_inflight > 0 && state.in_flight >= quota.max_inflight {
            return Err(QuotaError::InFlightExceeded {
                tenant: *tenant,
                limit: quota.max_inflight,
                in_flight: state.in_flight,
                retry_after_ms: 1_000,
            });
        }

        // 4. FLOPs budget.
        if quota.flops_per_window > 0
            && state.flops_used.saturating_add(estimated_flops) > quota.flops_per_window
        {
            let window_end = state.window_start_ms + window_ms;
            return Err(QuotaError::FlopsBudgetExceeded {
                tenant: *tenant,
                budget: quota.flops_per_window,
                used: state.flops_used,
                requested: estimated_flops,
                window_secs: cfg.window_secs,
                retry_after_ms: window_end.saturating_sub(now_ms).max(1),
            });
        }

        // Commit.
        if quota.max_qps > 0.0 {
            state.tokens -= 1.0;
        }
        state.flops_used = state.flops_used.saturating_add(estimated_flops);
        Ok(())
    }

    /// A public task for `tenant` entered the pool (Pending).
    pub fn task_started(&mut self, tenant: &Address, now_ms: u64) {
        let capacity = match &self.config {
            Some(cfg) => cfg.tier_for(tenant).capacity(),
            None => TierQuota::default().capacity(),
        };
        let state = self
            .tenants
            .entry(*tenant)
            .or_insert_with(|| TenantState::new(now_ms, capacity));
        state.in_flight = state.in_flight.saturating_add(1);
    }

    /// A public task for `tenant` left Pending/Assigned (completed or
    /// expired).
    pub fn task_finished(&mut self, tenant: &Address) {
        if let Some(state) = self.tenants.get_mut(tenant) {
            state.in_flight = state.in_flight.saturating_sub(1);
        }
    }

    /// In-flight tasks summed per priority tier (bounded-cardinality view
    /// for the exporter: per-tenant series would be unbounded).
    pub fn inflight_by_priority(&self) -> [u64; PRIORITY_TIERS] {
        let mut out = [0u64; PRIORITY_TIERS];
        for (tenant, state) in &self.tenants {
            let p = self.priority_for(tenant) as usize;
            out[p] = out[p].saturating_add(state.in_flight);
        }
        out
    }

    /// In-flight count for one tenant (test/RPC accessor).
    pub fn inflight_for(&self, tenant: &Address) -> u64 {
        self.tenants.get(tenant).map(|s| s.in_flight).unwrap_or(0)
    }
}

impl Default for QuotaEnforcer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::new([b; 20])
    }

    fn cfg_with_default(tier: TierQuota) -> QuotaConfig {
        let mut cfg = QuotaConfig {
            default_tier: tier,
            ..Default::default()
        };
        cfg.finalize().unwrap();
        cfg
    }

    #[test]
    fn disabled_enforcer_admits_everything() {
        let mut e = QuotaEnforcer::new();
        for i in 0..1000 {
            assert!(e.admit(&addr(1), u64::MAX, 1_000_000, i).is_ok());
        }
    }

    #[test]
    fn token_bucket_qps_edges() {
        let mut e = QuotaEnforcer::new();
        e.set_config(Some(cfg_with_default(TierQuota {
            max_qps: 2.0,
            burst: Some(2),
            max_inflight: 0,
            flops_per_window: 0,
            priority: 1,
        })));
        let t = addr(1);

        // Burst of 2 admitted instantly.
        assert!(e.admit(&t, 0, 0, 0).is_ok());
        assert!(e.admit(&t, 0, 0, 0).is_ok());
        // Third is rejected with a retry-after hint of ~500ms (1 token / 2 qps).
        let err = e.admit(&t, 0, 0, 0).unwrap_err();
        match &err {
            QuotaError::QpsExceeded { retry_after_ms, .. } => {
                assert_eq!(*retry_after_ms, 500);
            }
            other => panic!("expected QpsExceeded, got {other:?}"),
        }
        assert_eq!(err.reason(), "qps");

        // 499ms later: still not enough (0.998 tokens).
        assert!(e.admit(&t, 0, 0, 499).is_err());
        // 500ms later: exactly one token refilled.
        assert!(e.admit(&t, 0, 0, 500).is_ok());
        // Bucket never exceeds burst capacity: after a long idle, only 2.
        assert!(e.admit(&t, 0, 0, 100_000).is_ok());
        assert!(e.admit(&t, 0, 0, 100_000).is_ok());
        assert!(e.admit(&t, 0, 0, 100_000).is_err());
    }

    #[test]
    fn rejected_request_consumes_nothing() {
        // A request that fails the FLOPs check must not burn a QPS token.
        let mut e = QuotaEnforcer::new();
        e.set_config(Some(cfg_with_default(TierQuota {
            max_qps: 1.0,
            burst: Some(1),
            max_inflight: 0,
            flops_per_window: 100,
            priority: 1,
        })));
        let t = addr(1);
        assert!(matches!(
            e.admit(&t, 200, 0, 0),
            Err(QuotaError::FlopsBudgetExceeded { .. })
        ));
        // Token still available for an in-budget request at the same instant.
        assert!(e.admit(&t, 50, 0, 0).is_ok());
    }

    #[test]
    fn in_flight_limit() {
        let mut e = QuotaEnforcer::new();
        e.set_config(Some(cfg_with_default(TierQuota {
            max_qps: 0.0, // unlimited QPS
            burst: None,
            max_inflight: 2,
            flops_per_window: 0,
            priority: 1,
        })));
        let t = addr(1);

        assert!(e.admit(&t, 0, 0, 0).is_ok());
        e.task_started(&t, 0);
        assert!(e.admit(&t, 0, 0, 1).is_ok());
        e.task_started(&t, 1);
        let err = e.admit(&t, 0, 0, 2).unwrap_err();
        assert_eq!(err.reason(), "in_flight");

        // Completion frees a slot.
        e.task_finished(&t);
        assert!(e.admit(&t, 0, 0, 3).is_ok());
        assert_eq!(e.inflight_for(&t), 1);
    }

    #[test]
    fn flops_budget_window_expiry() {
        let mut e = QuotaEnforcer::new();
        let mut cfg = cfg_with_default(TierQuota {
            max_qps: 0.0,
            burst: None,
            max_inflight: 0,
            flops_per_window: 1_000,
            priority: 1,
        });
        cfg.window_secs = 10; // 10s window
        e.set_config(Some(cfg));
        let t = addr(1);

        assert!(e.admit(&t, 600, 0, 0).is_ok());
        assert!(e.admit(&t, 400, 0, 1).is_ok()); // exactly at budget
        let err = e.admit(&t, 1, 0, 2).unwrap_err();
        match &err {
            QuotaError::FlopsBudgetExceeded {
                used,
                budget,
                retry_after_ms,
                ..
            } => {
                assert_eq!(*used, 1_000);
                assert_eq!(*budget, 1_000);
                // Window started at t=0ms, 10s long → retry at 10_000-2.
                assert_eq!(*retry_after_ms, 9_998);
            }
            other => panic!("expected FlopsBudgetExceeded, got {other:?}"),
        }

        // One ms before expiry: still rejected.
        assert!(e.admit(&t, 1, 0, 9_999).is_err());
        // At window boundary: budget resets.
        assert!(e.admit(&t, 1_000, 0, 10_000).is_ok());
    }

    #[test]
    fn pool_pressure_sheds_lowest_priority_first() {
        let mut cfg = QuotaConfig {
            max_pending: 100,
            ..Default::default()
        };
        cfg.tenants.insert(
            format!("0x{}", hex::encode(addr(0).as_bytes())),
            TierQuota {
                priority: 0,
                max_qps: 0.0,
                ..Default::default()
            },
        );
        cfg.tenants.insert(
            format!("0x{}", hex::encode(addr(1).as_bytes())),
            TierQuota {
                priority: 1,
                max_qps: 0.0,
                ..Default::default()
            },
        );
        cfg.tenants.insert(
            format!("0x{}", hex::encode(addr(2).as_bytes())),
            TierQuota {
                priority: 2,
                max_qps: 0.0,
                ..Default::default()
            },
        );
        cfg.finalize().unwrap();
        let mut e = QuotaEnforcer::new();
        e.set_config(Some(cfg));

        // Below 50%: everyone admitted.
        for b in 0..3 {
            assert!(e.admit(&addr(b), 0, 49, 0).is_ok());
        }
        // At 50% (pending=50): priority 0 shed, 1 and 2 admitted.
        assert_eq!(
            e.admit(&addr(0), 0, 50, 0).unwrap_err().reason(),
            "pool_pressure"
        );
        assert!(e.admit(&addr(1), 0, 50, 0).is_ok());
        assert!(e.admit(&addr(2), 0, 50, 0).is_ok());
        // At 75% (pending=75): priority 1 also shed.
        assert!(e.admit(&addr(1), 0, 75, 0).is_err());
        assert!(e.admit(&addr(2), 0, 75, 0).is_ok());
        // At the hard cap: everyone shed.
        assert!(e.admit(&addr(2), 0, 100, 0).is_err());
    }

    #[test]
    fn config_parsing_defaults_and_overrides() {
        let json = r#"{
            "window_secs": 600,
            "max_pending": 500,
            "default_tier": { "max_qps": 5.0, "max_inflight": 10 },
            "tenants": {
                "0x0101010101010101010101010101010101010101": {
                    "max_qps": 50.0, "burst": 100, "priority": 2,
                    "flops_per_window": 5000000000000
                }
            }
        }"#;
        let cfg = QuotaConfig::from_json_str(json).unwrap();
        assert_eq!(cfg.window_secs, 600);
        assert_eq!(cfg.max_pending, 500);

        let vip = cfg.tier_for(&addr(1));
        assert_eq!(vip.max_qps, 50.0);
        assert_eq!(vip.clamped_priority(), 2);
        assert_eq!(vip.flops_per_window, 5_000_000_000_000);

        let other = cfg.tier_for(&addr(9));
        assert_eq!(other.max_qps, 5.0);
        assert_eq!(other.max_inflight, 10);
        // Unspecified fields fall back to serde defaults.
        assert_eq!(other.flops_per_window, 0);
        assert_eq!(other.clamped_priority(), 1);
    }

    #[test]
    fn config_rejects_bad_address() {
        let json = r#"{ "tenants": { "0xnothex": {} } }"#;
        assert!(matches!(
            QuotaConfig::from_json_str(json),
            Err(QuotaConfigError::InvalidAddress(_))
        ));
        let json = r#"{ "tenants": { "0x0102": {} } }"#; // wrong length
        assert!(matches!(
            QuotaConfig::from_json_str(json),
            Err(QuotaConfigError::InvalidAddress(_))
        ));
    }

    #[test]
    fn priority_clamped() {
        let q = TierQuota {
            priority: 9,
            ..Default::default()
        };
        assert_eq!(q.clamped_priority(), 2);
    }

    #[test]
    fn inflight_by_priority_aggregates() {
        let mut cfg = QuotaConfig::default();
        cfg.tenants.insert(
            format!("0x{}", hex::encode(addr(2).as_bytes())),
            TierQuota {
                priority: 2,
                ..Default::default()
            },
        );
        cfg.finalize().unwrap();
        let mut e = QuotaEnforcer::new();
        e.set_config(Some(cfg));

        e.task_started(&addr(1), 0); // default tier → priority 1
        e.task_started(&addr(1), 0);
        e.task_started(&addr(2), 0); // priority 2
        assert_eq!(e.inflight_by_priority(), [0, 2, 1]);
    }
}
