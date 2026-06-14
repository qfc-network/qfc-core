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

use std::io::Write;
use std::path::Path;

use qfc_storage::{cf, Database, StorageConfig, WriteBatch};
use qfc_types::Hash;
use tracing::debug;

use crate::{types::ParamRange, PsError};

/// Parameters per RocksDB value row. One row = `CHUNK` f32s (4 KiB), keyed by
/// chunk index as 8-byte BE — chunk i covers absolute keys
/// `[i * CHUNK, (i + 1) * CHUNK)`.
pub const CHUNK: u64 = 1024;

/// A1-pragmatic CF choice: `Database::open` hardcodes the chain's CF set
/// (`cf::ALL`) with no custom-CF support, so we reuse `cf::STATE` purely as a
/// namespace for parameter chunk rows. This is fine per ADR-0002/0006 because
/// a `ShardStore` is a *dedicated* DB instance at its own path — the chain DB
/// never sees these rows. A later milestone can teach `qfc-storage` custom CF
/// lists if the unused chain CFs become an annoyance.
const PARAMS_CF: &str = cf::STATE;

fn storage_err(e: impl std::fmt::Display) -> PsError {
    PsError::Storage(e.to_string())
}

fn encode_chunk(chunk: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len() * 4);
    for v in chunk {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_chunk(bytes: &[u8]) -> Result<Vec<f32>, PsError> {
    if bytes.len() != (CHUNK as usize) * 4 {
        return Err(PsError::Storage(format!(
            "corrupt chunk row: {} bytes, expected {}",
            bytes.len(),
            CHUNK * 4
        )));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().expect("chunks_exact(4) yields 4-byte slices")))
        .collect())
}

/// Range-sharded parameter store: one shard's contiguous `owned_range` of
/// dense model parameters, persisted in a dedicated RocksDB instance.
pub struct ShardStore {
    db: Database,
    owned_range: ParamRange,
}

impl ShardStore {
    /// Open (or create) the shard's dedicated database at `path`.
    pub fn open(path: impl AsRef<Path>, owned_range: ParamRange) -> Result<Self, PsError> {
        let config = StorageConfig {
            path: path.as_ref().to_path_buf(),
            create_if_missing: true,
            ..Default::default()
        };
        let db = Database::open(config).map_err(storage_err)?;
        debug!(path = %path.as_ref().display(), range = %owned_range, "opened shard store");
        Ok(Self { db, owned_range })
    }

    /// Open a throwaway store for tests; the backing directory is removed
    /// when the store drops.
    pub fn open_temp(owned_range: ParamRange) -> Result<Self, PsError> {
        let db = Database::open_temp().map_err(storage_err)?;
        Ok(Self { db, owned_range })
    }

    /// The contiguous parameter range this shard owns.
    pub fn owned_range(&self) -> ParamRange {
        self.owned_range
    }

    fn check_owned(&self, range: &ParamRange) -> Result<(), PsError> {
        if !self.owned_range.covers(range) {
            return Err(PsError::InvalidRange(format!(
                "range {} outside owned range {}",
                range, self.owned_range
            )));
        }
        Ok(())
    }

    fn chunk_key(idx: u64) -> [u8; 8] {
        idx.to_be_bytes()
    }

    /// Load chunk `idx` as a full `CHUNK`-wide vector, zero-filled if the row
    /// was never written.
    fn load_chunk(&self, idx: u64) -> Result<Vec<f32>, PsError> {
        match self
            .db
            .get(PARAMS_CF, &Self::chunk_key(idx))
            .map_err(storage_err)?
        {
            Some(bytes) => decode_chunk(&bytes),
            None => Ok(vec![0.0; CHUNK as usize]),
        }
    }

    /// Read-modify-write every chunk overlapping `range`, applying
    /// `f(stored_slot, incoming_value)` over the intersection, then commit all
    /// rows in one atomic `WriteBatch`.
    fn modify_range(
        &self,
        range: &ParamRange,
        values: &[f32],
        f: impl Fn(&mut f32, f32),
    ) -> Result<(), PsError> {
        self.check_owned(range)?;
        if values.len() as u64 != range.len() {
            return Err(PsError::InvalidUpdate(format!(
                "value count {} != range length {} for {}",
                values.len(),
                range.len(),
                range
            )));
        }
        let first = range.start / CHUNK;
        let last = (range.end - 1) / CHUNK;
        let mut batch = WriteBatch::with_capacity((last - first + 1) as usize);
        for idx in first..=last {
            let chunk_start = idx * CHUNK;
            let lo = range.start.max(chunk_start);
            let hi = range.end.min(chunk_start.saturating_add(CHUNK));
            let mut chunk = self.load_chunk(idx)?;
            let dst = &mut chunk[(lo - chunk_start) as usize..(hi - chunk_start) as usize];
            let src = &values[(lo - range.start) as usize..(hi - range.start) as usize];
            for (slot, v) in dst.iter_mut().zip(src) {
                f(slot, *v);
            }
            batch.put(
                PARAMS_CF,
                Self::chunk_key(idx).to_vec(),
                encode_chunk(&chunk),
            );
        }
        self.db.write_batch(batch).map_err(storage_err)
    }

    /// Current parameters for `range` (must lie within the owned range).
    /// Never-written keys read as 0.0.
    pub fn get_range(&self, range: ParamRange) -> Result<Vec<f32>, PsError> {
        self.check_owned(&range)?;
        let mut out = vec![0.0f32; range.len() as usize];
        let first = range.start / CHUNK;
        let last = (range.end - 1) / CHUNK;
        for idx in first..=last {
            if let Some(bytes) = self
                .db
                .get(PARAMS_CF, &Self::chunk_key(idx))
                .map_err(storage_err)?
            {
                let chunk = decode_chunk(&bytes)?;
                let chunk_start = idx * CHUNK;
                let lo = range.start.max(chunk_start);
                let hi = range.end.min(chunk_start.saturating_add(CHUNK));
                out[(lo - range.start) as usize..(hi - range.start) as usize].copy_from_slice(
                    &chunk[(lo - chunk_start) as usize..(hi - chunk_start) as usize],
                );
            }
        }
        Ok(out)
    }

    /// Overwrite `range` with `values` (initial load of a model version).
    /// Atomic across all affected chunk rows.
    pub fn set_range(&self, range: ParamRange, values: &[f32]) -> Result<(), PsError> {
        self.modify_range(&range, values, |slot, v| *slot = v)
    }

    /// Add `delta` element-wise onto `range`. Atomic: all affected chunks are
    /// read-modified-written in a single `WriteBatch` (the epoch barrier
    /// applies the aggregated delta through this).
    pub fn apply_delta(&self, range: ParamRange, delta: &[f32]) -> Result<(), PsError> {
        self.modify_range(&range, delta, |slot, d| *slot += d)
    }

    /// Stream the owned range's raw f32-LE bytes, chunk by chunk in key
    /// order (zero-filled gaps included), into `sink`. This byte sequence is
    /// the canonical shard image: both `params_hash` and `export_snapshot`
    /// are defined over it.
    fn stream_owned(
        &self,
        mut sink: impl FnMut(&[u8]) -> Result<(), PsError>,
    ) -> Result<(), PsError> {
        let range = self.owned_range;
        let first = range.start / CHUNK;
        let last = (range.end - 1) / CHUNK;
        for idx in first..=last {
            let chunk = self.load_chunk(idx)?;
            let chunk_start = idx * CHUNK;
            let lo = range.start.max(chunk_start);
            let hi = range.end.min(chunk_start.saturating_add(CHUNK));
            let bytes =
                encode_chunk(&chunk[(lo - chunk_start) as usize..(hi - chunk_start) as usize]);
            sink(&bytes)?;
        }
        Ok(())
    }

    /// Streaming Blake3 over the owned range's f32-LE bytes in key order —
    /// the shard's contribution to the epoch version commit (ADR-0006).
    /// Deterministic: unwritten keys hash as 0.0.
    pub fn params_hash(&self) -> Result<Hash, PsError> {
        let mut hasher = blake3::Hasher::new();
        self.stream_owned(|bytes| {
            hasher.update(bytes);
            Ok(())
        })?;
        Ok(Hash::new(*hasher.finalize().as_bytes()))
    }

    /// Raw f32-LE dump of the owned range to `path` — byte-identical to the
    /// `params_hash` input, and the input to B-1 `ShardManifest::split_file`
    /// (ADR-0007).
    pub fn export_snapshot(&self, path: impl AsRef<Path>) -> Result<(), PsError> {
        let file = std::fs::File::create(path.as_ref()).map_err(storage_err)?;
        let mut writer = std::io::BufWriter::new(file);
        self.stream_owned(|bytes| writer.write_all(bytes).map_err(storage_err))?;
        writer.flush().map_err(storage_err)?;
        debug!(path = %path.as_ref().display(), range = %self.owned_range, "exported snapshot");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> ParamRange {
        ParamRange::new(start, end).unwrap()
    }

    /// Pattern values distinct per key so cross-chunk copies are detectable.
    fn pattern(r: ParamRange) -> Vec<f32> {
        (r.start..r.end).map(|k| k as f32 * 0.5 - 7.0).collect()
    }

    #[test]
    fn test_set_get_round_trip_partial_chunks() {
        // Owned range starts and ends mid-chunk and spans 3 chunk rows.
        let owned = range(1000, 3100);
        let store = ShardStore::open_temp(owned).unwrap();
        store.set_range(owned, &pattern(owned)).unwrap();

        assert_eq!(store.get_range(owned).unwrap(), pattern(owned));

        // Sub-range crossing the 1024 and 2048 chunk boundaries.
        let sub = range(1020, 2060);
        assert_eq!(store.get_range(sub).unwrap(), pattern(sub));

        // Single-key edges.
        assert_eq!(
            store.get_range(range(1000, 1001)).unwrap(),
            pattern(range(1000, 1001))
        );
        assert_eq!(
            store.get_range(range(3099, 3100)).unwrap(),
            pattern(range(3099, 3100))
        );
    }

    #[test]
    fn test_zero_fill_for_unwritten() {
        let owned = range(1000, 3100);
        let store = ShardStore::open_temp(owned).unwrap();

        // Nothing written: everything reads as zero.
        assert_eq!(
            store.get_range(owned).unwrap(),
            vec![0.0; owned.len() as usize]
        );

        // Write only a middle slice; the flanks stay zero.
        let mid = range(2000, 2100);
        store.set_range(mid, &vec![1.5; 100]).unwrap();
        let all = store.get_range(owned).unwrap();
        assert!(all[..1000].iter().all(|&v| v == 0.0));
        assert!(all[1000..1100].iter().all(|&v| v == 1.5));
        assert!(all[1100..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn test_out_of_owned_range_rejected() {
        let owned = range(1000, 2000);
        let store = ShardStore::open_temp(owned).unwrap();

        for bad in [
            range(999, 1001),
            range(1999, 2001),
            range(0, 10),
            range(5000, 5010),
        ] {
            assert!(matches!(
                store.get_range(bad),
                Err(PsError::InvalidRange(_))
            ));
            assert!(matches!(
                store.set_range(bad, &vec![0.0; bad.len() as usize]),
                Err(PsError::InvalidRange(_))
            ));
            assert!(matches!(
                store.apply_delta(bad, &vec![0.0; bad.len() as usize]),
                Err(PsError::InvalidRange(_))
            ));
        }

        // Length mismatch is rejected before any write.
        assert!(matches!(
            store.set_range(range(1000, 1004), &[1.0; 3]),
            Err(PsError::InvalidUpdate(_))
        ));
    }

    #[test]
    fn test_apply_delta_including_negative() {
        let owned = range(0, 2500);
        let store = ShardStore::open_temp(owned).unwrap();
        store.set_range(owned, &vec![10.0; 2500]).unwrap();

        // Delta spans a chunk boundary; mixes positive and negative.
        let dr = range(1000, 1100);
        let delta: Vec<f32> = (0..100)
            .map(|i| if i % 2 == 0 { 2.5 } else { -4.0 })
            .collect();
        store.apply_delta(dr, &delta).unwrap();

        let got = store.get_range(dr).unwrap();
        for (i, v) in got.iter().enumerate() {
            let expect = if i % 2 == 0 { 12.5 } else { 6.0 };
            assert_eq!(*v, expect, "index {}", i);
        }
        // Delta on never-written keys starts from the zero fill.
        let dr2 = range(2400, 2500);
        store.apply_delta(dr2, &vec![-1.0; 100]).unwrap();
        assert_eq!(store.get_range(range(2400, 2401)).unwrap(), vec![9.0]);
    }

    #[test]
    fn test_params_hash_sensitivity_and_reopen_stability() {
        let owned = range(1000, 3100);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shard-db");

        let h1 = {
            let store = ShardStore::open(&path, owned).unwrap();
            store.set_range(owned, &pattern(owned)).unwrap();
            store.params_hash().unwrap()
        };

        // Reopen from disk: identical hash.
        let store = ShardStore::open(&path, owned).unwrap();
        assert_eq!(store.params_hash().unwrap(), h1);

        // Changing one parameter changes the hash.
        store.set_range(range(2048, 2049), &[123.456]).unwrap();
        let h2 = store.params_hash().unwrap();
        assert_ne!(h1, h2);

        // Hash of pristine (all-zero) shard differs from written shard.
        let empty = ShardStore::open_temp(owned).unwrap();
        assert_ne!(empty.params_hash().unwrap(), h1);
    }

    #[test]
    fn test_export_snapshot_matches_get_range_bytes() {
        let owned = range(1000, 3100);
        let store = ShardStore::open_temp(owned).unwrap();
        // Leave a zero gap in the middle on purpose.
        store
            .set_range(range(1000, 1500), &pattern(range(1000, 1500)))
            .unwrap();
        store
            .set_range(range(2500, 3100), &pattern(range(2500, 3100)))
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("snapshot.f32le");
        store.export_snapshot(&snap).unwrap();

        let file_bytes = std::fs::read(&snap).unwrap();
        let expect: Vec<u8> = store
            .get_range(owned)
            .unwrap()
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(file_bytes, expect);
        assert_eq!(file_bytes.len() as u64, owned.len() * 4);

        // Snapshot bytes are exactly the params_hash preimage.
        assert_eq!(
            store.params_hash().unwrap(),
            Hash::new(*blake3::hash(&file_bytes).as_bytes())
        );
    }
}
