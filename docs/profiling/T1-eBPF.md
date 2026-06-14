# T1 — eBPF profiling capture (QFC SRE roadmap)

Kernel-level traces of a live `qfc-node` validator, captured per
[ROADMAP-SRE.md](../ROADMAP-SRE.md) T1. This is the *evidence* item: the
optimization targets it surfaces feed T3 (engine hardening) and the
operational findings feed the observability / log-retention work.

**Raw artifacts:** [`ebpf/`](ebpf/) — one file per capture, plus the on-CPU
folded stacks. **Reproduce:** [`scripts/profiling/`](../../scripts/profiling/)
(`capture.sh` + the four `.bt` programs).

## Capture context

| | |
|---|---|
| Host | AWS, Ubuntu 24.04.4, kernel **6.17.0-1007-aws**, **aarch64** |
| Disk | `nvme0n1` 100G, non-rotational — **EBS-backed** (service time ~ms, not local-NVMe µs) |
| Target | `qfc-node-1`, image `ghcr.io/qfc-network/qfc-core:staging-sha-8cf3cb0` |
| Tools | `bpftrace v0.20.2`, `perf 6.17.9`, kernel BTF present |
| Load | **steady-state testnet** (natural block production; no synthetic stress) |

> The deployed `staging-sha-8cf3cb0` image **predates the SRE branch** (T2–T8,
> PRs #103–#121 on `main`). Read the findings as a *baseline of the current
> production build*, not of the merged-but-undeployed work. Where a merged PR
> would change a result, it's called out.

## Findings

### 1. ~47% of on-CPU time is BLAKE3 running the **portable (scalar) backend** — `05-oncpu`

On-CPU sampling of `qfc-node-1` (20s @ 99 Hz, 1987 samples,
[`ebpf/05-oncpu-report.txt`](ebpf/05-oncpu-report.txt),
[`ebpf/05-oncpu.folded`](ebpf/05-oncpu.folded)):

| Area | Samples | Share |
|---|---|---|
| `blake3::portable::compress_in_place` + `ChunkState::update` | 935 | **47.1%** |
| log formatting (`tracing_subscriber::fmt` + `core::fmt::write`) | 335 | **16.9%** |
| allocator (`malloc`/`cfree`) | 197 | 9.9% |
| RocksDB `Get` (serving block/sync reads from memtable) | 67 | 3.4% |

The hashing symbol is `blake3::**portable**::compress_in_place` — the scalar
fallback. On this **aarch64** host BLAKE3 should use the **NEON** SIMD backend;
the portable path means the build isn't compiling BLAKE3's SIMD
implementation (missing target-feature / `rust-target-cpu`, or the `blake3`
crate built without NEON). Roughly **half of node CPU** is recoverable hashing
work. **Highest-value optimization target — feeds a T3 follow-up.** Validate
the backend selection in the `blake3` build and benchmark NEON vs portable
(`cargo bench` on the hashing path) before/after.

### 2. Sync-protocol response **logging** is ~17% of on-CPU and ~20k write()/s — `05-oncpu`, `04`

The second-largest on-CPU consumer is formatting `qfc_network::sync_protocol::SyncResponse`
into log lines: `NetworkService::start … tracing_subscriber::fmt::format …
EscapingWriter … core::fmt::write`. The write-path syscall capture
([`ebpf/04-writepath-syscalls.txt`](ebpf/04-writepath-syscalls.txt)) shows the
cost downstream: **~1.4M `write()` syscalls in 25s**, with `tokio-rt-worker`
(~500k) mirrored almost exactly by `containerd-shim` (~260k) and `dockerd`
(~570k) — i.e. qfc-node stdout → Docker json-file log driver. ~**20k log
writes/sec on a near-idle testnet**. This burns CPU (escaping + integer
formatting), drives container-runtime overhead, and is the chain-side analogue
of the prior Loki disk-exhaustion incident. **Action:** drop / sample the
per-`SyncResponse` log line (or move it below `debug`), and reconsider the
Docker logging driver / verbosity.

### 3. The deployed build does **not fsync canonical blocks** — `01`, `03`

Over 60s of block production, the only process issuing `fsync`/`fdatasync` was
`alloy` (the metrics agent); **`qfc-node` issued none**
([`ebpf/01-fsync-latency.txt`](ebpf/01-fsync-latency.txt)). Consistently, the
off-CPU capture ([`ebpf/03-offcpu-qfcnode.txt`](ebpf/03-offcpu-qfcnode.txt))
shows `qfc-node` blocking only on **futex** (lock/parking, ~75s aggregate) and
**tcp_recvmsg** (network, ~15s) — **no `io_schedule`/disk wait**. So in the
current build, block commits drain to disk asynchronously and the producer
never stalls on durability.

This is the **baseline for T3.2** (PR #103, `set_sync(true)` per canonical
block + the durability ADR). Once deployed, expect an `fsync` per block to
appear in capture 1 and an `io_schedule` write-stall to appear in the off-CPU
flame — gated by the EBS write latency in finding 4. The "no write stalls
today" result is exactly why T3.2's RPO=0 change matters, and why it trades a
per-block fsync for durability.

### 4. Disk is EBS: write **service time** (1–8 ms, tail to 64 ms) is the cost, not IOPS — `02`

Block-I/O latency over 60s ([`ebpf/02-bio-latency.txt`](ebpf/02-bio-latency.txt)):
~550 MB written (~9 MB/s aggregate RocksDB traffic across the qfc containers),
write service time clustered at **1–8 ms with a tail to 16–64 ms**. That's
EBS gp3, not local NVMe (which would be tens of µs). Implications for T3:

- Reads that miss the block cache pay full EBS latency, so the **T3.1**
  per-CF block-cache + bloom-filter work (PR #105 — also undeployed) directly
  cuts the most expensive operation. Pair with **T8** hot-key analytics
  (`qfc_hotKeyReport`) to size caches against the actual hot CFs.
- **T3.2**'s per-block fsync will cost one EBS round-trip (~1–8 ms) per block —
  comfortably within a multi-second block interval, confirming the policy is
  affordable here.

## What's deliberately not here

- **A write-stall off-CPU flame graph** (the roadmap's example artifact): there
  were no disk write-stalls to capture in the deployed build — see finding 3.
  Re-run `offcpu-qfcnode.bt` after T3.2 deploys to capture one.
- **Under-stress numbers.** This is steady-state testnet load. A synthetic
  transaction-flood capture (to force memtable flush → L0 → compaction and
  exercise the `rocksdb:low` compaction thread) is the natural follow-up,
  best run on a dedicated node so as not to perturb the validators.

## Rendering the flame graph

The folded stacks are the committed artifact (`ebpf/05-oncpu.folded`, standard
collapsed format). Render to SVG with either tool:

```bash
inferno-flamegraph < docs/profiling/ebpf/05-oncpu.folded > flame.svg
# or
flamegraph.pl       docs/profiling/ebpf/05-oncpu.folded > flame.svg
```

## Reproducing

On a Linux host running the validators (needs `bpftrace`, `perf`, `docker`,
passwordless `sudo`, kernel BTF):

```bash
sudo ./scripts/profiling/capture.sh qfc-node-1 ./t1-ebpf-capture
```

Individual captures are runnable standalone, e.g.
`sudo bpftrace scripts/profiling/fsync-latency.bt`.
