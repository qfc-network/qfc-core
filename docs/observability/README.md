# QFC Node Observability (T2)

Dashboard and alert rules for the qfc-node Prometheus exporter
(`crates/qfc-node/src/metrics.rs`) and the RPC metrics middleware
(`crates/qfc-rpc/src/metrics.rs`).

## Exporter

Every node serves Prometheus text-format metrics on
`http://<node>:6060/metrics` (configurable via `--metrics-addr` /
`QFC_METRICS_ADDR`). No extra flags needed — chain, sync, mempool, RPC, and
RocksDB *property* metrics (memtable size, pending-compaction bytes, L0 files,
write-stall state) are always on.

RocksDB statistics *counters* (`qfc_rocksdb_*_total`, WAL sync latency)
additionally require starting the node with `--db-statistics` /
`QFC_DB_STATISTICS=1`, which enables RocksDB ticker/histogram collection at the
cost of a few percent of storage throughput. The
`qfc_storage_statistics_enabled` gauge (0/1) tells you which mode a node is in;
with statistics off the counter metrics are simply absent.

Scrape config:

```yaml
scrape_configs:
  - job_name: qfc-node          # alert rules expect job="qfc-node"
    scrape_interval: 15s
    static_configs:
      - targets: ["node-a:6060", "node-b:6060"]
```

## Loading the alert rules

1. Copy `alert-rules.yaml` to your Prometheus config directory.
2. Reference it in `prometheus.yml`:

   ```yaml
   rule_files:
     - alert-rules.yaml
   ```

3. Reload Prometheus (`kill -HUP` or `POST /-/reload`).

The rules include recording rules (`qfc:*`) used by both the burn-rate alerts
and the dashboard SLO panels — load the whole file, not just the alert groups.

SLOs encoded in the rules (multi-window multi-burn-rate, per the Google SRE
Workbook):

| SLO | Target | SLI |
|---|---|---|
| Block production | 99.9% | head block younger than 30s |
| RPC availability | 99.9% | non-client-error responses / total requests |
| RPC latency | 99% | requests completing in < 250ms |

The `qfc-backup-freshness` group (T4.2) is active and watches backup **age**
(`time() - qfc_snapshot_last_success_timestamp_seconds`), not job success — a
pipeline that silently stopped running is the failure mode that loses data.
Threshold convention: ticket at **2× your `--snapshot-interval-secs`** (one
missed run + slack), page at clearly-broken. The interval is deploy-specific
and not exported, so the shipped numbers assume the recommended 1h interval
(ticket 2h, page 12h) — scale both if you run a different interval. The
`qfc_snapshot_*` series only exist on nodes started with
`--snapshot-interval-secs`, so the rules are silent (no-data) elsewhere.

The `qfc-watchdog` group (T6) watches the **watchdog's** own exporter (see
the watchdog section at the bottom of this file) — scrape it as a separate
`job_name: qfc-watchdog`. Its rules only produce data where a `qfc-watchdog`
sidecar runs, and the action-related alerts only matter on nodes where action
mode is enabled (`qfc_watchdog_action_mode_enabled 1`).

The `qfc-rocksdb` group (T2.1) is active. Its property-based alerts
(`QfcRocksdbWriteStopped`, `QfcRocksdbCompactionBacklog*`) work on every node;
the stall-rate alert (`QfcRocksdbWriteStall`) only produces data on nodes
running with `--db-statistics` and never fires elsewhere.

## Loading the Grafana dashboard

UI: *Dashboards → New → Import → Upload JSON file* →
`grafana-dashboard.json` → select your Prometheus datasource when prompted.

Provisioning: drop the JSON into your dashboards provider directory, e.g.

```yaml
# /etc/grafana/provisioning/dashboards/qfc.yaml
apiVersion: 1
providers:
  - name: qfc
    type: file
    options:
      path: /var/lib/grafana/dashboards/qfc
```

and place `grafana-dashboard.json` in that path. The dashboard has an
`$instance` variable populated from `qfc_block_height`.

## Metric inventory (T2.2)

| Metric | Type | Labels | Source |
|---|---|---|---|
| `qfc_block_height` | gauge | — | chain head |
| `qfc_blocks_produced_total` | counter | — | chain head |
| `qfc_transactions_total` | counter | — | blocks (accumulated) |
| `qfc_block_time_seconds` | gauge | — | last two block timestamps |
| `qfc_time_since_last_block_seconds` | gauge | — | head timestamp vs wall clock |
| `qfc_reorgs_detected_total` | counter | — | exporter (scrape-granularity hash check) |
| `qfc_sync_syncing` | gauge (0/1) | — | SyncManager |
| `qfc_sync_highest_peer_block` | gauge | — | SyncManager |
| `qfc_sync_lag_blocks` | gauge | — | SyncManager vs head |
| `qfc_sync_pending_blocks` | gauge | — | SyncManager |
| `qfc_mempool_size` | gauge | — | mempool |
| `qfc_mempool_oldest_tx_age_seconds` | gauge | — | mempool |
| `qfc_peer_count` | gauge | — | libp2p network service |
| `qfc_active_validators` | gauge | — | consensus |
| `qfc_is_validator` | gauge (0/1) | — | consensus |
| `qfc_epoch_number` | gauge | — | consensus |
| `qfc_contribution_score` | gauge | `validator` | consensus |
| `qfc_inference_flops_total` | counter | `validator` | consensus |
| `qfc_inference_score` | gauge | `validator` | consensus |
| `qfc_inference_tasks_completed` | gauge | — | proof pool |
| `qfc_inference_pass_rate` | gauge | — | proof pool |
| `qfc_chain_id` | gauge | — | config |
| `qfc_node_info` | gauge | `version` | build info |
| `qfc_rpc_requests_in_flight` | gauge | — | RPC middleware |
| `qfc_rpc_requests_total` | counter | `method` | RPC middleware |
| `qfc_rpc_request_duration_seconds` | histogram | `method`, `le` | RPC middleware |
| `qfc_rpc_errors_total` | counter | `method`, `code` | RPC middleware |

## Metric inventory — AI task pool quotas (T5)

Quota/cost-attribution metrics from the shared AI task pool, always exported
(`qfc_ai_quotas_enabled` says whether enforcement is on — accounting runs
regardless). Labeled by tenant **priority tier** (`tenant_tier` ∈ `0`/`1`/`2`,
0 = lowest/shed first), never by tenant address: tenant cardinality is
unbounded, tiers are fixed at three. Per-tenant detail lives in the periodic
cost report (structured log, target `qfc::ai_cost`). Full model + operational
notes: [docs/AI-QUOTAS.md](../AI-QUOTAS.md).

| Metric | Type | Labels | Source |
|---|---|---|---|
| `qfc_ai_quotas_enabled` | gauge (0/1) | — | `--ai-quotas` flag |
| `qfc_ai_pending_tasks` | gauge | — | task pool pending queue |
| `qfc_ai_tasks_submitted_total` | counter | `tenant_tier` | admitted public-task submissions |
| `qfc_ai_tasks_rejected_total` | counter | `reason` (`pool_pressure`/`qps`/`in_flight`/`flops_budget`) | quota admission |
| `qfc_ai_flops_metered_total` | counter | `tenant_tier` | `estimated_flops` of completed public tasks |
| `qfc_ai_tenant_inflight` | gauge | `tenant_tier` | tasks in Pending/Assigned, summed per tier |
| `qfc_ai_cost_report_last_timestamp_seconds` | gauge | — | cost-report task (0 = never since start; alert on age, like `qfc_snapshot_*`) |

## Metric inventory — RocksDB (T2.1)

Property-based gauges, always exported (computed on demand from live engine
state; `cf` ranges over all 18 column families):

| Metric | Type | Labels | Source (RocksDB property) |
|---|---|---|---|
| `qfc_rocksdb_memtable_bytes` | gauge | `cf` | `rocksdb.cur-size-all-mem-tables` |
| `qfc_rocksdb_pending_compaction_bytes` | gauge | `cf` | `rocksdb.estimate-pending-compaction-bytes` |
| `qfc_rocksdb_immutable_memtables` | gauge | `cf` | `rocksdb.num-immutable-mem-table` |
| `qfc_rocksdb_l0_files` | gauge | `cf` | `rocksdb.num-files-at-level0` |
| `qfc_rocksdb_block_cache_usage_bytes` | gauge | — | `rocksdb.block-cache-usage` (cache is shared) |
| `qfc_rocksdb_write_stopped` | gauge (0/1) | — | `rocksdb.is-write-stopped` |
| `qfc_rocksdb_actual_delayed_write_rate` | gauge | — | `rocksdb.actual-delayed-write-rate` |
| `qfc_storage_statistics_enabled` | gauge (0/1) | — | `StorageConfig::enable_statistics` |

Statistics counters, only exported when the node runs with `--db-statistics`
(all DB-wide; RocksDB tickers are not per-CF):

| Metric | Type | Labels | Source (RocksDB ticker/histogram) |
|---|---|---|---|
| `qfc_rocksdb_stall_micros_total` | counter | — | `rocksdb.stall.micros` |
| `qfc_rocksdb_block_cache_hits_total` | counter | — | `rocksdb.block.cache.hit` |
| `qfc_rocksdb_block_cache_misses_total` | counter | — | `rocksdb.block.cache.miss` |
| `qfc_rocksdb_bloom_filter_useful_total` | counter | — | `rocksdb.bloom.filter.useful` |
| `qfc_rocksdb_bytes_written_total` | counter | — | `rocksdb.bytes.written` |
| `qfc_rocksdb_bytes_read_total` | counter | — | `rocksdb.bytes.read` |
| `qfc_rocksdb_compaction_read_bytes_total` | counter | — | `rocksdb.compact.read.bytes` |
| `qfc_rocksdb_compaction_write_bytes_total` | counter | — | `rocksdb.compact.write.bytes` |
| `qfc_rocksdb_flush_write_bytes_total` | counter | — | `rocksdb.flush.write.bytes` |
| `qfc_rocksdb_wal_syncs_total` | counter | — | `rocksdb.wal.synced` |
| `qfc_rocksdb_wal_bytes_total` | counter | — | `rocksdb.wal.bytes` |
| `qfc_rocksdb_wal_sync_duration_seconds_sum` | counter | — | `rocksdb.wal.file.sync.micros` (sum) |
| `qfc_rocksdb_wal_sync_duration_seconds_count` | counter | — | `rocksdb.wal.file.sync.micros` (count) |
| `qfc_rocksdb_wal_sync_duration_seconds_p99` | gauge | — | `rocksdb.wal.file.sync.micros` (engine p99 since open) |

Selection rationale: the ticker set is deliberately small — stall time (the
top-level "engine is unhealthy" signal), cache/bloom effectiveness (validates
the T3.1 cache/bloom work), user vs compaction vs flush byte volume (write
amplification = (compaction+flush)/user bytes), and WAL sync behaviour
(durability cost of the T3.2 sync-commit policy). Everything else in the
~200-ticker catalogue is diagnostic rather than alertable and can be read
ad-hoc via `Database::statistics()`.

Notes:

- `qfc_reorgs_detected_total` counts canonical-chain rewrites observed between
  scrapes (the block hash previously seen at a height changed). Reorgs that
  fully revert between two scrapes are missed; a precise import-time counter
  ships with the T2.1/T3 follow-up (needs a hook in `qfc-chain`).
- RPC histogram buckets span 500µs–10s (14 buckets), tuned so
  `histogram_quantile()` gives usable p50/p99/p999.
- WAL sync latency is exported as a cumulative sum/count pair —
  `rate(..._sum[5m]) / rate(..._count[5m])` gives the windowed mean — plus an
  engine-side p99 gauge. RocksDB's C API exposes histogram percentiles, not
  bucket boundaries, so a full Prometheus histogram is not derivable.

## Metric inventory — hot-key / hot-account analytics (T8)

Exported only when the node runs with `--hot-key-sampling <N>` (env
`QFC_HOT_KEY_SAMPLING`; 1-in-N access sampling, N rounded up to a power of
two). When sampling is off, the single gauge `qfc_hot_key_sampling_enabled 0`
is emitted and nothing else — zero per-op cost in the hot path.

Add `--hot-key-window-secs <N>` (env `QFC_HOT_KEY_WINDOW_SECS`, 0 = cumulative)
to make sampling **windowed**: every N seconds the node logs a ranked report
(`tracing` target `qfc::hot_keys`) and resets the sketches, so estimates stay
accurate and these gauges read per-window (a sawtooth) rather than
cumulative-since-start. The full ranked report (the hot *identities* kept out
of the labels here) is also available on demand via the `qfc_hotKeyReport`
JSON-RPC method (optional `topN`).

**Cardinality:** the top-N hot *identities* (raw keys, account addresses, code
hashes) are deliberately **not** Prometheus labels — the hot set churns
window-to-window and would leak unbounded short-lived series (same reasoning
as the T5 tier-only labels). What is exported is bounded and stable: per-CF
traffic estimates (one series per column family) and per-CF / DB-wide **skew**
gauges (the hottest entry's *count*, without its identity). The actual ranked
identities live in `Database::hot_key_report` / `StateDB::hot_account_report`
and in the findings: [docs/profiling/HOT-KEYS.md](../profiling/HOT-KEYS.md).

| Metric | Type | Labels | Source |
|---|---|---|---|
| `qfc_hot_key_sampling_enabled` | gauge (0/1) | — | `--hot-key-sampling` flag (always emitted) |
| `qfc_hot_key_sampling_rate` | gauge | — | effective 1-in-N rate (power of two) |
| `qfc_hot_key_window_start_timestamp_seconds` | gauge | — | window open time (resets with `reset_hot_key_stats`) |
| `qfc_hot_key_estimated_reads` | gauge | `cf` | per-CF sampled reads × rate |
| `qfc_hot_key_estimated_writes` | gauge | `cf` | per-CF sampled writes × rate |
| `qfc_hot_key_top_estimated_count` | gauge | `cf` | hottest key's estimated accesses in each CF (skew) |
| `qfc_hot_account_estimated_reads` | gauge | — | sampled `get_account` × rate |
| `qfc_hot_account_estimated_writes` | gauge | — | sampled `set_account` × rate |
| `qfc_hot_account_estimated_code_reads` | gauge | — | sampled contract-code reads × rate |
| `qfc_hot_account_top_estimated_count` | gauge | — | hottest account's estimated accesses (skew) |
| `qfc_hot_code_top_estimated_count` | gauge | — | hottest bytecode's estimated reads (skew) |

Accuracy caveat: per-CF key stats are complete (the storage sampler is shared
across every `Database` clone). Per-account stats cover only the chain's
persistent state handle (`chain.state()`); the sync path's transient
`state_at` handles carry their own trackers and are not aggregated into the
exporter. The estimates carry space-saving overestimation plus 1-in-N sampling
noise (~`sqrt(N / true_count)` relative) — see HOT-KEYS.md for error bounds.

These metrics are visualized in the **Hot keys & accounts (T8)** row of the
Grafana dashboard (sampling status, per-CF access rate, and the per-CF /
account / bytecode skew gauges). The traffic panels apply `deriv()` to the
cumulative window gauges to show throughput; the skew panels plot the raw
cumulative top-entry counts.

## Snapshot backups (T4.2) and the backup-freshness metrics

Nodes started with `--snapshot-interval-secs <N>` (env
`QFC_SNAPSHOT_INTERVAL_SECS`) take a **live RocksDB snapshot** (file-level
`Checkpoint`: hard links, near-instant, consistent, no stop-the-world — not
to be confused with consensus `ValidatorCheckpoint`s) every N seconds, pack it
as `<prefix>-<height>-<unix_ts>.tar.gz` in `--snapshot-dir` (default
`<datadir>/snapshots`), keep the newest `--snapshot-retain` archives locally
(default 2), and optionally ship each archive to object storage with
`--snapshot-upload-cmd` — an external command with `{file}` substituted by the
archive path, so any store works without bundling an SDK:

```bash
# rclone (S3, GCS, B2, ...)
qfc-node --snapshot-interval-secs 3600 \
  --snapshot-upload-cmd 'rclone copy {file} remote:qfc-backups/'

# aws-cli (S3)
qfc-node --snapshot-interval-secs 3600 \
  --snapshot-upload-cmd 'aws s3 cp {file} s3://qfc-backups/'

# MinIO client
qfc-node --snapshot-interval-secs 3600 \
  --snapshot-upload-cmd 'mc cp {file} minio/qfc-backups/'
```

Each archive extracts to a single directory (named like the archive stem)
that is a complete, directly-openable database plus a
`qfc-snapshot-manifest.json` (format version, creation unix time, block
height, db schema version, column families) consumed by the T4.3 restore
tooling. Upload failures never crash the node: they are logged, counted, and
surfaced through the freshness metrics below.

| Metric | Type | Labels | Source |
|---|---|---|---|
| `qfc_snapshot_last_success_timestamp_seconds` | gauge | — | backup task (0 = never since start) |
| `qfc_snapshot_last_attempt_timestamp_seconds` | gauge | — | backup task (0 = never since start) |
| `qfc_snapshot_failures_total` | counter | — | backup task (snapshot, tar, or upload step failed) |
| `qfc_snapshot_duration_seconds` | gauge | — | last successful run (checkpoint + tar + upload) |
| `qfc_snapshot_size_bytes` | gauge | — | last successfully produced archive |

These are only exported when backups are enabled. Alert on age (see the
`qfc-backup-freshness` group above); `..._last_success... == 0` plus a recent
`..._last_attempt...` means runs are happening but failing — node logs carry
the upload command's stderr.

## Self-healing watchdog (T6) and its metrics

`qfc-watchdog` (crates/qfc-watchdog) is an **out-of-process** sidecar that
observes one node through this exporter plus an `eth_blockNumber` RPC probe,
and serves its own hand-rendered Prometheus endpoint on
`--watchdog-metrics-addr` (default `:6061`). Full design, detector catalog,
gate semantics, and deployment examples: [docs/WATCHDOG.md](../WATCHDOG.md).

Scrape config (one target per watchdog, one watchdog per node):

```yaml
scrape_configs:
  - job_name: qfc-watchdog     # alert rules group `qfc-watchdog`
    scrape_interval: 15s
    static_configs:
      - targets: ["node-a:6061", "node-b:6061"]
```

| Metric | Type | Labels | Source |
|---|---|---|---|
| `qfc_watchdog_health_score` | gauge | — | 100 minus the weights of firing detectors (clamped at 0) |
| `qfc_watchdog_detector_firing` | gauge (0/1) | `detector` | one series per detector (`block_production_stall`, `stuck_sync`, `compaction_stall`, `metrics_down`, `rpc_down`) |
| `qfc_watchdog_polls_total` | counter | — | poll cycles executed |
| `qfc_watchdog_scrape_failures_total` | counter | — | failed scrapes of the node's `/metrics` |
| `qfc_watchdog_rpc_probe_failures_total` | counter | — | failed `eth_blockNumber` probes |
| `qfc_watchdog_actions_total` | counter | — | remediation actions executed (action mode only) |
| `qfc_watchdog_action_failures_total` | counter | — | actions whose command exited non-zero / failed to spawn |
| `qfc_watchdog_action_budget_exhausted` | gauge (0/1) | — | 1 while an action is wanted but `--max-actions-per-hour` is spent |
| `qfc_watchdog_action_last_timestamp_seconds` | gauge | — | unix time of the last action (0 = none since start) |
| `qfc_watchdog_action_mode_enabled` | gauge (0/1) | — | whether `--action-cmd` is configured |
| `qfc_watchdog_info` | gauge | `version` | build info |
