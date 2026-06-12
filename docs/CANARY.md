# Canary Rollout Policy (T7)

Policy and runbook for staging a new `qfc-node` release on one node, watching
SLO guardrails, and promoting to the fleet only after the canary has earned
it. The driver is `scripts/canary.sh`; this document is the policy it
implements.

```
            start                     watch (poll guardrails)
  ┌───────┐ deploy  ┌──────────┐  N clean epochs   ┌──────────┐ promote ┌──────────┐
  │ (idle)│────────>│ watching │──────────────────>│ eligible │────────>│ promoted │
  └───────┘         └────┬─────┘                   └──────────┘         └──────────┘
                         │
                         │ sustained violations (--violations-to-rollback
                         │ consecutive dirty polls) or manual `rollback`
                         v
                  ┌─────────────┐   rollback cmd fails   ┌─────────────────┐
                  │ rolled_back │ <────── OR ──────────> │ rollback_failed │
                  └─────────────┘                        │ (exit 3, human) │
                                                         └─────────────────┘
```

## Rollout policy

1. **One canary.** A new version is deployed to exactly one node
   (prefer a non-critical validator or a full node that serves a slice of
   RPC traffic — it must *receive real load*, or the guardrails see nothing).
2. **N clean epochs.** The canary must complete `--clean-epochs`
   (default **3**) consecutive epoch windows with **zero** guardrail
   violations. Any violation inside a window resets the clean-epoch counter
   to zero — partial credit is not a thing.
3. **Fleet promotion.** Only an `eligible` canary may be promoted.
   `canary.sh promote` enforces this; `--force` exists for emergencies and
   shouts when used.
4. **Auto-rollback.** During the watch, `--violations-to-rollback`
   (default **3**) *consecutive* dirty polls trigger an automatic rollback to
   the recorded baseline version. Requiring consecutive dirty polls means a
   single transient blip (one slow scrape, one GC pause) does not kill a
   canary, while a sustained regression is reverted within
   `3 × --poll-secs` ≈ 45s of onset.
5. **Rollback never retries.** If the rollback command itself fails, the
   driver stops, sets phase `rollback_failed`, and exits 3. A failing deploy
   command must never be retry-looped — that is how one broken node becomes
   four.

### Epoch windows

Consensus epochs are deploy-specific (`qfc_types::BLOCKS_PER_EPOCH` is **3**
on the dev chain; testnets configure their own). The driver therefore counts
its own windows:

- `--epoch-blocks N` (default 3): a window closes when the canary's head
  advances N blocks past the window start. Set this to your deployment's
  real epoch length.
- `--clean-window-secs S`: time-based windows instead, for chains where
  block-height windows are inconvenient (e.g. very fast or very slow chains).

Pick window sizes so that **3 clean epochs ≥ ~15–30 minutes of real traffic**.
Shorter than that and you are promoting on noise.

## Guardrail catalog

Defaults are aligned with `docs/observability/alert-rules.yaml` (T2) so the
canary trips at or before the point the pager would.

| Guardrail | Signal | Default threshold | Rationale |
|---|---|---|---|
| `block_age` | `qfc_time_since_last_block_seconds` | ≤ 30s (`--max-block-age`) | Block-production SLO SLI: head older than 30s = stalled (alert-rules `qfc:block_production_stalled`). |
| `height_advance` | `qfc_block_height` static for > `--max-block-age` s of *observer* wall clock | 30s | Complements `block_age`: trusts only observed height, so a node with a wedged clock or bogus head timestamps still trips. Decoupled from `--poll-secs` (chains with block interval > poll interval must not false-positive). |
| `height_lag` | `max(fleet qfc_block_height) − canary height` | ≤ 10 blocks (`--max-height-lag`) | Canary must keep pace with the fleet, not merely move. Prometheus mode only. |
| `rpc_errors` | server-side error ratio, client codes `-32700/-32600/-32601/-32602` excluded (same set as the SLO recording rules) | ≤ 1% absolute (`--max-rpc-error-rate`); ≤ 1.5× fleet (`--fleet-ratio`) | RPC availability SLO is 99.9%; 1% sustained on the canary is unambiguous. The fleet-ratio check catches regressions that stay under the absolute bar. |
| `rpc_p99` | p99 of `qfc_rpc_request_duration_seconds` | ≤ 0.5s absolute (`--max-rpc-p99`); ≤ 1.5× fleet | Latency SLO is 99% < 250ms; p99 at 2× that budget is a regression worth reverting. |
| `sync` | `qfc_sync_lag_blocks`, `qfc_sync_syncing` | lag ≤ 50 (`--max-sync-lag`); not syncing with lag > `--max-height-lag` | Matches the `QfcSyncLagHigh` alert (lag > 50). A canary stuck in catch-up is not serving its role. |
| `storage` | `qfc_rocksdb_write_stopped`, `Σ qfc_rocksdb_pending_compaction_bytes` | write_stopped = 0; compaction debt ≤ 64 GiB (`--max-compaction-gb`) | Hard write stall is an immediate fail (matches `QfcRocksdbWriteStopped`); compaction-debt ceiling matches `QfcRocksdbCompactionBacklog`. New versions that regress compaction behaviour show up here first. |
| `scrape` | exporter reachable | must answer | An unreachable canary is a violated canary — crash loops must not read as "no data, no problem". |

Fleet-ratio checks apply **noise floors** (error ratio 0.5%, p99 100ms): a
canary at 0.2% errors vs a fleet at 0.1% is "2×" but not a regression. Below
the floor, ratio checks pass unconditionally.

**Missing data is a violation, not a pass**, for signals that must exist
(`block_age`, height). Optional subsystems (sync without peers, fleet series
in a single-node deploy) are `skipped` and reported as such — visible in
`status`, never silently counted as clean.

## Rollback criteria

Automatic, from `watch`:

- `--violations-to-rollback` (default 3) consecutive dirty polls, any
  guardrail. The rollback redeploys the **recorded baseline version** (taken
  at `start` from `qfc_nodeInfo` RPC / the `qfc_node_info{version=...}`
  metric, or `--current-version`).

Manual, any time:

- `canary.sh rollback` — same path, same audit trail. Use it when a human
  spots something the guardrails don't encode (log spew, consensus weirdness
  on other nodes, a bad changelog discovery).

After any rollback the rollout is dead: fix, cut a new version, start a new
canary. There is no "resume" of a rolled-back canary by design.

## Observation modes

- **Prometheus** (`--prom-url` / `PROM_URL` + `--canary-instance`): full
  catalog including fleet-relative checks. Queries use 5m rate windows,
  mirroring the T2 recording rules.
- **Direct `/metrics` fallback** (`--metrics-url`): no Prometheus needed.
  The driver scrapes the canary's exporter and computes error-rate and p99
  from counter/histogram deltas between its own polls. Fleet-relative checks
  are skipped (and reported as skipped). This mode is also how the guardrail
  logic is tested against a local `--dev` node.

## Interaction with backup/DR (T4)

- **Snapshot before promote.** Promotion touches every node; take a fresh
  snapshot first. With the T4.2 pipeline this is automatic on nodes running
  `--snapshot-interval-secs`, but verify freshness before promoting:
  `time() - qfc_snapshot_last_success_timestamp_seconds` should be well under
  your interval on all nodes. A promote with stale backups is a promote
  without a parachute.
- **Rollback ≠ restore.** Canary rollback redeploys the baseline *binary*;
  it does not touch data. If a bad version corrupted state (schema bump,
  bad migration), rollback alone is insufficient — restore the affected
  node from a snapshot via `scripts/restore.sh` (see `docs/DR.md`), then
  redeploy the baseline.
- **Schema-bump releases get extra care:** a version that migrates the DB
  forward may not be downgradeable. For those, the policy is: snapshot the
  canary's datadir immediately before `start`, and treat any rollback as
  restore-from-snapshot + baseline redeploy.

## Wiring into the qfc-testnet compose deployment

The fleet runs docker compose on 4 VPS with images from
`ghcr.io/qfc-network/`. The driver is deployment-agnostic; the compose
specifics live entirely in the three command flags (placeholders here — the
qfc-testnet repo is private):

```bash
export PROM_URL=http://<prometheus-host>:9090
DEPLOY='ssh {node} "cd /opt/qfc && QFC_TAG={version} docker compose pull qfc-node && QFC_TAG={version} docker compose up -d qfc-node"'
PROMOTE='for h in <node-a> <node-c> <node-d>; do ssh $h "cd /opt/qfc && QFC_TAG={version} docker compose pull qfc-node && QFC_TAG={version} docker compose up -d qfc-node"; done'

scripts/canary.sh start --version <ghcr-tag> --node <node-b> \
  --canary-instance <node-b>:6060 --rpc-url http://<node-b>:8545 \
  --deploy-cmd "$DEPLOY" --rollback-cmd "$DEPLOY" --promote-cmd "$PROMOTE" \
  --epoch-blocks <your-epoch-length> --wait
```

Operational notes for this fleet:

- The compose file must take the image tag from `QFC_TAG` (e.g.
  `image: ghcr.io/qfc-network/qfc-node:${QFC_TAG:-latest}`) for the
  placeholder substitution to mean anything. Pin the running tag in `.env`
  per GitOps convention — commit + push the tag change before/after the
  canary run so the repo matches reality.
- Run the driver from a box that can SSH to the nodes **as a user with GHCR
  credentials** — on at least one VPS, root has no GHCR auth and
  `sudo docker compose pull` fails.
- The state file is local to wherever the driver runs. Keep one rollout =
  one state file; check it into your ops scratch space if you want the audit
  trail preserved.

## Runbook: stuck or ambiguous canary

| Symptom | Reading | Action |
|---|---|---|
| `watch` loops with everything `skipped` | Canary gets no RPC traffic / has no peers — guardrails are blind. | Don't promote on a blind canary. Route some real traffic at it or extend the watch with synthetic load, then re-watch. |
| Clean-epoch counter keeps resetting on one flapping guardrail | Threshold too tight for this deployment (e.g. `block_age` < real block cadence — a single dev validator can legitimately go 30s+ between wins). | Check `status` for which guardrail flaps and its values; re-run `watch` with an adjusted threshold *if the value is healthy for your chain*; otherwise it's a real regression — roll back. |
| `phase: rollback_failed` (exit 3) | The bad version may still be running AND the rollback didn't land. | Highest-priority manual intervention: redeploy the baseline by hand (`status` shows the exact command and its exit code), verify with `qfc_nodeInfo`, then `start --force` a fresh state when resolved. |
| `watch` exits 4 (`--max-duration-secs`) | No verdict either way within the time budget. | Inspect `status`. Usually means windows are too long or traffic too thin. Resume with `watch` (state is persistent) or widen the budget. |
| Canary healthy but fleet degrades during watch | Guardrails watch the canary, not the fleet (the fleet is the *baseline* in ratio checks — a degrading fleet makes the canary look *better*). | The T2 alerts own fleet health. If the fleet pages during a canary, freeze: no promote until the fleet is understood. |
| Dirty polls from `scrape` only | Exporter down ≠ node down (metrics server is a separate thread). | Check `qfc_nodeInfo` RPC and container state. If only the exporter died, that is still a regression worth investigating — it shipped with the new version. |
| State file lost mid-watch | Driver can't resume (baseline version was in it). | The audit trail is gone but the fleet is fine. Recover the baseline version from the previous image tag / git history, `start --force` with explicit `--current-version`, and re-earn the clean epochs from scratch. |

## Future work

- **Automated per-PR canary in CI:** on a release-candidate tag, CI deploys
  to a dedicated canary node in qfc-testnet, runs
  `canary.sh start --wait --max-duration-secs <budget>` with a synthetic RPC
  load generator, and gates the release on exit 0. Exit 2 attaches the state
  file (guardrail evaluations + audit trail) to the PR.
- **Webhook notifications:** post phase transitions (eligible / rolled_back /
  rollback_failed) to the alerting channel instead of relying on the
  operator's terminal.
- **Multi-canary stages:** 1 node → 25% → 100% with per-stage clean-epoch
  requirements, once the fleet is large enough for percentages to mean
  anything (at 4 nodes, it isn't).
- **Version verification post-deploy:** after `--deploy-cmd`, poll
  `qfc_nodeInfo` until the reported version matches the target before
  starting guardrail accounting (today a deploy that silently no-ops would
  be watched — and likely pass, since nothing changed).
