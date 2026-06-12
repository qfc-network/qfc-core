# AI Task Pool: Multi-Tenant Quotas & Cost Attribution (T5)

Per-submitter quotas on the AI task pool (QPS, in-flight, FLOPs budget) with
fair scheduling, FLOPs cost attribution per tenant and per miner, and a
treasury hook. Implements roadmap item T5 of
[ROADMAP-SRE.md](ROADMAP-SRE.md).

Code: `crates/qfc-ai-coordinator/src/quota.rs` (admission),
`crates/qfc-ai-coordinator/src/cost.rs` (metering + treasury hook),
`crates/qfc-ai-coordinator/src/task_pool.rs` (integration),
`crates/qfc-rpc/src/error.rs` (RPC error surface),
`crates/qfc-node/src/{main,metrics}.rs` (config + exporter).

## Tenant model

A **tenant is a submitter address** — the `submitter` of a public inference
task (`qfc_submitPublicTask`). Each tenant resolves to a quota: an explicit
per-tenant entry in the config, or the configured **default tier**.

Per-tenant limits:

| Limit | Mechanism | Config field | Disabled by |
|---|---|---|---|
| QPS | token bucket: refill `max_qps` tokens/s, capacity `burst` (default `ceil(max_qps)`) | `max_qps`, `burst` | `max_qps: 0` |
| In-flight tasks | count of the tenant's public tasks in `Pending`/`Assigned` | `max_inflight` | `max_inflight: 0` |
| FLOPs budget | sum of admitted `estimated_flops` per fixed window of `window_secs` (fixed-window approximation of a rolling budget; resets at window boundary) | `flops_per_window` | `flops_per_window: 0` |
| Priority | tier 0–2; **2 = highest, 0 = shed first** | `priority` | — |

`estimated_flops` comes from
`task_types::task_requirements(task_type).estimated_flops` — the same
per-task FLOPs estimate the fee pricing uses, so budget and billing agree.

## Configuration

`qfc-node --ai-quotas <path>` (env `QFC_AI_QUOTAS`). The file is **JSON**
(not TOML: `serde_json` is already in the dependency tree; the workspace has
no TOML dependency). Example:

```json
{
  "window_secs": 3600,
  "max_pending": 10000,
  "default_tier": {
    "max_qps": 10.0,
    "max_inflight": 100,
    "flops_per_window": 0,
    "priority": 1
  },
  "tenants": {
    "0x0101010101010101010101010101010101010101": {
      "max_qps": 50.0,
      "burst": 100,
      "max_inflight": 500,
      "flops_per_window": 5000000000000000,
      "priority": 2
    },
    "0x0202020202020202020202020202020202020202": {
      "max_qps": 1.0,
      "max_inflight": 5,
      "priority": 0
    }
  }
}
```

Every field has a default (the `default_tier` shown above *is* the default:
10 QPS, 100 in-flight, unlimited FLOPs, priority 1) — an empty `{}` file
enables quota accounting with generous limits.

- **Absent flag → quotas off.** Admission always succeeds; the fee escrow on
  the submission path remains the economic backpressure. Chosen over a
  default-on tier so existing deployments are unchanged by this upgrade.
  Accounting (in-flight gauges, FLOPs metering, cost reports) runs either
  way.
- **Hot reload:** the node re-checks the file's mtime every ~30 s and
  reloads on change. A file that fails to parse is logged and **ignored**
  (previous config stays active). No restart needed; a restart also works.
- Startup with an invalid file is a hard error (fail fast).

## Admission & enforcement

`TaskPool::try_submit_public_task` checks, in order:

1. **Pool pressure** (degradation order, below)
2. **QPS** token bucket
3. **In-flight** limit
4. **FLOPs budget**

A rejected submission consumes nothing (no token, no budget). Rejections are
typed (`QuotaError`) — never a panic — and counted in
`qfc_ai_tasks_rejected_total{reason=...}` with
`reason ∈ pool_pressure | qps | in_flight | flops_budget`.

### RPC error surface

`qfc_submitPublicTask` surfaces rejections as JSON-RPC error **-32029** with
the violated limit and a retry-after hint in both the message and structured
`data` (the escrowed fee is refunded first):

```json
{
  "code": -32029,
  "message": "Quota exceeded (qps): QPS limit exceeded for tenant 0x01…: limit 10 req/s; retry after 73ms",
  "data": { "reason": "qps", "retryAfterMs": 73 }
}
```

### Degradation order (pool pressure)

Under pool pressure the **lowest-priority tenants are shed first**, at
thresholds derived from `max_pending` (the hard cap on the pending queue):

| Pending queue ≥ | Shed (rejected at admission) |
|---|---|
| 50% of `max_pending` | priority 0 |
| 75% of `max_pending` | priority 0, 1 |
| 100% of `max_pending` | everyone (hard cap) |

### Fair scheduling

When a miner fetches work (`TaskPool::fetch_task_for`), selection is:

1. **Highest priority tier present** (tier 2 before 1 before 0);
2. **round-robin across tenants** within that tier — tenants ordered by
   address, served cyclically from a single cursor, so no tenant is served
   twice before every other tenant with matching pending work is served once
   (starvation-free, regardless of fees);
3. **highest fee within a tenant** (the pre-T5 fee ordering, preserved).

Synthetic (filler) tasks have no submitter and run as the zero-address
tenant at the default tier's priority.

## Cost attribution & treasury hook

Every **completed** public task is metered — `estimated_flops` + fee,
attributed to both the tenant (who pays) and the miner (who earned) —
expired tasks are not metered (never executed; their escrow is refunded).

A periodic **cost report** (`--ai-cost-report-interval-secs`, default 600,
`0` = off) aggregates interval + cumulative totals per tenant and per miner,
sorted by interval FLOPs:

- emitted as a structured log, target **`qfc::ai_cost`** (JSON payload in
  the `report` field);
- retained in memory, queryable via `TaskPool::last_cost_report()` /
  `cost_meter()`;
- handed to the **treasury hook**.

### TreasuryHook contract

```rust
pub trait TreasuryHook: Send + Sync {
    /// Once per completed task (per-task charge granularity).
    fn on_task_charged(&self, tenant: &Address, miner: &Address,
                       flops: u64, fee_wei: u128) {}
    /// Once per periodic cost report (batch settlement granularity).
    fn on_cost_report(&self, report: &CostReport) {}
}
```

Install with `TaskPool::set_treasury_hook`. Both callbacks run **while the
TaskPool lock is held** — implementations must be cheap and non-blocking
(enqueue, don't settle inline). The default `LoggingTreasuryHook` only logs.
The real on-chain treasury integration (charging tenants / crediting miners
against `qfc-ai-coordinator::Treasury`) is explicitly out of scope for T5 —
this trait is the integration point it will implement.

## Metrics & operations

Exported by the qfc-node exporter (`:6060/metrics`, see
[observability/README.md](observability/README.md)). Labels use the
**priority tier** (`tenant_tier` ∈ 0/1/2), never the tenant address —
tenant cardinality is unbounded, tiers are fixed at three. Per-tenant detail
is in the cost report.

| Metric | Type | Labels |
|---|---|---|
| `qfc_ai_quotas_enabled` | gauge (0/1) | — |
| `qfc_ai_pending_tasks` | gauge | — |
| `qfc_ai_tasks_submitted_total` | counter | `tenant_tier` |
| `qfc_ai_tasks_rejected_total` | counter | `reason` |
| `qfc_ai_flops_metered_total` | counter | `tenant_tier` |
| `qfc_ai_tenant_inflight` | gauge | `tenant_tier` |
| `qfc_ai_cost_report_last_timestamp_seconds` | gauge | — (0 = never; alert on age) |

**Spotting a throttled tenant:**

1. `rate(qfc_ai_tasks_rejected_total[5m]) > 0` → someone is being limited;
   the `reason` label says which limit.
2. Node logs name the tenant: every rejection logs
   `Public task rejected by quota for 0x…: <limit details>` at info level;
   clients simultaneously see `-32029` with `retryAfterMs`.
3. `reason="pool_pressure"` rising while `qfc_ai_pending_tasks` approaches
   `max_pending` → the pool is saturated and shedding by priority
   (degradation order above), not a per-tenant misconfiguration.
4. Per-tenant consumption: grep the `qfc::ai_cost` cost-report logs for the
   tenant address (interval + cumulative FLOPs/fees per tenant and miner).
5. `time() - qfc_ai_cost_report_last_timestamp_seconds` much larger than the
   configured interval → the reporting task is stuck/disabled.
