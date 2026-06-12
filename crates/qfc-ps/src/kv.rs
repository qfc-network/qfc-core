//! Range-sharded parameter storage on `qfc_storage::Database`.
//!
//! Implemented in milestone A1. Required API (service.rs builds on this):
//!
//! - `ShardStore::open(path, owned_range) -> Result<Self, PsError>` and
//!   `ShardStore::open_temp(owned_range)` for tests — a dedicated RocksDB
//!   instance (NOT the chain DB), reusing `qfc_storage::Database`.
//! - Keys: parameter index as 8-byte big-endian (matches the workspace's
//!   BE-ordered key convention; gives natural range iteration). Values: f32 LE.
//!   Stored row-batched per a fixed chunk width to avoid one-KV-per-f32.
//! - `get_range(&self, range) -> Result<Vec<f32>, PsError>` — zero-filled for
//!   never-written keys inside the owned range; error outside it.
//! - `apply_delta(&self, range, delta: &[f32]) -> Result<(), PsError>` —
//!   read-modify-write, atomic via `WriteBatch` (epoch barrier applies the
//!   aggregated delta).
//! - `set_range(&self, range, values)` — initial load of a model version.
//! - `params_hash(&self) -> Result<qfc_types::Hash, PsError>` — streaming
//!   Blake3 over the owned range in key order (the shard's contribution to
//!   the epoch version commit, ADR-0006).
//! - `export_snapshot(&self, path) -> Result<(), PsError>` — raw f32-LE dump
//!   of the owned range, the input to B-1 `ShardManifest::split_file`
//!   (ADR-0007: snapshots reuse the shard-manifest format).

#[allow(unused_imports)]
use crate::{types::ParamRange, PsError};
