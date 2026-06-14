# T8 — Hot-key analytics: design, overhead, findings

SRE roadmap T8 (`docs/ROADMAP-SRE.md`). Per-CF and per-account access sampling
in `qfc-storage`/`qfc-state`, with top-N heavy-hitter reports. Feeds cache
sizing for T3 and is the chain-side mirror of embedding hot-key skew in the AI
stack.

## Design

Two layers, because raw storage keys are not attributable to accounts:

1. **`qfc_storage::hotkeys::HotKeySampler`** — wired into `Database`
   `get`/`put`/`delete`/`contains`/`write_batch{,_sync}`. Enabled via
   `StorageConfig::hot_key_sampling: Option<u32>` (1-in-N, rounded up to a
   power of two so the sample check is a mask, not a division; default rate
   constant is 64). Per column family: sampled read/write counters plus a
   fixed-capacity **space-saving** heavy-hitter sketch (Metwally et al. 2005,
   default 256 entries/CF) over the sampled keys. Read via
   `Database::hot_key_report(top_n)`, windowed via
   `Database::reset_hot_key_stats()`.
2. **`qfc_state::HotAccountTracker`** — the `state` CF stores trie nodes keyed
   by *node hash*, which cannot be mapped back to accounts (and churns on
   every commit — see findings). So `StateDB` samples its own API
   (`get_account`/`set_account`/`get_code`) into sketches keyed by `Address`
   and code `Hash`, **before** the LRU caches, so the report reflects the
   logical access distribution that cache sizing needs. Enabled automatically
   at the same rate/capacity when the underlying `Database` has sampling on.
   Read via `StateDB::hot_account_report(top_n)` /
   `reset_hot_account_stats()`.

Cost model:

- **Disabled** (default): the `Database`/`StateDB` holds no sampler at all —
  per-op cost is a single `Option` branch. No atomics, no allocation.
- **Enabled, non-sampled op** (63 of 64): one relaxed `fetch_add` + mask
  check.
- **Enabled, sampled op** (1 of 64): one relaxed counter increment + mutex +
  sketch offer. Already-tracked keys are a hash-map hit and a counter bump;
  new keys on a full sketch take an O(capacity) min-scan eviction
  (L1/L2-resident at capacity 256).

### Error bounds (what the report numbers mean)

Let `S` = sampled offers absorbed by a sketch in the window, `C` = sketch
capacity (256), `N` = sampling rate (64).

1. **Space-saving guarantee** (within sampled counts): for every tracked key,
   `sampled_count − error ≤ true_sampled ≤ sampled_count`, with
   `error ≤ S/C`. Every key whose true sampled count exceeds `S/C` is
   guaranteed present. The report exposes `max_overestimate = error × N`; when
   `max_overestimate ≈ estimated_count` the entry is churn noise, not a real
   heavy hitter (diagnostic in itself — see the trie-node finding below).
2. **Sampling noise**: `estimated_count = sampled_count × N` estimates the
   true count `T` with relative error ≈ `sqrt(N/T)` — e.g. ±5.7 % at
   `T = 20 000`, ±18 % at `T = 2 000`. Genuinely hot keys (large `T`) are
   exactly the regime where this is small.
3. **Stride-sampling caveat**: deterministic 1-in-N can alias with access
   loops whose period divides N. Hash-keyed blockchain traffic is fine;
   strictly periodic synthetic loops should be interpreted accordingly.

## Overhead benchmarks

`cargo bench -p qfc-storage`. The `*_sampled` groups are structurally
identical to their unsampled twins but open the DB with
`hot_key_sampling: Some(64)`. Numbers below are Criterion medians from the
focused back-to-back run on an otherwise idle machine (2026-06-12, macOS,
same binary); baseline is `main` @ a762936.

| Benchmark | main baseline | branch, sampling disabled | branch, sampling 1/64 | enabled vs disabled |
|---|---|---|---|---|
| `storage_put/256b` | 1.81 µs | 1.55 µs | 1.70 µs | +9.3 % (+0.15 µs/op) |
| `storage_get/from_10000_keys` | 398 ns | 331 ns | 356 ns | +7.4 % (+25 ns/op) |
| `storage_batch_write/ops/500` | 79.1 µs | 79.3 µs | 97.8 µs | +23 % (+37 ns/batched-op) |

Honest reading:

- **Disabled vs baseline: no measurable delta.** Across three independent
  runs the branch-with-sampling-disabled numbers straddle the main baseline
  (puts ran *faster* than baseline in all three runs, gets within the
  run-to-run band). The disabled path is one branch; any true cost is below
  environment noise, which for these RocksDB microbenches is large
  (`storage_batch_write/ops/500` with *identical* code ranged 79–152 µs
  across earlier preliminary runs — treat single-run percentages
  accordingly).
- **Enabled overhead is tens of ns/op and these benches are the sketch's
  worst case**: keys are all-distinct and monotonically increasing, so every
  sampled offer misses the sketch and takes the O(256) eviction scan +
  allocation. Real hot-key traffic (the workload below) takes the cheap
  already-tracked increment path. The absolute penalty — ~25 ns/get,
  ~37 ns/batched-write — is small against RocksDB op costs and applies only
  while sampling is switched on.

Earlier full-suite runs (preliminary, same ordering of magnitude):
baseline `/tmp/t8_bench_baseline_main.txt`, branch runs
`/tmp/t8_bench_branch.txt` / `/tmp/t8_bench_branch2.txt`.

## Findings: synthetic block-import workload

Generator: `cargo run -p qfc-state --release --example hot_key_workload
[transfers]` — 200 000 zipf(s = 1.1)-distributed transfers over 5 000
accounts, one zipf(s = 1.2) contract-code read per 4 transfers over 100
contracts, commit every 500 transfers; sampling at the production default
1-in-64; counters reset after seeding so only steady state is measured.
Throughput on the dev machine: ≈ 39 k transfers/s.

### Account layer: clean power law, rank order recovered

Total estimated account ops 1.41 M (829 k reads, 581 k writes; writes are
`set_account`, i.e. transfer + nonce updates).

| report rank | true zipf rank | estimated count | max_overestimate |
|---|---|---|---|
| 1 | 0 | 194 560 | 0 |
| 2 | 1 | 101 056 | 0 |
| 3 | 2 | 66 624 | 0 |
| 5 | 4 | 36 928 | 0 |
| 10 | 9 | 17 792 | 0 |
| 20 | 18 | 8 768 | 0 |

- Successive-rank ratios ≈ 1.9–2.0 ≈ 2^1.1 — the configured zipf exponent is
  recovered; `max_overestimate = 0` for the whole top-20 (hot entries were
  never evicted).
- Rank order is essentially exact: report ranks 1–13 are true ranks 0–12 with
  a single adjacent swap; one "intruder" at report rank 14 is the hottest
  *contract* address — `get_code` funnels through `get_account`, so contract
  accounts correctly show up in account traffic too.
- **Concentration**: the top 13 accounts (0.26 % of 5 000) carry ≈ 43 % of
  all account traffic; the top 20 carry ≈ 48 %.

### Code layer: even more concentrated

Estimated code reads 52.9 k. Top contract = 28 % of all code reads; top 4 of
100 contracts = 53 %; top 20 = 81 %. All `max_overestimate = 0`.

### Storage layer: trie-node keys churn — no stable hot keys

The `state` CF saw an estimated **3.38 M writes** for 200 k transfers
(≈ 17 trie-node writes per transfer — path copying on every commit) but only
**51 k reads** at the storage layer: the `StateDB` account LRU (10 k entries
≥ the 5 k-account working set) absorbs virtually all logical reads before
RocksDB. The per-CF sketch for `state` is fully saturated churn: every top
entry sits at sampled_count 210 with `max_overestimate` 13 376 (≈ 99.5 % of
the estimate) — i.e. **there are no stable hot keys at trie-node granularity**,
because node hashes change on every write. This is the empirical
justification for the two-layer design: account attribution must happen in
`qfc-state`, and the storage-layer sketch is mainly useful for CFs with
stable keys (code, transactions, receipts) and for per-CF traffic *volume*.

### Implications for the T3 per-CF cache split

1. **Weight the `state` CF by traffic volume, not key reuse.** It dominates
   ops (3.4 M writes vs zero observed traffic on most other CFs in this
   workload), but its keys churn — size its share for write-path health
   (write buffer, upper-trie-level block reuse during commit) rather than
   expecting stable per-key block-cache hits.
2. **The in-process account LRU is the real read cache and it is already
   ample.** A few-hundred-entry account cache would capture ~40–50 % of
   logical account traffic under this skew; the existing 10 k-entry LRU
   covers the entire working set. Do not grow it; per-CF block cache for
   `state` buys little for reads while the LRU is warm (cold-start/restart is
   the exception).
3. **`code` CF block-cache share can be small.** Code access is extremely
   top-heavy (top 4 bytecodes = 53 % of reads) and the code LRU
   (content-addressed) absorbs it; allocate the `code` CF a token share.
4. **Give zero-traffic CFs the minimum.** The report omits CFs with no
   sampled traffic — under block-import-style load that is most of the 18+
   CFs; the report is the live input for rebalancing on real nodes.

### Caveats

- **Synthetic dev workload ≠ testnet.** Single process, no RPC read traffic,
  no EVM execution, no compaction pressure from history, zipf exponents
  chosen a priori. Real skew (and the read/write mix) must be re-measured on
  a testnet node before committing T3 cache numbers; the mechanism (enable
  `hot_key_sampling`, window with `reset_*`, dump both reports) is what this
  item delivers.
- Storage-level read counts reflect *cache misses* of the layers above, by
  design at that layer; the account-level report is the logical distribution.
- Counts are estimates: ±`sqrt(64/T)` relative sampling noise plus the
  space-saving bound surfaced per entry as `max_overestimate`.

## Follow-up

**Exporter wiring — done.** The Prometheus surface for `hot_key_report` /
`hot_account_report` is wired into the node's `/metrics` endpoint, gated on
`--hot-key-sampling <N>` (env `QFC_HOT_KEY_SAMPLING`). To avoid leaking
churning identities as labels, the exporter publishes bounded aggregates and
skew gauges only (per-CF traffic estimates, hottest-entry counts); the ranked
identities above stay in the report accessors. Full metric inventory:
[docs/observability/README.md](../observability/README.md#metric-inventory--hot-key--hot-account-analytics-t8).

**Grafana panel — done.** The *Hot keys & accounts (T8)* dashboard row plots
sampling status, per-CF access rate (`deriv()` of the window gauges), and the
per-CF / account / bytecode skew gauges.

**Window reset scheduler — done.** `--hot-key-window-secs <N>` (env
`QFC_HOT_KEY_WINDOW_SECS`, 0 = cumulative) makes sampling *windowed*: every N
seconds the node logs a ranked report (`tracing` target `qfc::hot_keys` — the
permanent record of the hot identities kept out of Prometheus) and then resets
both sketches. This keeps the space-saving estimates accurate (the `state`-CF
saturation noted above is bounded to one window) and makes the `/metrics`
gauges read per-window. With a window set the gauges are a sawtooth; the
dashboard's traffic panels already apply `deriv()`, so they read correctly
either way.

Still open: an RPC method to fetch the full ranked `hot_key_report` on demand
(the windowed `qfc::hot_keys` log is the current record of ranked identities).
