# Disaster Recovery Runbook (T4)

Scripted, rehearsed restore of a QFC node from snapshot backups, with measured
RPO/RTO. Companion tooling:

- `scripts/restore.sh` — the restore path (fetch → verify → place → restart → validate)
- `scripts/gameday.sh` — repeatable local chaos drill that measures RPO/RTO
- `docs/observability/alert-rules.yaml` — the backup-freshness alerts that tell
  you this runbook is about to matter

## 1. Architecture recap — what you are restoring

### 1.1 What's in a snapshot (T4.2)

A node started with `--snapshot-interval-secs N` periodically produces
`<prefix>-<height>-<unix_ts>.tar.gz` (default prefix `qfc-snapshot`, default
location `<datadir>/snapshots`, overridable with `--snapshot-dir`; optionally
shipped off-box via `--snapshot-upload-cmd`, with `{file}` substituted).

Each archive contains **one top-level directory** named like the archive stem,
holding:

- a complete, point-in-time-consistent **RocksDB database** — a file-level
  RocksDB `Checkpoint` of the live DB (hard links of SSTs + copies of
  MANIFEST/CURRENT/OPTIONS, memtables flushed first, so no WAL replay needed).
  All 18 column families are captured: headers, bodies, transactions,
  receipts, state, code, metadata, validators, **checkpoints**, etc.
- `qfc-snapshot-manifest.json` — `{format_version: 1, created_at_unix,
  block_height, db_version, column_families[]}`. The restore script refuses
  archives whose manifest it does not understand.

The extracted directory **is** a node database: place it at `<datadir>/db` and
start the node. No import step.

### 1.2 What the consensus checkpoint adds (T4.1)

Independently of file-level snapshots, consensus writes a
`ValidatorCheckpoint` (validator set, epoch, scores) into the `checkpoints`
column family at every epoch boundary. On startup the node restores
validator/epoch state from the latest checkpoint and replays **at most one
epoch of blocks** instead of re-deriving consensus state from genesis.

Because the `checkpoints` CF lives *inside* the DB, every snapshot archive
carries the consensus checkpoints too — a restored snapshot fast-restarts the
same way a cleanly restarted node does. Log line to confirm:

```
Loaded validator checkpoint: epoch=…, height=…, validators=…
```

### 1.3 The two recovery paths

| | Path A — snapshot restore | Path B — checkpoint fast restart |
|---|---|---|
| When | Datadir lost/corrupt (disk loss, fat-finger `rm`, bad host) | Process died / host rebooted, datadir intact |
| Data source | Newest snapshot archive (local dir or object storage) | The on-disk DB itself |
| RPO | Up to one snapshot interval | 0 (RocksDB WAL recovers in-flight writes) |
| RTO driver | Archive fetch + untar + node start | Node start only |
| Catch-up | Blocks since the snapshot re-sync from peers | At most one epoch replayed locally |
| Tool | `scripts/restore.sh` | just restart the service |

A third path — **full peer re-sync from genesis** (no local data at all) — is
the fallback when no usable snapshot exists. It is bounded by network sync
throughput, grows with chain length, and is not yet timed by the game-day
script (see §6).

## 2. Measured RPO/RTO (game-day results)

Measured by `scripts/gameday.sh` on **2026-06-12**, local dev Mac
(Apple M5 Max, 36 GB RAM, macOS 26.5, release build). Drill parameters: dev
node (`--dev --no-network`, 3 s block interval), 15 s snapshot interval,
SIGKILL + `rm -rf` of the datadir, restore from the newest local archive via
`scripts/restore.sh`, RTO clock from datadir destruction until RPC serves an
*advancing* `eth_blockNumber`.

| Run | Path A: RPO (blocks) | Path A: RTO | Path B: RPO | Path B: RTO |
|-----|----------------------|-------------|-------------|-------------|
| 1 | 1 | 13.2 s | 0 | 30.1 s |
| 2 | 1 | 12.8 s | 0 | 12.1 s |
| 3 | 1 | 37.6 s | 0 | 27.3 s |

Reading the numbers:

- **RPO**: 1 block lost out of a 15 s interval / 3 s block time (worst case 5).
  In production RPO ≈ the snapshot interval — pick `--snapshot-interval-secs`
  from your data-loss tolerance, and remember the freshness *alert* thresholds
  assume a 1 h interval (§5).
- **RTO variance is not the restore.** The mechanical restore — verify +
  untar + place + DB open + checkpoint load + RPC up — is **≈ 1 s** end to end
  (node boot from snapshot to serving RPC was < 15 ms in the logs). The rest
  of the RTO, and all of its variance, is waiting for the dev node to win a
  VRF leader slot and produce the *next* block (genesis registers 4
  validators; 3 are offline in the drill, so ~¾ of 3 s slots are skipped).
  On a real testnet, "serving reads" recovers in ~1 s + fetch time; "head
  advancing" depends on the network producing blocks, not on the node.
- Path A numbers use a **local** archive: add object-store download time for a
  true off-site restore (archive size scales with state; drill archives were
  ~20 KB, testnet archives will be larger — measure with
  `restore.sh --verify-only` which times fetch+verify without touching data).

Re-measure with: `scripts/gameday.sh` (~2 min, needs ports 18545/16060).

## 3. Restore procedure — testnet VPS

Assumptions (adapt paths/units to the host): node runs as a systemd service
`qfc-testnet-node`, datadir `/var/lib/qfc` (DB at `/var/lib/qfc/db`), archives
shipped by `--snapshot-upload-cmd "rclone copy {file} remote:qfc-backups/"`.

**0. Decide the path.** Datadir intact and healthy disk → just
`systemctl restart qfc-testnet-node` (Path B) and verify (§4). Datadir lost or
RocksDB corruption errors in logs → continue (Path A).

**1. Stop the node** (idempotent if already dead):

```sh
systemctl stop qfc-testnet-node
```

**2. Pick an archive.** Newest local one, if the snapshot dir survived:

```sh
ls -t /var/lib/qfc/snapshots/qfc-snapshot-*.tar.gz | head -3
```

or list remote ones: `rclone lsl remote:qfc-backups/ | sort -k2,3 | tail -3`.
Archive names embed `<height>-<unix_ts>` — sanity-check the height against
the explorer / a healthy peer before restoring something stale.

**3. Restore.** Local archive (or directory — newest is picked):

```sh
scripts/restore.sh \
  --datadir /var/lib/qfc --force \
  --start-cmd 'systemctl start qfc-testnet-node' \
  --peer-rpc-url http://<healthy-peer>:8545 \
  /var/lib/qfc/snapshots
```

Remote archive — same command with a fetch hook instead of a path (mirrors
the upload convention; `{file}` is the local destination):

```sh
scripts/restore.sh \
  --fetch-cmd 'rclone copyto remote:qfc-backups/qfc-snapshot-<H>-<TS>.tar.gz {file}' \
  --datadir /var/lib/qfc --force \
  --start-cmd 'systemctl start qfc-testnet-node' \
  --peer-rpc-url http://<healthy-peer>:8545
```

The script verifies tar integrity and the manifest (format version, column
families, height/age) **before** touching the datadir, preserves any existing
DB as `/var/lib/qfc/db.bak.<ts>` (never deletes it), starts the node, and
polls RPC until the block number advances — comparing against the peer if
given. Exit code 2 = DB placed but validation timed out: read the node logs
before re-running. `--verify-only` checks an archive without restoring;
`--help` has all flags.

**4. Clean up later.** After the node has been healthy for a day, remove
`db.bak.*` dirs to reclaim disk.

## 4. Post-restore validation checklist

- `restore.sh` printed `VALIDATED: block number advancing` (or do it manually:
  two `eth_blockNumber` calls a few seconds apart must differ).
- Startup log shows `Loaded validator checkpoint: epoch=…` — consensus
  fast-restarted instead of falling back to genesis validators.
- `qfc_sync_lag_blocks` (metrics, `:6060/metrics`) trending to 0; the restore
  script warns if the peer is > 50 blocks ahead (the `QfcSyncLagHigh` alert
  watches the same signal).
- Backup pipeline resumed: `qfc_snapshot_last_success_timestamp_seconds`
  advancing again — a restored node must immediately rebuild its own DR
  coverage.

## 5. Alert linkage

From `docs/observability/alert-rules.yaml` (group `qfc-backup-freshness`) —
these alerts protect the **RPO** and fire *before* a disaster:

- `QfcSnapshotBackupStale` (ticket, > 2 h): one missed interval + slack — fix
  the backup pipeline now; thresholds assume a 1 h interval, scale if yours
  differs (ticket = 2 × interval).
- `QfcSnapshotBackupVeryStale` (page, > 12 h): DR coverage is effectively
  gone; a node loss now means a very stale restore or a full peer re-sync.

Alerts that mean you may be *executing* this runbook shortly:
`QfcNodeDown`, `QfcBlockProductionStalled`, `QfcRocksdbWriteStopped`.

## 6. Game-day cadence & follow-ups

- **Local drill (`scripts/gameday.sh`)**: run after any change to
  `qfc-storage` snapshot/DB code, the backup task, or `restore.sh` itself —
  it is the integration test for this runbook (~2 min). Update the table in
  §2 when numbers move materially.
- **Testnet game day**: quarterly, on one designated testnet validator
  (VPS): stop it, move the datadir aside, restore from object storage with
  `restore.sh`, record fetch time + RTO at real archive sizes, and append the
  results to §2. The first testnet run should also validate the
  `--snapshot-upload-cmd`/`--fetch-cmd` pair against the real object store.
- **Follow-up (not yet measured): peer re-sync timing.** The third recovery
  path — empty datadir, full sync from peers — needs a multi-node network to
  time meaningfully (a 2-node local rig measures loopback sync, not reality).
  Do it as part of the first testnet game day: wipe the drill validator's
  datadir *without* restoring and time sync-to-head; record alongside §2 so
  the "restore vs re-sync" decision in §1.3 has real numbers.
