# qfc-watchdog — out-of-process, detection-first self-healing (T6)

Implements [ROADMAP-SRE.md](ROADMAP-SRE.md) item **T6**: detect
block-production stall, stuck sync, and compaction stall from the T2
signals; **detection-first** (alert + health score); automated restart only
behind explicit safety gates. Crate: `crates/qfc-watchdog`.

## Design

### Why out-of-process

A stalled node cannot restart itself: if block import is wedged on a RocksDB
write stop, or the process is deadlocked, any in-process "self-healing" task
is wedged with it. The watchdog is therefore a **separate binary** (ideally a
separate container) that observes a node purely through its public surfaces:

- the Prometheus exporter (`qfc-node --metrics-addr`, default `:6060`) —
  the same T2 metrics Prometheus scrapes, so watchdog and alerting can never
  disagree about what they saw;
- the JSON-RPC port (`--rpc-addr`, default `:8545`), probed with
  `eth_blockNumber` — proves the node answers real client traffic, not just
  its metrics thread.

It never links node internals. **One watchdog watches one node**; run N
watchdog sidecars for N nodes (state like sliding windows and the action
budget is per-node by construction).

### Detection-first philosophy

Per the roadmap's non-goals: *no auto-remediation without gates — T6 stays
detection-first until trust is earned*. Concretely:

- The default mode is **pure detection**: health score + per-detector gauges
  on the watchdog's own metrics endpoint, structured log lines on every
  detector fire/clear, and the `qfc-watchdog` alert group in
  [observability/alert-rules.yaml](observability/alert-rules.yaml).
- Action mode must be explicitly enabled (`--action-cmd`), starts with the
  narrowest possible trigger (block-production stall only), and every
  execution must pass **all** safety gates below.
- When the action budget is spent, automation stands down loudly
  (`qfc_watchdog_action_budget_exhausted 1` + error log) and keeps alerting.
  It never escalates its own privileges; a flapping node gets a human.

## Detector catalog

Evaluated once per poll (`--poll-interval-secs`, default 15 s) over sliding
windows of scrape history. Each detector is a firing bool + an evidence
string (logged on transitions, attached to any action).

| Detector | Fires when | Tunables (default) | Health weight |
|---|---|---|---|
| `block_production_stall` | height unchanged over the window AND `qfc_time_since_last_block_seconds` > threshold AND not syncing | `--stall-threshold-secs` (120), `--stall-window-secs` (120) | 40 |
| `stuck_sync` | `qfc_sync_syncing` = 1 at both window ends AND `qfc_sync_lag_blocks` has not decreased (lag > 0) | `--sync-window-secs` (300) | 15 |
| `compaction_stall` | `qfc_rocksdb_write_stopped` = 1 (immediate); OR `qfc_rocksdb_actual_delayed_write_rate` > 0 continuously for the sustain window; OR Σ `qfc_rocksdb_pending_compaction_bytes` above the floor AND growing over the window | `--delayed-write-sustain-secs` (120), `--compaction-debt-bytes` (8 GiB), `--compaction-window-secs` (600) | 25 |
| `metrics_down` | N consecutive failed scrapes of the node's `/metrics` | `--metrics-down-consecutive` (3) | 30 |
| `rpc_down` | N consecutive failed `eth_blockNumber` probes | `--rpc-down-consecutive` (3) | 30 |

Notes:

- **Health score** = `100 − Σ(weights of firing detectors)`, clamped at 0,
  exported as `qfc_watchdog_health_score`. Weights deliberately sum past 100
  — a node with several detectors firing is simply score 0.
- A syncing node legitimately has an old head, so `block_production_stall`
  requires `syncing = 0`; `stuck_sync` covers the syncing case.
- While the node's `/metrics` cannot be scraped, the three node-state
  detectors **freeze**: they keep their last firing state but their
  consecutive-poll counters stop advancing, so action gating cannot progress
  while the watchdog is blind. `metrics_down` covers that situation.
- A flat-but-large compaction debt does not fire (steady-state big DBs are
  fine); the debt trigger needs *above floor AND growing*, mirroring
  `QfcRocksdbCompactionBacklogGrowing`.

Defaults are aligned with the existing alert rules
(`QfcBlockProductionStalled` pages at head age 120 s;
`QfcRocksdbCompactionBacklogGrowing` uses the 8 GiB floor).

## Action mode and gate semantics

OFF by default. Enabled by setting `--action-cmd '<shell command>'`, e.g.
`docker restart qfc-node-1` or `systemctl restart qfc-node`. The command
runs via `sh -c`; every execution is logged with a full evidence dump and
counted (`qfc_watchdog_actions_total`).

All gates must pass, checked in order:

| # | Gate | Rule | Tunable (default) |
|---|---|---|---|
| 1 | trigger | an **action-eligible** detector (default: `block_production_stall` only) has been firing for K consecutive polls | `--action-eligible` (`block_production_stall`), `--action-after-consecutive` (3) |
| 2 | startup grace | never within T of watchdog start | `--startup-grace-secs` (120) |
| 3 | snapshot in-flight | never while a snapshot backup appears to be running: `qfc_snapshot_last_attempt_timestamp_seconds` > `..._last_success...` AND the attempt is recent | `--snapshot-inflight-grace-secs` (1800) |
| 4 | sync progressing | never while the node is syncing and **not** detectably stuck (`stuck_sync` not firing) — a catching-up node is healing itself | — |
| 5 | cooldown | at least T after any previous action | `--action-cooldown-secs` (600) |
| 6 | hourly budget | at most N actions per sliding hour; when spent: error log + `qfc_watchdog_action_budget_exhausted 1`, **no action**, alerting continues | `--max-actions-per-hour` (2) |

What action mode will **not** do:

- act on any detector outside the configured eligible set (no "restart on
  RPC blip");
- act more than the budget allows, ever — exhausted budget means humans;
- act while a snapshot might be writing, while sync is making progress, or
  right after its own start;
- retry instantly: a failed action still spends a budget slot and starts the
  cooldown (a restart command that fails is exactly when you do not want a
  tight loop).

With defaults, the worst case is: a genuinely stalled producer is restarted
after ~45 s of confirmed stall (3 × 15 s polls) — but no earlier than 120 s
after watchdog start — at most twice per hour, ≥10 min apart.

## Configuration

CLI flags with `QFC_WATCHDOG_*` env equivalents (same clap conventions as
`qfc-node`); `qfc-watchdog --help` is the authoritative list. Endpoints:

| Flag | Env | Default |
|---|---|---|
| `--node-metrics-addr` | `QFC_WATCHDOG_NODE_METRICS_ADDR` | `127.0.0.1:6060` |
| `--node-rpc-addr` | `QFC_WATCHDOG_NODE_RPC_ADDR` | `127.0.0.1:8545` |
| `--watchdog-metrics-addr` | `QFC_WATCHDOG_METRICS_ADDR` | `0.0.0.0:6061` |
| `--poll-interval-secs` | `QFC_WATCHDOG_POLL_INTERVAL_SECS` | `15` |
| `--probe-timeout-secs` | `QFC_WATCHDOG_PROBE_TIMEOUT_SECS` | `5` |

The watchdog's own metric inventory and Prometheus scrape config live in
[observability/README.md](observability/README.md); the alert rules are the
`qfc-watchdog` group in
[observability/alert-rules.yaml](observability/alert-rules.yaml)
(health score low → ticket; action taken → ticket; budget exhausted while
unhealthy → page).

## Deployment example (docker compose sidecar)

One watchdog per node, outside the node container so it can restart it. For
the restart action it needs the docker socket (or run detection-only and
omit it):

```yaml
services:
  qfc-node-1:
    image: ghcr.io/qfc-network/qfc-node:latest
    container_name: qfc-node-1
    command: ["--metrics-addr", "0.0.0.0:6060", "--rpc-addr", "0.0.0.0:8545"]
    ports: ["8545:8545", "6060:6060"]

  qfc-watchdog-1:
    image: ghcr.io/qfc-network/qfc-node:latest   # ships qfc-watchdog too
    container_name: qfc-watchdog-1
    entrypoint: ["qfc-watchdog"]
    environment:
      QFC_WATCHDOG_NODE_METRICS_ADDR: "qfc-node-1:6060"
      QFC_WATCHDOG_NODE_RPC_ADDR: "qfc-node-1:8545"
      # Step 2 of the trust path — leave unset for detection-only:
      # QFC_WATCHDOG_ACTION_CMD: "docker restart qfc-node-1"
    ports: ["6061:6061"]
    # Only needed when the action command drives docker:
    # volumes: ["/var/run/docker.sock:/var/run/docker.sock"]
    restart: unless-stopped     # who watches the watchdog: the supervisor
```

For N nodes, repeat the sidecar with different `QFC_WATCHDOG_NODE_*` targets
and a distinct `--watchdog-metrics-addr` port mapping, and add each `:6061`
target to the `qfc-watchdog` Prometheus job.

## Trust-escalation path

Automation earns trust incrementally. Each step runs long enough to prove
itself (suggested: ≥2 weeks or several real incidents) before the next:

1. **Detection-only** (default). Deploy with no `--action-cmd`. Compare
   `qfc_watchdog_detector_firing` against incidents: every real stall must
   be caught, and false positives tuned out *here*, where they are free.
2. **Action on block-production stall** (default eligible set). Enable
   `--action-cmd` with the conservative default gates. Review every
   `QfcWatchdogActionTaken` ticket: was the restart justified? did it fix
   it? Tighten `--max-actions-per-hour` to 1 if confidence is low.
3. **Broader detector set.** Add detectors to `--action-eligible` one at a
   time as evidence accumulates that a restart actually cures them —
   `compaction_stall` (a hard `write_stopped` rarely resolves without
   intervention), then possibly `rpc_down`. `metrics_down` alone is a poor
   restart trigger (often a network/scrape problem); prefer leaving it
   detection-only.

What stays out of scope at every step (roadmap non-goals): actions other
than the single operator-supplied command, cross-node orchestration
(that is T7 canary territory), and any gate-bypass "emergency" mode.

## Implementation notes

- No async runtime, no HTTP/metrics libraries: plain-std HTTP/1.0 probes and
  a hand-rendered exporter on `tiny_http`, mirroring `qfc-node`'s own
  `metrics.rs`.
- Detector and gate logic take the clock as a parameter (`now_secs`), so all
  windows, cooldowns, budgets, and grace periods are unit-tested against
  synthetic metrics text and synthetic time — no sleeps in tests
  (`cargo test -p qfc-watchdog`).
- An optional integration test boots a real dev node and asserts the engine
  scores it 100; like the qfc-node integration tests it requires
  `target/release/qfc-node` and skips when absent.
