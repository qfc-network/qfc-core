//! T4.2: periodic live-database snapshot backups shipped to object storage.
//!
//! Pipeline, per interval tick:
//!
//! 1. **Snapshot** — `Database::create_snapshot` (RocksDB file-level
//!    checkpoint: hard links, near-instant, consistent, no stop-the-world).
//! 2. **Archive** — pack the snapshot directory into
//!    `<prefix>-<height>-<unix_ts>.tar.gz` (the snapshot dir itself is
//!    deleted after packing; tar+gzip via the `tar`/`flate2` crates already
//!    in-tree, no heavy compression deps).
//! 3. **Ship** — run a user-supplied upload command
//!    (`--snapshot-upload-cmd`, `{file}` substituted with the archive path)
//!    so any object store works without bundling an S3 SDK. Examples:
//!    `rclone copy {file} remote:qfc-backups/`,
//!    `aws s3 cp {file} s3://qfc-backups/`,
//!    `mc cp {file} minio/qfc-backups/`.
//! 4. **Retain** — keep the newest `--snapshot-retain` local archives, prune
//!    the rest.
//!
//! Failures never crash the node: they are logged, counted in
//! [`SnapshotBackupMetrics::failures_total`], and surfaced via the
//! backup-*freshness* metrics (`qfc_snapshot_last_success_timestamp_seconds`)
//! that the alert rules watch — alert on AGE, not job success.
//!
//! Naming note: "snapshot" = RocksDB file-level checkpoint of the whole DB.
//! Not to be confused with consensus `ValidatorCheckpoint`s (T4.1).

use flate2::write::GzEncoder;
use flate2::Compression;
use qfc_storage::Database;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Configuration for the periodic snapshot backup task.
#[derive(Clone, Debug)]
pub struct SnapshotBackupConfig {
    /// Interval between snapshots (`--snapshot-interval-secs`).
    pub interval: Duration,
    /// Directory where snapshot archives are written
    /// (`--snapshot-dir`, default `<datadir>/snapshots`).
    pub dir: PathBuf,
    /// Archive name prefix (`--snapshot-prefix`, default `qfc-snapshot`).
    pub prefix: String,
    /// Optional shell command to ship the archive to object storage
    /// (`--snapshot-upload-cmd`). `{file}` is replaced with the archive path
    /// (shell-quoted); if no `{file}` placeholder is present, the path is
    /// appended as the last argument. Run via `sh -c`.
    pub upload_cmd: Option<String>,
    /// How many local archives to keep (`--snapshot-retain`, min 1).
    pub retain: usize,
}

/// Backup-freshness metrics shared between the backup task (writer) and the
/// Prometheus exporter (reader). All timestamps are unix seconds; `0` means
/// "never since process start", which makes `time() - <metric>` correctly
/// huge in PromQL when backups are enabled but have never succeeded.
#[derive(Debug, Default)]
pub struct SnapshotBackupMetrics {
    /// Unix time of the last fully successful run (snapshot + archive +
    /// upload if configured). The freshness/RPO alert watches this.
    pub last_success_unix: AtomicU64,
    /// Unix time of the last attempt, successful or not.
    pub last_attempt_unix: AtomicU64,
    /// Total failed runs since process start.
    pub failures_total: AtomicU64,
    /// Duration of the last successful run, f64 seconds stored as bits.
    last_duration_secs_bits: AtomicU64,
    /// Size in bytes of the last successfully produced archive.
    pub last_size_bytes: AtomicU64,
}

impl SnapshotBackupMetrics {
    pub fn last_duration_secs(&self) -> f64 {
        f64::from_bits(self.last_duration_secs_bits.load(Ordering::Relaxed))
    }

    fn set_last_duration_secs(&self, secs: f64) {
        self.last_duration_secs_bits
            .store(secs.to_bits(), Ordering::Relaxed);
    }
}

/// Outcome of one successful backup run (returned by [`run_backup_once`]).
#[derive(Debug)]
pub struct BackupOutcome {
    /// Path of the archive produced (may already be pruned if `retain` is
    /// small and another archive superseded it — not possible within one run).
    pub archive_path: PathBuf,
    /// Size of the archive in bytes.
    pub size_bytes: u64,
    /// Chain head height captured in the snapshot manifest (0 if the DB had
    /// no head yet).
    pub block_height: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Spawn the periodic backup task. The first snapshot is taken immediately
/// (so freshness metrics are populated soon after start), then every
/// `config.interval`. Returns the task handle; the task runs until the node
/// shuts down. Errors are logged and counted, never propagated.
pub fn spawn(
    db: Database,
    config: SnapshotBackupConfig,
    metrics: Arc<SnapshotBackupMetrics>,
) -> tokio::task::JoinHandle<()> {
    info!(
        "Snapshot backups enabled: every {:?} into {} (retain {}, upload: {})",
        config.interval,
        config.dir.display(),
        config.retain.max(1),
        config.upload_cmd.as_deref().unwrap_or("local only"),
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;

            let started = Instant::now();
            metrics
                .last_attempt_unix
                .store(now_unix(), Ordering::Relaxed);

            // The whole pipeline is blocking I/O (checkpoint, tar, child
            // process) — keep it off the async runtime.
            let db = db.clone();
            let config = config.clone();
            let result = tokio::task::spawn_blocking(move || run_backup_once(&db, &config)).await;

            match result {
                Ok(Ok(outcome)) => {
                    let elapsed = started.elapsed().as_secs_f64();
                    metrics
                        .last_success_unix
                        .store(now_unix(), Ordering::Relaxed);
                    metrics
                        .last_size_bytes
                        .store(outcome.size_bytes, Ordering::Relaxed);
                    metrics.set_last_duration_secs(elapsed);
                    info!(
                        "Snapshot backup succeeded: {} ({} bytes, height {}, {:.1}s)",
                        outcome.archive_path.display(),
                        outcome.size_bytes,
                        outcome.block_height,
                        elapsed,
                    );
                }
                Ok(Err(e)) => {
                    metrics.failures_total.fetch_add(1, Ordering::Relaxed);
                    warn!("Snapshot backup failed (node keeps running): {e:#}");
                }
                Err(e) => {
                    metrics.failures_total.fetch_add(1, Ordering::Relaxed);
                    warn!("Snapshot backup task panicked (node keeps running): {e}");
                }
            }
        }
    })
}

/// One full backup run: snapshot → tar.gz → prune → upload.
///
/// Blocking; call from `spawn_blocking`. Pruning happens *before* the upload
/// so persistent upload failures cannot fill the disk (the just-created
/// archive is always within the newest `retain >= 1`).
pub fn run_backup_once(
    db: &Database,
    config: &SnapshotBackupConfig,
) -> anyhow::Result<BackupOutcome> {
    std::fs::create_dir_all(&config.dir)?;
    let ts = now_unix();

    // 1. Snapshot (RocksDB checkpoint) into a hidden work dir on the same
    //    filesystem as the final archive.
    let work_dir = config.dir.join(format!(".work-{}-{}", config.prefix, ts));
    if work_dir.exists() {
        std::fs::remove_dir_all(&work_dir)?;
    }
    let manifest = db.create_snapshot(&work_dir, ts)?;
    let height = manifest.block_height.unwrap_or(0);

    // 2. Archive: `<prefix>-<height>-<unix_ts>.tar.gz`, containing a single
    //    top-level directory named like the archive stem (so extraction
    //    yields `<prefix>-<height>-<unix_ts>/` with the DB files + manifest).
    let stem = format!("{}-{}-{}", config.prefix, height, ts);
    let archive_path = config.dir.join(format!("{stem}.tar.gz"));
    let archive_result = (|| -> anyhow::Result<()> {
        let file = std::fs::File::create(&archive_path)?;
        let enc = GzEncoder::new(file, Compression::fast());
        let mut builder = tar::Builder::new(enc);
        builder.append_dir_all(&stem, &work_dir)?;
        builder.into_inner()?.finish()?;
        Ok(())
    })();
    // The snapshot dir is no longer needed either way.
    let _ = std::fs::remove_dir_all(&work_dir);
    if let Err(e) = archive_result {
        let _ = std::fs::remove_file(&archive_path);
        return Err(e.context("packing snapshot archive"));
    }
    let size_bytes = std::fs::metadata(&archive_path)?.len();

    // 3. Local retention (before upload, see doc comment).
    prune_local_archives(&config.dir, &config.prefix, config.retain.max(1))?;

    // 4. Ship to object storage via the configured external command.
    if let Some(cmd) = &config.upload_cmd {
        run_upload_command(cmd, &archive_path)?;
    }

    Ok(BackupOutcome {
        archive_path,
        size_bytes,
        block_height: height,
    })
}

/// Keep the newest `retain` archives matching `<prefix>-*.tar.gz` in `dir`
/// (by modification time), delete the rest.
fn prune_local_archives(dir: &Path, prefix: &str, retain: usize) -> anyhow::Result<()> {
    let mut archives: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !entry.file_type()?.is_file()
            || !name.starts_with(&format!("{prefix}-"))
            || !name.ends_with(".tar.gz")
        {
            continue;
        }
        let mtime = entry
            .metadata()?
            .modified()
            .unwrap_or(std::time::UNIX_EPOCH);
        archives.push((mtime, entry.path()));
    }
    // Newest first.
    archives.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, path) in archives.into_iter().skip(retain) {
        info!("Pruning old snapshot archive: {}", path.display());
        if let Err(e) = std::fs::remove_file(&path) {
            warn!("Failed to prune {}: {e}", path.display());
        }
    }
    Ok(())
}

/// Run the upload command via `sh -c`, substituting `{file}` with the
/// single-quoted archive path (appended as the last argument when no
/// placeholder is present). Non-zero exit is an error.
fn run_upload_command(cmd_template: &str, archive: &Path) -> anyhow::Result<()> {
    // POSIX single-quote escaping: ' -> '\''
    let quoted = format!("'{}'", archive.display().to_string().replace('\'', r"'\''"));
    let cmd = if cmd_template.contains("{file}") {
        cmd_template.replace("{file}", &quoted)
    } else {
        format!("{cmd_template} {quoted}")
    };

    info!("Uploading snapshot archive: {cmd}");
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "upload command failed (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use qfc_storage::{cf, meta, SnapshotManifest, StorageConfig, MANIFEST_FILE_NAME};

    fn test_db(dir: &Path) -> Database {
        let db = Database::open(StorageConfig {
            path: dir.join("db"),
            create_if_missing: true,
            ..Default::default()
        })
        .unwrap();
        db.put(cf::METADATA, meta::LATEST_BLOCK_NUMBER, &7u64.to_le_bytes())
            .unwrap();
        db.put(cf::STATE, b"k", b"v").unwrap();
        db
    }

    fn test_config(dir: &Path) -> SnapshotBackupConfig {
        SnapshotBackupConfig {
            interval: Duration::from_secs(3600),
            dir: dir.join("snapshots"),
            prefix: "qfc-snapshot".into(),
            upload_cmd: None,
            retain: 2,
        }
    }

    #[test]
    fn backup_produces_tar_gz_with_manifest_and_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db(tmp.path());
        let config = test_config(tmp.path());

        let outcome = run_backup_once(&db, &config).unwrap();
        assert_eq!(outcome.block_height, 7);
        assert!(outcome.size_bytes > 0);
        let name = outcome.archive_path.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("qfc-snapshot-7-") && name.ends_with(".tar.gz"),
            "unexpected archive name {name}"
        );

        // The work dir must be cleaned up.
        assert!(!std::fs::read_dir(&config.dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".work-")
        }));

        // Unpack and verify: single root dir named like the archive stem,
        // containing a parseable manifest, and openable as a database.
        let unpack = tmp.path().join("unpack");
        let file = std::fs::File::open(&outcome.archive_path).unwrap();
        tar::Archive::new(GzDecoder::new(file))
            .unpack(&unpack)
            .unwrap();
        let stem = name.trim_end_matches(".tar.gz");
        let root = unpack.join(stem);
        assert!(root.join(MANIFEST_FILE_NAME).is_file());

        let manifest = SnapshotManifest::read_from_dir(&root).unwrap();
        assert_eq!(manifest.block_height, Some(7));

        let restored = Database::open(StorageConfig {
            path: root,
            create_if_missing: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(restored.get(cf::STATE, b"k").unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn retention_keeps_newest_n_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db(tmp.path());
        let mut config = test_config(tmp.path());
        config.retain = 2;

        std::fs::create_dir_all(&config.dir).unwrap();
        // Two pre-existing "old" archives with old mtimes.
        for (i, ts) in [(1u64, 100u64), (2, 200)] {
            let p = config.dir.join(format!("qfc-snapshot-{i}-{ts}.tar.gz"));
            std::fs::write(&p, b"old").unwrap();
            // Make mtimes clearly older than anything created now.
            let old = SystemTime::now() - Duration::from_secs(86_400 * (3 - i));
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(old).unwrap();
        }
        // An unrelated file must never be pruned.
        let unrelated = config.dir.join("keep-me.txt");
        std::fs::write(&unrelated, b"x").unwrap();

        let outcome = run_backup_once(&db, &config).unwrap();

        let mut archives: Vec<String> = std::fs::read_dir(&config.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tar.gz"))
            .collect();
        archives.sort();
        // retain=2: the new archive + the newest old one survive.
        assert_eq!(archives.len(), 2, "archives: {archives:?}");
        assert!(archives.contains(
            &outcome
                .archive_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        ));
        assert!(archives.contains(&"qfc-snapshot-2-200.tar.gz".to_string()));
        assert!(unrelated.is_file());
    }

    #[test]
    fn upload_command_receives_substituted_path() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db(tmp.path());
        let mut config = test_config(tmp.path());
        let captured = tmp.path().join("uploaded-path.txt");
        config.upload_cmd = Some(format!("printf %s {{file}} > {}", captured.display()));

        let outcome = run_backup_once(&db, &config).unwrap();

        let recorded = std::fs::read_to_string(&captured).unwrap();
        assert_eq!(recorded, outcome.archive_path.display().to_string());
    }

    #[test]
    fn upload_command_appends_path_without_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("copied.tar.gz");
        let src = tmp.path().join("a b'c.tar.gz"); // exercise shell quoting
        std::fs::write(&src, b"data").unwrap();
        // No {file} placeholder: the quoted path is appended as the last
        // argument, here becoming the stdin redirect source.
        run_upload_command(&format!("cat > {} <", dest.display()), &src).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");
    }

    #[test]
    fn upload_failure_is_an_error_but_archive_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let db = test_db(tmp.path());
        let mut config = test_config(tmp.path());
        config.upload_cmd = Some("exit 3".into());

        let err = run_backup_once(&db, &config).unwrap_err();
        assert!(err.to_string().contains("upload command failed"), "{err}");

        // The archive is still on disk (within retention) for a retry/next run.
        let archives = std::fs::read_dir(&config.dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tar.gz")
            })
            .count();
        assert_eq!(archives, 1);
    }

    #[test]
    fn metrics_duration_roundtrip() {
        let m = SnapshotBackupMetrics::default();
        assert_eq!(m.last_duration_secs(), 0.0);
        m.set_last_duration_secs(12.5);
        assert_eq!(m.last_duration_secs(), 12.5);
    }
}
