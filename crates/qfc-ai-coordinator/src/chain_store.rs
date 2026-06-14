//! Commitment persistence for training-epoch settlements (ROADMAP-AI-V3
//! milestone A5; design in `docs/adr/0009-chain-integration.md`, Decision 5).
//!
//! [`TrainingChainStore`] is a thin append-only wrapper over a `qfc-storage`
//! [`Database`]. Each closed epoch's [`TrainingEpochCommit`] is stored in the
//! dedicated `TRAINING_COMMITMENTS` column family, keyed `job_id ‖ epoch(BE)`
//! and Borsh-encoded. The BE epoch suffix makes a job's commits scan in epoch
//! order, and the 32-byte job_id prefix isolates jobs.
//!
//! # `create_missing_column_families`: VERIFIED SAFE
//!
//! `qfc-storage`'s `Database::open` sets `create_missing_column_families(true)`
//! (db.rs), so adding `TRAINING_COMMITMENTS` to `cf::ALL` does NOT break
//! opening existing on-disk databases — RocksDB creates the missing CF on the
//! next open. No migration is required (ADR-0009 Decision 5 risk: cleared).
//!
//! # Deferred to A6 / node integration (ADR-0009 Decision 6)
//!
//! Persistence stores the commitment only; it does NOT apply any state. Each
//! is grep-able as `A6`:
//!
//! - **A6: state mutation at the epoch boundary** — crediting the settlement's
//!   rewards to balances and applying its slashes to stake. This store is the
//!   audit anchor, not the applier.
//! - **A6: P2P broadcast** of stored commitments to peers.
//! - **A6: N-of-M operator agreement** selecting the canonical commitment
//!   before it is persisted (this store persists whatever it is given).

use borsh::BorshDeserialize;
use qfc_storage::{cf, Database, StorageConfig, WriteBatch};
use qfc_types::Hash;

use crate::settlement::{SettlementError, TrainingEpochCommit};

/// Append-only persistence for [`TrainingEpochCommit`]s (ADR-0009 Decision 5).
pub struct TrainingChainStore {
    db: Database,
}

impl TrainingChainStore {
    /// Open (or create) a commitment store at `path`. The
    /// `TRAINING_COMMITMENTS` CF is created on open if missing (see module
    /// docs — `create_missing_column_families` is set).
    pub fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, SettlementError> {
        let config = StorageConfig {
            path: path.into(),
            create_if_missing: true,
            ..Default::default()
        };
        let db = Database::open(config).map_err(|e| SettlementError::Storage(e.to_string()))?;
        Ok(Self { db })
    }

    /// Open a temporary in-memory-ish store backed by a temp directory removed
    /// on drop — for tests.
    pub fn open_temp() -> Result<Self, SettlementError> {
        let db = Database::open_temp().map_err(|e| SettlementError::Storage(e.to_string()))?;
        Ok(Self { db })
    }

    /// Key = `job_id (32) ‖ epoch (u64 BE)`.
    fn key(job_id: &Hash, epoch: u64) -> Vec<u8> {
        let mut k = Vec::with_capacity(40);
        k.extend_from_slice(job_id.as_bytes());
        k.extend_from_slice(&epoch.to_be_bytes());
        k
    }

    /// Persist a commitment (append-only; re-putting the same (job, epoch)
    /// overwrites with identical data on an honest path).
    pub fn put_commit(&self, commit: &TrainingEpochCommit) -> Result<(), SettlementError> {
        let key = Self::key(&commit.job_id, commit.epoch);
        let value = borsh::to_vec(commit).map_err(|e| SettlementError::Storage(e.to_string()))?;
        let mut batch = WriteBatch::new();
        batch.put(cf::TRAINING_COMMITMENTS, key, value);
        self.db
            .write_batch(batch)
            .map_err(|e| SettlementError::Storage(e.to_string()))
    }

    /// Fetch one commitment by `(job_id, epoch)`, or `None` if absent.
    pub fn get_commit(
        &self,
        job_id: &Hash,
        epoch: u64,
    ) -> Result<Option<TrainingEpochCommit>, SettlementError> {
        let key = Self::key(job_id, epoch);
        match self
            .db
            .get(cf::TRAINING_COMMITMENTS, &key)
            .map_err(|e| SettlementError::Storage(e.to_string()))?
        {
            Some(bytes) => {
                let commit = TrainingEpochCommit::try_from_slice(&bytes)
                    .map_err(|e| SettlementError::Storage(e.to_string()))?;
                Ok(Some(commit))
            }
            None => Ok(None),
        }
    }

    /// All commitments for one job, in epoch order. Range-scans from the
    /// job_id prefix and stops at the first key with a different prefix.
    pub fn commits_for_job(
        &self,
        job_id: &Hash,
    ) -> Result<Vec<TrainingEpochCommit>, SettlementError> {
        let prefix = job_id.as_bytes();
        // Start the scan at job_id ‖ 0; the BE epoch suffix keeps keys for
        // this job contiguous and epoch-ordered.
        let start = Self::key(job_id, 0);
        let mut out = Vec::new();
        let iter = self
            .db
            .try_iter_from(cf::TRAINING_COMMITMENTS, &start)
            .map_err(|e| SettlementError::Storage(e.to_string()))?;
        for item in iter {
            let (key, value) = item.map_err(|e| SettlementError::Storage(e.to_string()))?;
            // Stop once we leave this job's prefix.
            if key.len() < 32 || &key[..32] != prefix {
                break;
            }
            let commit = TrainingEpochCommit::try_from_slice(&value)
                .map_err(|e| SettlementError::Storage(e.to_string()))?;
            out.push(commit);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(b: u8) -> Hash {
        Hash::new([b; 32])
    }

    fn commit(job: u8, epoch: u64) -> TrainingEpochCommit {
        TrainingEpochCommit {
            job_id: hash(job),
            epoch,
            params_hash: hash(0x55),
            records_root: qfc_crypto::blake3_hash(&[job, epoch as u8]),
            accepted_count: epoch + 1,
            total_flops_accepted: (epoch + 1) * 1000,
        }
    }

    #[test]
    fn test_put_get_round_trip() {
        let store = TrainingChainStore::open_temp().unwrap();
        let c = commit(1, 0);
        store.put_commit(&c).unwrap();
        let got = store.get_commit(&hash(1), 0).unwrap().unwrap();
        assert_eq!(got, c);
    }

    #[test]
    fn test_get_missing_is_none() {
        let store = TrainingChainStore::open_temp().unwrap();
        assert!(store.get_commit(&hash(1), 0).unwrap().is_none());
        // Put one epoch, query a different one.
        store.put_commit(&commit(1, 0)).unwrap();
        assert!(store.get_commit(&hash(1), 1).unwrap().is_none());
    }

    #[test]
    fn test_commits_for_job_sorted_by_epoch() {
        let store = TrainingChainStore::open_temp().unwrap();
        // Insert out of order; expect epoch-sorted output.
        for e in [2u64, 0, 1] {
            store.put_commit(&commit(1, e)).unwrap();
        }
        let commits = store.commits_for_job(&hash(1)).unwrap();
        let epochs: Vec<u64> = commits.iter().map(|c| c.epoch).collect();
        assert_eq!(epochs, vec![0, 1, 2]);
    }

    #[test]
    fn test_multi_job_isolation() {
        let store = TrainingChainStore::open_temp().unwrap();
        store.put_commit(&commit(1, 0)).unwrap();
        store.put_commit(&commit(1, 1)).unwrap();
        store.put_commit(&commit(2, 0)).unwrap();
        store.put_commit(&commit(2, 1)).unwrap();
        store.put_commit(&commit(2, 2)).unwrap();

        let job1 = store.commits_for_job(&hash(1)).unwrap();
        assert_eq!(job1.len(), 2);
        assert!(job1.iter().all(|c| c.job_id == hash(1)));

        let job2 = store.commits_for_job(&hash(2)).unwrap();
        assert_eq!(job2.len(), 3);
        assert!(job2.iter().all(|c| c.job_id == hash(2)));

        // A job with no commits.
        assert!(store.commits_for_job(&hash(9)).unwrap().is_empty());
    }

    /// Reopen persistence: write, drop, reopen at the same path, read back.
    /// Exercises that adding the CF to `cf::ALL` does NOT break reopening an
    /// existing DB (create_missing_column_families is set — see module docs).
    #[test]
    fn test_reopen_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let c = commit(3, 4);
        {
            let store = TrainingChainStore::open(&path).unwrap();
            store.put_commit(&c).unwrap();
            store.db.flush().unwrap();
        } // store dropped, DB closed
        let store = TrainingChainStore::open(&path).unwrap();
        let got = store.get_commit(&hash(3), 4).unwrap().unwrap();
        assert_eq!(got, c);
    }
}
