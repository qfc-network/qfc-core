//! Live-database snapshots via RocksDB `Checkpoint` (T4.2, DR automation).
//!
//! A *snapshot* here is RocksDB's **file-level** checkpoint: a new directory
//! containing hard links to the live database's immutable SST files plus
//! copies of the small mutable files (MANIFEST/CURRENT/OPTIONS). Creating one
//! is near-instant and does not stop the world; the result is a complete,
//! point-in-time-consistent database that can be opened directly with
//! [`Database::open`].
//!
//! Naming note: this is deliberately called *snapshot*, not *checkpoint*, to
//! avoid confusion with the consensus-level `ValidatorCheckpoint` data stored
//! in the `checkpoints` column family (see `cf::CHECKPOINTS`), which is an
//! entirely different concept owned by qfc-consensus (T4.1).
//!
//! Each snapshot directory carries a JSON manifest
//! ([`MANIFEST_FILE_NAME`] = `qfc-snapshot-manifest.json`) describing what was
//! captured; the T4.3 restore tooling consumes it to verify a snapshot before
//! placing it.

use crate::db::Database;
use crate::error::{Result, StorageError};
use crate::schema::{cf, meta};
use rocksdb::checkpoint::Checkpoint;
use serde::{Deserialize, Serialize};
use std::path::Path;
use tracing::info;

/// Version of the snapshot directory layout + manifest schema.
///
/// Bump when the manifest fields or the on-disk layout change; restore
/// tooling must refuse versions it does not understand.
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// File name of the JSON manifest written inside every snapshot directory.
///
/// RocksDB ignores unknown files in a database directory, so the manifest can
/// live next to the SSTs without affecting `Database::open`.
pub const MANIFEST_FILE_NAME: &str = "qfc-snapshot-manifest.json";

/// JSON manifest written into each snapshot directory.
///
/// Consumed by the T4.3 restore script — treat this as a stable format and
/// bump [`SNAPSHOT_FORMAT_VERSION`] on any change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// Snapshot format/manifest schema version ([`SNAPSHOT_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Unix timestamp (seconds) at which the snapshot was taken, supplied by
    /// the caller so the backup task and metrics agree on one clock reading.
    pub created_at_unix: u64,
    /// Chain head height (`latest_block_number` metadata) at snapshot time.
    /// `None` for a database that has not committed a head yet.
    pub block_height: Option<u64>,
    /// On-disk schema version of the captured database (`db_version`
    /// metadata; see [`crate::DB_VERSION`]).
    pub db_version: u32,
    /// Column families present in the captured database.
    pub column_families: Vec<String>,
}

impl SnapshotManifest {
    /// Read and parse the manifest from a snapshot directory.
    pub fn read_from_dir(dir: &Path) -> Result<Self> {
        let raw = std::fs::read(dir.join(MANIFEST_FILE_NAME))?;
        serde_json::from_slice(&raw)
            .map_err(|e| StorageError::Deserialization(format!("snapshot manifest: {e}")))
    }
}

impl Database {
    /// Create a consistent file-level snapshot of the live database at `dir`.
    ///
    /// Uses RocksDB's `Checkpoint` API: hard-links immutable SST files into
    /// `dir` (near-instant, no stop-the-world; memtables are flushed first so
    /// the snapshot needs no WAL replay). `dir` must not exist yet and must
    /// be on the **same filesystem** as the database for hard links to work
    /// (RocksDB silently falls back to copying across filesystems).
    ///
    /// `created_at_unix` is recorded verbatim in the manifest; pass the
    /// caller's clock reading so backup metrics and the manifest agree.
    ///
    /// The resulting directory is a complete database, openable with
    /// [`Database::open`], plus a [`SnapshotManifest`] JSON file
    /// ([`MANIFEST_FILE_NAME`]).
    pub fn create_snapshot(&self, dir: &Path, created_at_unix: u64) -> Result<SnapshotManifest> {
        if dir.exists() {
            return Err(StorageError::InvalidData(format!(
                "snapshot directory already exists: {}",
                dir.display()
            )));
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Capture metadata *before* the checkpoint: the height recorded in
        // the manifest is then guaranteed to be <= the height captured in the
        // snapshot files (RocksDB checkpoints include all writes up to
        // creation time), which is the safe direction for restore validation.
        let block_height = self
            .get(cf::METADATA, meta::LATEST_BLOCK_NUMBER)?
            .and_then(|b| <[u8; 8]>::try_from(b.as_slice()).ok())
            .map(u64::from_le_bytes);
        let db_version = self
            .get(cf::METADATA, meta::DB_VERSION)?
            .and_then(|b| <[u8; 4]>::try_from(b.as_slice()).ok())
            .map(u32::from_le_bytes)
            .unwrap_or(crate::schema::DB_VERSION);

        let checkpoint = Checkpoint::new(self.rocksdb())?;
        checkpoint.create_checkpoint(dir)?;

        let manifest = SnapshotManifest {
            format_version: SNAPSHOT_FORMAT_VERSION,
            created_at_unix,
            block_height,
            db_version,
            column_families: cf::ALL.iter().map(|s| s.to_string()).collect(),
        };

        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| StorageError::Serialization(format!("snapshot manifest: {e}")))?;
        std::fs::write(dir.join(MANIFEST_FILE_NAME), manifest_json)?;

        info!(
            "Created database snapshot at {} (height: {:?})",
            dir.display(),
            block_height
        );
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::StorageConfig;

    /// Open a database in `<tempdir>/db` so the snapshot can live in a
    /// sibling path on the same filesystem.
    fn open_db(dir: &Path) -> Database {
        Database::open(StorageConfig {
            path: dir.join("db"),
            create_if_missing: true,
            ..Default::default()
        })
        .unwrap()
    }

    fn open_snapshot(path: &Path) -> Database {
        Database::open(StorageConfig {
            path: path.to_path_buf(),
            create_if_missing: false,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn snapshot_of_live_db_opens_read_consistently() {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_db(tmp.path());

        // Simulate chain state across several CFs, including head metadata.
        db.put(cf::BLOCK_HEADERS, &42u64.to_be_bytes(), b"header-42")
            .unwrap();
        db.put(cf::STATE, b"trie-node-a", b"node-bytes").unwrap();
        db.put(
            cf::METADATA,
            meta::LATEST_BLOCK_NUMBER,
            &42u64.to_le_bytes(),
        )
        .unwrap();

        let snap_dir = tmp.path().join("snap");
        let manifest = db.create_snapshot(&snap_dir, 1_765_000_000).unwrap();

        assert_eq!(manifest.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(manifest.created_at_unix, 1_765_000_000);
        assert_eq!(manifest.block_height, Some(42));
        assert_eq!(manifest.db_version, crate::schema::DB_VERSION);
        assert_eq!(manifest.column_families.len(), cf::ALL.len());

        // The manifest on disk round-trips.
        let on_disk = SnapshotManifest::read_from_dir(&snap_dir).unwrap();
        assert_eq!(on_disk, manifest);

        // The snapshot opens as a regular database with all data visible —
        // while the source DB is still open and live.
        let snap = open_snapshot(&snap_dir);
        assert_eq!(
            snap.get(cf::BLOCK_HEADERS, &42u64.to_be_bytes()).unwrap(),
            Some(b"header-42".to_vec())
        );
        assert_eq!(
            snap.get(cf::STATE, b"trie-node-a").unwrap(),
            Some(b"node-bytes".to_vec())
        );
        assert_eq!(
            snap.get(cf::METADATA, meta::LATEST_BLOCK_NUMBER).unwrap(),
            Some(42u64.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn snapshot_survives_source_db_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_db(tmp.path());

        db.put(cf::STATE, b"key", b"original").unwrap();
        db.put(cf::STATE, b"doomed", b"to-be-deleted").unwrap();

        let snap_dir = tmp.path().join("snap");
        db.create_snapshot(&snap_dir, 1).unwrap();

        // Mutate the source aggressively after the snapshot: overwrite,
        // delete, flush memtables, and force a full compaction (which
        // rewrites/unlinks SST files — the hard links must keep the old
        // blocks alive for the snapshot).
        db.put(cf::STATE, b"key", b"mutated").unwrap();
        db.delete(cf::STATE, b"doomed").unwrap();
        db.put(cf::STATE, b"new-key", b"new-value").unwrap();
        db.flush().unwrap();
        db.compact().unwrap();

        let snap = open_snapshot(&snap_dir);
        assert_eq!(
            snap.get(cf::STATE, b"key").unwrap(),
            Some(b"original".to_vec()),
            "snapshot must not see post-snapshot overwrites"
        );
        assert_eq!(
            snap.get(cf::STATE, b"doomed").unwrap(),
            Some(b"to-be-deleted".to_vec()),
            "snapshot must not see post-snapshot deletes"
        );
        assert_eq!(
            snap.get(cf::STATE, b"new-key").unwrap(),
            None,
            "snapshot must not see post-snapshot inserts"
        );

        // And the source still sees its own mutations.
        assert_eq!(
            db.get(cf::STATE, b"key").unwrap(),
            Some(b"mutated".to_vec())
        );
    }

    #[test]
    fn snapshot_rejects_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_db(tmp.path());

        let snap_dir = tmp.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        assert!(matches!(
            db.create_snapshot(&snap_dir, 0),
            Err(StorageError::InvalidData(_))
        ));
    }

    #[test]
    fn snapshot_of_empty_db_has_no_height() {
        let tmp = tempfile::tempdir().unwrap();
        let db = open_db(tmp.path());

        let snap_dir = tmp.path().join("snap");
        let manifest = db.create_snapshot(&snap_dir, 7).unwrap();
        assert_eq!(manifest.block_height, None);
        // Still a valid, openable database.
        let snap = open_snapshot(&snap_dir);
        assert!(snap.get(cf::METADATA, meta::DB_VERSION).unwrap().is_some());
    }
}
