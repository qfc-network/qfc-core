# ADR-0007: Data availability & disaster recovery for training

**Status:** accepted · **Date:** 2026-06-12 · **Context:** Feature A review finding 1 (replay-stream availability was unowned).

## Problem

The recovery story "snapshot per epoch + replay the sample stream" (Monolith's
RPO model) silently assumes someone durably stores the sample stream and the
snapshots. In a datacenter that's Kafka + HDFS; in a decentralized network it
must be an owned, incentivized duty or it does not exist.

## Decision

Three artifacts, three owners:

| Artifact | Producer | Pinned by | Lifetime |
|---|---|---|---|
| **Data-shard manifests** (training data, sharded, CID'd — B-1 manifest format) | job submitter | submitter pays pinning deposit at job creation; assigned miners pin their shards for the job duration | job + audit window |
| **Epoch parameter snapshots** (full params, shard-manifest format) | PS operators at the epoch barrier | the ≥2 operators of each range (staked duty, ADR-0002); CID in the on-chain commit | last `K=4` epochs + every model-version release |
| **Accepted-update log** (per-epoch `(worker, range, step, update_hash, flops)` records, ADR-0004) | PS operators | operators; record-set hash is in the on-chain commit, so withholding is detectable | audit window |

**RPO = one training epoch.** Operator failure mid-epoch loses at most the
in-flight epoch's updates; recovery = load snapshot N−1, re-run the epoch.
This is the deliberate, Monolith-style "RPO is a business decision": training
re-learns a lost epoch faster than any sub-epoch durability machinery would
pay for itself.

A job whose data shards become unavailable mid-job (submitter unpinned,
miners churned) is **failed-safe**: epoch can't start without quorum
availability attestation from assigned miners; the job suspends rather than
training on partial data.

## Consequences

- `qfc-ps` snapshot format = B-1 `ShardManifest` (split + per-shard Blake3 +
  IPFS), so snapshots are resumable/verifiable downloads for free.
- Slashable pinning duty needs a cheap availability challenge (operator must
  serve a random byte-range of a pinned CID within a deadline) — lands with
  A5; out of scope for A1/A2.
