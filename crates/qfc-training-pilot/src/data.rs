//! Synthetic training data + a real-but-local `ShardManifest` (ADR-0010
//! Decision 3).
//!
//! Training data is synthetic and resolved from an in-process
//! `shard_index → rows` map ([`DataStore`]). The `TrainingJobSpec` still
//! carries a VALID [`ShardManifest`] (non-empty alphanumeric CIDs,
//! `shards.len() >= min_updates`) so it passes A3 validation — but the
//! executor reads rows from the local store by `data_shard_indices`, not from
//! IPFS.
//!
//! ## Why `ShardManifest::split_file` can't be used here
//!
//! `split_file` (the B-1 pre-upload path) leaves every `ShardEntry::cid`
//! EMPTY — that is the legal "not yet published to IPFS" state. But
//! `TrainingJobSpec::validate` explicitly REJECTS any shard with an empty cid
//! ("training jobs require uploaded data"). So a `split_file` manifest can
//! never back a training job; the pilot hand-constructs a manifest with
//! non-empty alphanumeric CIDs instead. (Surfacing this gap is itself an
//! ADR-0010 D3 finding.)

use qfc_crypto::blake3_hash;
use qfc_inference::{ShardEntry, ShardManifest};
use qfc_types::Hash;

use crate::model::Dataset;

/// In-process analogue of a `ShardStore`: synthetic training data split into
/// `S` shards, indexable by `u32` shard index. The executor and honest
/// workers both read rows from here by their assigned `data_shard_indices`.
#[derive(Clone, Debug)]
pub struct DataStore {
    /// One [`Dataset`] per shard index.
    pub shards: Vec<Dataset>,
    /// Feature/parameter dimension every row carries (last feature is the
    /// constant `1.0` bias).
    pub dim: usize,
}

impl DataStore {
    /// Resolve a set of shard indices into one concatenated dataset, in the
    /// given index order (the executor and workers MUST agree on order —
    /// gradient summation order is load-bearing for determinism).
    pub fn assemble(&self, indices: &[u32]) -> Dataset {
        let mut out = Dataset::new();
        for &idx in indices {
            if let Some(shard) = self.shards.get(idx as usize) {
                out.extend(shard);
            }
        }
        out
    }

    /// The full dataset across all shards (for reporting global loss).
    pub fn full(&self) -> Dataset {
        let mut out = Dataset::new();
        for shard in &self.shards {
            out.extend(shard);
        }
        out
    }
}

/// True linear weights the synthetic data is generated from (length `dim`,
/// last entry is the bias coefficient). Deterministic for a given `dim`.
pub fn true_weights(dim: usize) -> Vec<f32> {
    // Small, varied, deterministic coefficients; bias = 1.0.
    let mut w = Vec::with_capacity(dim);
    for j in 0..dim {
        // Alternating-sign ramp keeps coefficients O(1).
        let v = 0.5 + 0.25 * (j as f32);
        w.push(if j % 2 == 0 { v } else { -v });
    }
    if dim > 0 {
        w[dim - 1] = 1.0; // bias coefficient
    }
    w
}

/// Generate a synthetic linear dataset `y = w·x (+ small deterministic noise)`
/// split into `num_shards` shards of `rows_per_shard` rows each. The last
/// feature of every row is the constant `1.0` (bias). Fully deterministic
/// given `(dim, num_shards, rows_per_shard, noise)`.
pub fn synth_datastore(
    dim: usize,
    num_shards: usize,
    rows_per_shard: usize,
    noise: f32,
) -> DataStore {
    let w = true_weights(dim);
    let mut shards = Vec::with_capacity(num_shards);
    let mut counter: u64 = 0;
    for _ in 0..num_shards {
        let mut rows = Vec::with_capacity(rows_per_shard);
        for _ in 0..rows_per_shard {
            counter += 1;
            // Deterministic pseudo-random features in a small range.
            let mut x = Vec::with_capacity(dim);
            for j in 0..dim {
                if j == dim - 1 {
                    x.push(1.0); // bias feature
                } else {
                    // Deterministic spread of feature values in [-1, 1).
                    let h = blake3_hash(&[(counter as u8), (j as u8), 0x5A]);
                    let b = u32::from_be_bytes(h.as_bytes()[..4].try_into().unwrap());
                    let f = (b as f32 / u32::MAX as f32) * 2.0 - 1.0;
                    x.push(f);
                }
            }
            let mut y = 0.0f32;
            for j in 0..dim {
                y += w[j] * x[j];
            }
            if noise != 0.0 {
                let h = blake3_hash(&[(counter as u8), 0xEE, 0x11]);
                let b = u32::from_be_bytes(h.as_bytes()[..4].try_into().unwrap());
                let n = (b as f32 / u32::MAX as f32) * 2.0 - 1.0;
                y += n * noise;
            }
            rows.push((x, y));
        }
        shards.push(Dataset { rows });
    }
    DataStore { shards, dim }
}

/// Hand-construct a VALID [`ShardManifest`] for `num_shards` shards of the
/// given store — non-empty alphanumeric CIDs, a real blake3 hash of each
/// shard's serialized bytes, correct `size_bytes`, no `layer_range`, and a
/// consistent `total_size_bytes`. Passes BOTH `ShardManifest::validate` AND
/// `TrainingJobSpec::validate` (non-empty CIDs; `shards.len()` must be
/// `>= TrimmedMean::min_updates()`, which the caller ensures via `num_shards`).
///
/// We serialize each shard's rows to a deterministic byte image purely to give
/// the manifest a real content hash and size — the executor never fetches by
/// CID, it reads rows from the local [`DataStore`].
pub fn build_manifest(store: &DataStore) -> ShardManifest {
    let mut shards = Vec::with_capacity(store.shards.len());
    let mut total: u64 = 0;
    for (i, ds) in store.shards.iter().enumerate() {
        let bytes = serialize_shard(ds);
        let size = bytes.len() as u64;
        total = total.saturating_add(size);
        shards.push(ShardEntry {
            // Non-empty, purely alphanumeric — passes ShardManifest::validate's
            // cid charset check AND TrainingJobSpec's non-empty-cid rule.
            cid: format!("shard{i}"),
            hash: blake3_hash(&bytes),
            size_bytes: size,
            layer_range: None,
        });
    }
    ShardManifest {
        shards,
        total_size_bytes: total,
        // Hash of the concatenated shard images — consistent but unused by the
        // pilot executor (no assembly happens). ShardManifest::validate does
        // not cross-check this, but we keep it honest.
        assembled_hash: assembled_hash(store),
    }
}

/// Deterministic byte image of one shard (f32-LE features then label per row).
fn serialize_shard(ds: &Dataset) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (x, y) in &ds.rows {
        for f in x {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        bytes.extend_from_slice(&y.to_le_bytes());
    }
    // Ensure non-empty so size_bytes > 0 even for an (impossible) empty shard.
    if bytes.is_empty() {
        bytes.push(0);
    }
    bytes
}

fn assembled_hash(store: &DataStore) -> Hash {
    let mut all = Vec::new();
    for ds in &store.shards {
        all.extend(serialize_shard(ds));
    }
    blake3_hash(&all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_passes_shard_validate() {
        let store = synth_datastore(4, 5, 10, 0.0);
        let m = build_manifest(&store);
        assert!(m.validate().is_ok(), "manifest must pass B-1 validation");
        assert_eq!(m.shards.len(), 5);
        // All cids non-empty + alphanumeric.
        for s in &m.shards {
            assert!(!s.cid.is_empty());
            assert!(s.cid.bytes().all(|b| b.is_ascii_alphanumeric()));
            assert!(s.size_bytes > 0);
        }
        let sum: u64 = m.shards.iter().map(|s| s.size_bytes).sum();
        assert_eq!(sum, m.total_size_bytes);
    }

    #[test]
    fn assemble_concatenates_in_order() {
        let store = synth_datastore(3, 2, 4, 0.0);
        let d01 = store.assemble(&[0, 1]);
        assert_eq!(d01.len(), store.shards[0].len() + store.shards[1].len());
        // order matters
        let d10 = store.assemble(&[1, 0]);
        assert_ne!(d01.rows[0].1, d10.rows[0].1);
    }

    #[test]
    fn datastore_is_deterministic() {
        let a = synth_datastore(4, 3, 8, 0.01);
        let b = synth_datastore(4, 3, 8, 0.01);
        for (sa, sb) in a.shards.iter().zip(&b.shards) {
            assert_eq!(sa.rows, sb.rows);
        }
    }
}
