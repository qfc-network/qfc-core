# QFC Node Observability (T2)

Dashboard and alert rules for the qfc-node Prometheus exporter
(`crates/qfc-node/src/metrics.rs`) and the RPC metrics middleware
(`crates/qfc-rpc/src/metrics.rs`).

## Exporter

Every node serves Prometheus text-format metrics on
`http://<node>:6060/metrics` (configurable via `--metrics-addr` /
`QFC_METRICS_ADDR`). No extra flags needed — chain, sync, mempool, and RPC
metrics are always on.

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

Commented placeholders at the bottom of the file are intentionally disabled
until their metrics exist:

- **Backup freshness** — enable after T4 exports `qfc_backup_last_success_timestamp_seconds`.
- **RocksDB write stall / compaction backlog** — enable after T2.1 lands the
  qfc-storage statistics hook.

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

Notes:

- `qfc_reorgs_detected_total` counts canonical-chain rewrites observed between
  scrapes (the block hash previously seen at a height changed). Reorgs that
  fully revert between two scrapes are missed; a precise import-time counter
  ships with the T2.1/T3 follow-up (needs a hook in `qfc-chain`).
- RPC histogram buckets span 500µs–10s (14 buckets), tuned so
  `histogram_quantile()` gives usable p50/p99/p999.
- RocksDB metrics (`qfc_rocksdb_*`) are **not exported yet** — T2.1 is
  deferred to a follow-up PR; the exporter has a marked seam for it.
