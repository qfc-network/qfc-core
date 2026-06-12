//! Sharded model distribution (ADR-0001, Roadmap AI-V3 milestone B-1)
//!
//! Large model weight files are split into verifiable byte-range shards.
//! Each shard is content-addressed by its Blake3 hash and fetched from an
//! IPFS gateway by CID. Shards live in a [`LocalDataStore`] keyed by hash,
//! which gives resumable downloads (already-present shards are skipped) and
//! cross-version reuse (a fine-tune that only changed some layers re-uses
//! the byte-identical shards already on disk).
//!
//! Flow (ADR-0001 Decision 4):
//! 1. For each manifest entry: skip if cached, else fetch `<gateway>/ipfs/<cid>`
//!    via curl and verify `blake3(bytes) == entry.hash` before storing.
//! 2. Assemble: concatenate shards in manifest order into a temp file while
//!    streaming a Blake3 hash; verify against `assembled_hash`; atomic rename.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use borsh::{BorshDeserialize, BorshSerialize};
use qfc_types::Hash;
use serde::{Deserialize, Serialize};

use crate::data_store::LocalDataStore;
use crate::InferenceError;

/// One byte-range shard of a model weight file.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ShardEntry {
    /// IPFS CID of the shard bytes (empty until published to IPFS)
    pub cid: String,
    /// Blake3 hash of the shard bytes (verification key)
    pub hash: Hash,
    /// Size of the shard in bytes
    pub size_bytes: u64,
    /// Optional `[start, end)` layer range covered by this shard.
    /// Metadata only in B-1; B-2 pipeline execution will use it.
    pub layer_range: Option<(u32, u32)>,
}

/// Ordered list of shards describing a complete model weight file.
///
/// Concatenating the shards in order yields the weight file; its Blake3
/// hash must equal `assembled_hash` (and `ModelInfo::weights_hash` when set).
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
pub struct ShardManifest {
    /// Shards in file order (concatenation order = byte order)
    pub shards: Vec<ShardEntry>,
    /// Total size of the assembled file in bytes (must equal sum of shard sizes)
    pub total_size_bytes: u64,
    /// Blake3 hash of the assembled (concatenated) file
    pub assembled_hash: Hash,
}

impl ShardManifest {
    /// Validate structural invariants of the manifest.
    ///
    /// Checks: non-empty shard list, no zero-size shards, shard sizes sum to
    /// `total_size_bytes`, CIDs are alphanumeric (or empty), and layer ranges
    /// (if used) are present on every shard, non-empty, contiguous, and
    /// non-overlapping in order.
    pub fn validate(&self) -> Result<(), InferenceError> {
        if self.shards.is_empty() {
            return Err(InferenceError::ShardVerification(
                "Invalid shard manifest: shard list is empty".to_string(),
            ));
        }

        let mut sum: u64 = 0;
        for (i, shard) in self.shards.iter().enumerate() {
            if shard.size_bytes == 0 {
                return Err(InferenceError::ShardVerification(format!(
                    "Invalid shard manifest: shard {} has zero size",
                    i
                )));
            }
            // Non-empty CIDs must be purely alphanumeric ([A-Za-z0-9]+). This
            // covers base58btc CIDv0 ("Qm...") and lowercase base32 CIDv1
            // ("bafy...") while rejecting `/ . ? # %` and friends — which is
            // what makes interpolating the cid into the gateway URL
            // (`<gateway>/ipfs/<cid>`) safe from path traversal and query
            // injection. An empty cid stays allowed: it is the publisher's
            // pre-upload state (`split_file` emits empty cids); fetching such
            // an entry is rejected separately in `ensure_shard`.
            if !shard.cid.is_empty() && !shard.cid.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(InferenceError::ShardVerification(format!(
                    "Invalid shard manifest: shard {} cid {:?} contains characters outside [A-Za-z0-9]",
                    i, shard.cid
                )));
            }
            sum = sum.checked_add(shard.size_bytes).ok_or_else(|| {
                InferenceError::ShardVerification(
                    "Invalid shard manifest: shard sizes overflow u64".to_string(),
                )
            })?;
        }
        if sum != self.total_size_bytes {
            return Err(InferenceError::ShardVerification(format!(
                "Invalid shard manifest: shard sizes sum to {} but total_size_bytes is {}",
                sum, self.total_size_bytes
            )));
        }

        // Layer ranges: all-or-none, each non-empty, contiguous and in order.
        let with_range = self
            .shards
            .iter()
            .filter(|s| s.layer_range.is_some())
            .count();
        if with_range > 0 {
            if with_range != self.shards.len() {
                return Err(InferenceError::ShardVerification(
                    "Invalid shard manifest: layer_range must be set on all shards or none"
                        .to_string(),
                ));
            }
            let mut prev_end: Option<u32> = None;
            for (i, shard) in self.shards.iter().enumerate() {
                let (start, end) = shard.layer_range.expect("checked above");
                if start >= end {
                    return Err(InferenceError::ShardVerification(format!(
                        "Invalid shard manifest: shard {} has empty layer range [{}, {})",
                        i, start, end
                    )));
                }
                if let Some(prev) = prev_end {
                    if start != prev {
                        return Err(InferenceError::ShardVerification(format!(
                            "Invalid shard manifest: shard {} layer range starts at {} but previous shard ends at {} (ranges must be contiguous and non-overlapping)",
                            i, start, prev
                        )));
                    }
                }
                prev_end = Some(end);
            }
        }

        Ok(())
    }

    /// Split a weight file into `shard_size`-byte shards (governance/publishing
    /// helper). Reads the file in chunks — never loads it whole into memory.
    ///
    /// The returned entries have empty `cid` (filled in by the publisher after
    /// uploading each shard to IPFS) and no `layer_range`.
    pub fn split_file(path: &Path, shard_size: u64) -> Result<ShardManifest, InferenceError> {
        if shard_size == 0 {
            return Err(InferenceError::ExecutionFailed(
                "Cannot split file: shard_size must be > 0".to_string(),
            ));
        }

        let mut file = std::fs::File::open(path)?;
        let mut whole_hasher = blake3::Hasher::new();
        let mut shards = Vec::new();
        let mut total_size_bytes: u64 = 0;

        loop {
            let mut chunk = Vec::with_capacity(shard_size.min(64 * 1024 * 1024) as usize);
            let n = (&mut file).take(shard_size).read_to_end(&mut chunk)?;
            if n == 0 {
                break;
            }
            whole_hasher.update(&chunk);
            shards.push(ShardEntry {
                cid: String::new(),
                hash: qfc_crypto::blake3_hash(&chunk),
                size_bytes: n as u64,
                layer_range: None,
            });
            total_size_bytes += n as u64;
        }

        if shards.is_empty() {
            return Err(InferenceError::ExecutionFailed(format!(
                "Cannot split file {}: file is empty",
                path.display()
            )));
        }

        Ok(ShardManifest {
            shards,
            total_size_bytes,
            assembled_hash: Hash::new(*whole_hasher.finalize().as_bytes()),
        })
    }
}

/// Downloads manifest shards from an IPFS gateway into a content-addressed
/// shard store and assembles them into the final weight file (ADR-0001).
pub struct ShardDownloader {
    /// IPFS gateway base URL (e.g. `http://127.0.0.1:8080`)
    gateway_url: String,
    /// Content-addressed shard store (`<shard_dir>/<hash>`)
    store: LocalDataStore,
    /// Private shard store directory. Download temp files live here too —
    /// not in the world-writable system temp dir, where a pre-planted
    /// symlink or a colliding name could clobber/redirect the write.
    shard_dir: PathBuf,
}

/// Monotonic counter making download temp-file names unique per attempt,
/// even across threads in the same process.
static TMP_ATTEMPT: AtomicU64 = AtomicU64::new(0);

impl ShardDownloader {
    /// Create a downloader fetching from `gateway_url` and caching shards in `shard_dir`.
    pub fn new(gateway_url: impl Into<String>, shard_dir: PathBuf) -> Result<Self, InferenceError> {
        Ok(Self {
            gateway_url: gateway_url.into(),
            store: LocalDataStore::new(shard_dir.clone())?,
            shard_dir,
        })
    }

    /// Access the underlying shard store (e.g. for pre-seeding or cleanup).
    pub fn store(&self) -> &LocalDataStore {
        &self.store
    }

    /// Ensure a shard is present in the local store, fetching it from the
    /// gateway if missing. Already-cached shards are never re-downloaded —
    /// this is what makes downloads resumable and shards reusable across
    /// model versions (ADR-0001 Decision 3).
    pub fn ensure_shard(&self, entry: &ShardEntry) -> Result<(), InferenceError> {
        if self.store.contains(&entry.hash) {
            tracing::debug!("Shard {} already cached, skipping fetch", entry.hash);
            return Ok(());
        }

        // An empty cid is the publisher's pre-upload state — there is no
        // valid gateway URL to construct, so refuse instead of fetching
        // `<gateway>/ipfs/` (a garbage URL).
        if entry.cid.is_empty() {
            return Err(InferenceError::ExecutionFailed(format!(
                "Cannot fetch shard {}: manifest entry has an empty cid (shard not yet published to IPFS)",
                entry.hash
            )));
        }

        let url = shard_url(&self.gateway_url, &entry.cid);
        // Temp file inside the private shard store dir (not the shared system
        // temp dir), named uniquely per attempt: pid distinguishes processes,
        // the atomic counter distinguishes threads/retries within a process.
        let attempt = TMP_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let tmp_path = self.shard_dir.join(format!(
            ".qfc-shard-{}-{}-{}.part",
            std::process::id(),
            attempt,
            entry.hash
        ));

        let fetch_result = fetch_url(&url, &tmp_path, entry.size_bytes);
        if let Err(e) = fetch_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }

        // Stat the downloaded file BEFORE reading it into memory: curl's
        // --max-filesize is ignored for chunked transfer-encoding, so this
        // check is the real bound on how much attacker-controlled data we
        // are willing to load and hash.
        let actual_size = match std::fs::metadata(&tmp_path) {
            Ok(m) => m.len(),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e.into());
            }
        };
        if actual_size != entry.size_bytes {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(InferenceError::ShardVerification(format!(
                "Shard size mismatch for cid {}: expected {} bytes, downloaded {}",
                entry.cid, entry.size_bytes, actual_size
            )));
        }

        let bytes = match std::fs::read(&tmp_path) {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e.into());
            }
        };
        let _ = std::fs::remove_file(&tmp_path);

        let actual_hash = qfc_crypto::blake3_hash(&bytes);
        if actual_hash != entry.hash {
            return Err(InferenceError::ShardVerification(format!(
                "Shard hash mismatch for cid {}: expected {}, got {}",
                entry.cid, entry.hash, actual_hash
            )));
        }

        self.store.put(&bytes)?;
        Ok(())
    }

    /// Download all manifest shards (skipping cached ones) and assemble them
    /// into `dest`, verifying the streamed Blake3 of the assembled file
    /// against `manifest.assembled_hash`. Idempotent: if `dest` already
    /// exists with the expected hash, returns immediately.
    pub fn download_and_assemble(
        &self,
        manifest: &ShardManifest,
        dest: &Path,
    ) -> Result<(), InferenceError> {
        manifest.validate()?;

        // Idempotent early return: dest already assembled and intact.
        if dest.exists() && file_blake3(dest)? == manifest.assembled_hash {
            tracing::info!(
                "Assembled model already present at {} with matching hash, skipping",
                dest.display()
            );
            return Ok(());
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let total = manifest.shards.len();
        for (i, entry) in manifest.shards.iter().enumerate() {
            tracing::info!(
                "Ensuring shard {}/{} ({} bytes, cid {})",
                i + 1,
                total,
                entry.size_bytes,
                entry.cid
            );
            self.ensure_shard(entry)?;
        }

        // Assemble: concatenate shards in order into a temp file while
        // streaming the Blake3 hash, then atomic rename into place.
        let tmp_path = dest.with_extension("tmp");
        let mut hasher = blake3::Hasher::new();
        {
            let mut file = std::fs::File::create(&tmp_path)?;
            for entry in &manifest.shards {
                let bytes = match self.store.get(&entry.hash) {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(e);
                    }
                };
                hasher.update(&bytes);
                if let Err(e) = file.write_all(&bytes) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e.into());
                }
            }
            let assembled = Hash::new(*hasher.finalize().as_bytes());
            if assembled != manifest.assembled_hash {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(InferenceError::ShardVerification(format!(
                    "Assembled file hash mismatch: expected {}, got {}",
                    manifest.assembled_hash, assembled
                )));
            }
            if let Err(e) = file.sync_all() {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e.into());
            }
        }
        std::fs::rename(&tmp_path, dest)?;

        tracing::info!(
            "Assembled {} shards ({} bytes) into {}",
            total,
            manifest.total_size_bytes,
            dest.display()
        );
        Ok(())
    }
}

/// Build the gateway fetch URL for a shard CID: `<gateway>/ipfs/<cid>`.
fn shard_url(gateway_url: &str, cid: &str) -> String {
    format!("{}/ipfs/{}", gateway_url.trim_end_matches('/'), cid)
}

/// Fetch a URL to a destination file via `curl -sfL` (same convention as
/// `download.rs` and `qfc-ai-coordinator/src/ipfs.rs`), bounded in size and
/// time.
///
/// - `--max-filesize` caps the transfer at the manifest-declared shard size.
///   curl ignores it for chunked transfer-encoding, so the caller MUST also
///   stat the result against `max_size_bytes` — that check is the real
///   guarantee; this flag just aborts obviously-oversized transfers early.
/// - `--max-time 300` is a fixed wall-clock ceiling rather than one computed
///   from size: shards are bounded artifacts (publishers split big weight
///   files into many shards precisely so each download is small), and a
///   fixed ceiling keeps behavior predictable while still preventing a
///   stalled or throttled gateway from hanging a miner indefinitely.
fn fetch_url(url: &str, dest: &Path, max_size_bytes: u64) -> Result<(), InferenceError> {
    let output = std::process::Command::new("curl")
        .args(["-sfL", "--max-time", "300", "--max-filesize"])
        .arg(max_size_bytes.to_string())
        .arg("-o")
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to spawn curl for {}: {}", url, e))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(InferenceError::ExecutionFailed(format!(
            "Failed to fetch shard from {}: curl exited with {} ({})",
            url,
            output.status,
            stderr.trim()
        )));
    }
    Ok(())
}

/// Streaming Blake3 hash of a file on disk.
fn file_blake3(path: &Path) -> Result<Hash, InferenceError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(Hash::new(*hasher.finalize().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qfc_shard_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_entry(data: &[u8]) -> ShardEntry {
        ShardEntry {
            cid: String::new(),
            hash: qfc_crypto::blake3_hash(data),
            size_bytes: data.len() as u64,
            layer_range: None,
        }
    }

    #[test]
    fn test_shard_url_join() {
        assert_eq!(
            shard_url("http://127.0.0.1:8080", "QmFoo"),
            "http://127.0.0.1:8080/ipfs/QmFoo"
        );
        assert_eq!(
            shard_url("http://127.0.0.1:8080/", "QmFoo"),
            "http://127.0.0.1:8080/ipfs/QmFoo"
        );
    }

    #[test]
    fn test_split_file_round_trip() {
        let dir = test_dir("split");
        let file_path = dir.join("weights.bin");
        // 2500 bytes with a non-trivial pattern → shards of 1000, 1000, 500
        let data: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file_path, &data).unwrap();

        let manifest = ShardManifest::split_file(&file_path, 1000).unwrap();
        manifest.validate().unwrap();

        assert_eq!(manifest.shards.len(), 3);
        assert_eq!(manifest.total_size_bytes, 2500);
        assert_eq!(manifest.shards[0].size_bytes, 1000);
        assert_eq!(manifest.shards[1].size_bytes, 1000);
        assert_eq!(manifest.shards[2].size_bytes, 500); // last-shard remainder
        assert_eq!(manifest.assembled_hash, qfc_crypto::blake3_hash(&data));
        assert_eq!(
            manifest.shards[0].hash,
            qfc_crypto::blake3_hash(&data[..1000])
        );
        assert_eq!(
            manifest.shards[2].hash,
            qfc_crypto::blake3_hash(&data[2000..])
        );
        assert!(manifest.shards.iter().all(|s| s.cid.is_empty()));
        assert!(manifest.shards.iter().all(|s| s.layer_range.is_none()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_split_file_rejects_zero_shard_size_and_empty_file() {
        let dir = test_dir("split_reject");
        let file_path = dir.join("weights.bin");
        std::fs::write(&file_path, b"data").unwrap();
        assert!(ShardManifest::split_file(&file_path, 0).is_err());

        let empty_path = dir.join("empty.bin");
        std::fs::write(&empty_path, b"").unwrap();
        assert!(ShardManifest::split_file(&empty_path, 100).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_rejections() {
        // Empty
        let empty = ShardManifest {
            shards: vec![],
            total_size_bytes: 0,
            assembled_hash: Hash::ZERO,
        };
        assert!(empty.validate().is_err());

        // Size mismatch
        let mismatch = ShardManifest {
            shards: vec![make_entry(b"abc")],
            total_size_bytes: 99,
            assembled_hash: Hash::ZERO,
        };
        assert!(mismatch.validate().is_err());

        // Zero-size shard
        let zero = ShardManifest {
            shards: vec![ShardEntry {
                cid: String::new(),
                hash: Hash::ZERO,
                size_bytes: 0,
                layer_range: None,
            }],
            total_size_bytes: 0,
            assembled_hash: Hash::ZERO,
        };
        assert!(zero.validate().is_err());

        // layer_range on some shards but not all
        let mut e1 = make_entry(b"aaa");
        e1.layer_range = Some((0, 4));
        let e2 = make_entry(b"bbb");
        let partial = ShardManifest {
            shards: vec![e1.clone(), e2.clone()],
            total_size_bytes: 6,
            assembled_hash: Hash::ZERO,
        };
        assert!(partial.validate().is_err());

        // Overlapping layer ranges
        let mut e2_overlap = e2.clone();
        e2_overlap.layer_range = Some((2, 8)); // overlaps [0,4)
        let overlapping = ShardManifest {
            shards: vec![e1.clone(), e2_overlap],
            total_size_bytes: 6,
            assembled_hash: Hash::ZERO,
        };
        assert!(overlapping.validate().is_err());

        // Non-contiguous layer ranges (gap)
        let mut e2_gap = e2.clone();
        e2_gap.layer_range = Some((6, 8)); // gap after [0,4)
        let gapped = ShardManifest {
            shards: vec![e1.clone(), e2_gap],
            total_size_bytes: 6,
            assembled_hash: Hash::ZERO,
        };
        assert!(gapped.validate().is_err());

        // Valid contiguous layer ranges pass
        let mut e2_ok = e2;
        e2_ok.layer_range = Some((4, 8));
        let valid = ShardManifest {
            shards: vec![e1, e2_ok],
            total_size_bytes: 6,
            assembled_hash: Hash::ZERO,
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_ensure_shard_cache_hit_no_network() {
        let dir = test_dir("cache_hit");
        let downloader =
            ShardDownloader::new("http://invalid.invalid:1", dir.join("shards")).unwrap();

        let data = b"pre-seeded shard bytes";
        downloader.store().put(data).unwrap();

        let mut entry = make_entry(data);
        entry.cid = "this-cid-does-not-exist".to_string();

        // Must succeed without any network access (shard already in store).
        downloader.ensure_shard(&entry).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_and_assemble_via_file_gateway() {
        let dir = test_dir("file_gw");
        let gw_dir = dir.join("fake-gw").join("ipfs");
        std::fs::create_dir_all(&gw_dir).unwrap();

        // Two shards served as files under <gateway>/ipfs/<cid>
        let shard_a: Vec<u8> = vec![0xAA; 1500];
        let shard_b: Vec<u8> = vec![0xBB; 700];
        std::fs::write(gw_dir.join("cida"), &shard_a).unwrap();
        std::fs::write(gw_dir.join("cidb"), &shard_b).unwrap();

        let mut full = shard_a.clone();
        full.extend_from_slice(&shard_b);

        let mut entry_a = make_entry(&shard_a);
        entry_a.cid = "cida".to_string();
        let mut entry_b = make_entry(&shard_b);
        entry_b.cid = "cidb".to_string();
        let manifest = ShardManifest {
            shards: vec![entry_a, entry_b],
            total_size_bytes: full.len() as u64,
            assembled_hash: qfc_crypto::blake3_hash(&full),
        };

        let gateway = format!("file://{}", dir.join("fake-gw").display());
        let downloader = ShardDownloader::new(gateway, dir.join("shards")).unwrap();

        let dest = dir.join("models").join("assembled.bin");
        downloader.download_and_assemble(&manifest, &dest).unwrap();

        let assembled = std::fs::read(&dest).unwrap();
        assert_eq!(assembled, full);

        // Idempotent early return: remove all shards from the store, dest is
        // already intact → must still succeed without fetching anything.
        for entry in &manifest.shards {
            downloader.store().delete(&entry.hash).unwrap();
        }
        downloader.download_and_assemble(&manifest, &dest).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_gateway_serves_wrong_bytes_rejected() {
        let dir = test_dir("wrong_bytes");
        let gw_dir = dir.join("fake-gw").join("ipfs");
        std::fs::create_dir_all(&gw_dir).unwrap();

        // Gateway serves bytes of the RIGHT size but the WRONG content.
        let good: Vec<u8> = vec![0xAA; 1024];
        let bad: Vec<u8> = vec![0xBB; 1024];
        std::fs::write(gw_dir.join("cidbad"), &bad).unwrap();

        let gateway = format!("file://{}", dir.join("fake-gw").display());
        let downloader = ShardDownloader::new(gateway, dir.join("shards")).unwrap();

        let mut entry = make_entry(&good);
        entry.cid = "cidbad".to_string();

        let err = downloader.ensure_shard(&entry).unwrap_err();
        assert!(
            matches!(err, InferenceError::ShardVerification(_)),
            "expected ShardVerification, got: {:?}",
            err
        );
        assert!(
            err.to_string().contains("cidbad"),
            "error should mention the cid, got: {}",
            err
        );
        // Neither the expected hash nor the bad bytes' hash may be stored.
        assert!(!downloader.store().contains(&entry.hash));
        assert!(!downloader.store().contains(&qfc_crypto::blake3_hash(&bad)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_gateway_serves_wrong_size_rejected() {
        let dir = test_dir("wrong_size");
        let gw_dir = dir.join("fake-gw").join("ipfs");
        std::fs::create_dir_all(&gw_dir).unwrap();

        // Gateway serves fewer bytes than the manifest declares (truncated).
        let good: Vec<u8> = vec![0xCC; 2048];
        std::fs::write(gw_dir.join("cidshort"), &good[..512]).unwrap();

        let gateway = format!("file://{}", dir.join("fake-gw").display());
        let shard_dir = dir.join("shards");
        let downloader = ShardDownloader::new(gateway, shard_dir.clone()).unwrap();

        let mut entry = make_entry(&good);
        entry.cid = "cidshort".to_string();

        let err = downloader.ensure_shard(&entry).unwrap_err();
        assert!(
            matches!(err, InferenceError::ShardVerification(_)),
            "expected ShardVerification, got: {:?}",
            err
        );
        assert!(
            err.to_string().contains("size mismatch"),
            "expected size mismatch error, got: {}",
            err
        );
        assert!(!downloader.store().contains(&entry.hash));
        // Temp download files must not be left behind in the shard dir.
        let leftover: Vec<_> = std::fs::read_dir(&shard_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {:?}", leftover);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_rejects_invalid_cid() {
        for bad_cid in [
            "../evil", "Qm/Foo", "cid?x=1", "cid#frag", "cid%2e", "cid foo",
        ] {
            let mut entry = make_entry(b"abc");
            entry.cid = bad_cid.to_string();
            let manifest = ShardManifest {
                shards: vec![entry],
                total_size_bytes: 3,
                assembled_hash: Hash::ZERO,
            };
            let err = manifest.validate().unwrap_err();
            assert!(
                matches!(err, InferenceError::ShardVerification(_)),
                "cid {:?}: expected ShardVerification, got: {:?}",
                bad_cid,
                err
            );
        }

        // Valid CIDv0/CIDv1 shapes (and the pre-upload empty cid) pass.
        for ok_cid in [
            "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG",
            "bafybeigdyrzt5",
            "",
        ] {
            let mut entry = make_entry(b"abc");
            entry.cid = ok_cid.to_string();
            let manifest = ShardManifest {
                shards: vec![entry],
                total_size_bytes: 3,
                assembled_hash: Hash::ZERO,
            };
            assert!(
                manifest.validate().is_ok(),
                "cid {:?} should be valid",
                ok_cid
            );
        }
    }

    #[test]
    fn test_ensure_shard_empty_cid_errors_cleanly() {
        let dir = test_dir("empty_cid");
        // Invalid gateway: the empty-cid error must fire before any fetch.
        let downloader =
            ShardDownloader::new("http://invalid.invalid:1", dir.join("shards")).unwrap();

        let entry = make_entry(b"never published"); // cid stays empty
        let err = downloader.ensure_shard(&entry).unwrap_err();
        assert!(
            err.to_string().contains("empty cid"),
            "expected empty-cid error, got: {}",
            err
        );
        assert!(!downloader.store().contains(&entry.hash));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_download_and_assemble_pre_seeded() {
        let dir = test_dir("preseed");
        let downloader =
            ShardDownloader::new("http://invalid.invalid:1", dir.join("shards")).unwrap();

        let shard_a: Vec<u8> = (0..1024u32).map(|i| (i % 7) as u8).collect();
        let shard_b: Vec<u8> = (0..333u32).map(|i| (i % 13) as u8).collect();
        downloader.store().put(&shard_a).unwrap();
        downloader.store().put(&shard_b).unwrap();

        let mut full = shard_a.clone();
        full.extend_from_slice(&shard_b);
        let manifest = ShardManifest {
            shards: vec![make_entry(&shard_a), make_entry(&shard_b)],
            total_size_bytes: full.len() as u64,
            assembled_hash: qfc_crypto::blake3_hash(&full),
        };

        let dest = dir.join("assembled.bin");
        downloader.download_and_assemble(&manifest, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), full);
        // tmp file cleaned up by rename
        assert!(!dest.with_extension("tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_assembled_hash_mismatch_rejected() {
        let dir = test_dir("asm_mismatch");
        let downloader =
            ShardDownloader::new("http://invalid.invalid:1", dir.join("shards")).unwrap();

        let shard: Vec<u8> = vec![0x11; 256];
        downloader.store().put(&shard).unwrap();

        let manifest = ShardManifest {
            shards: vec![make_entry(&shard)],
            total_size_bytes: shard.len() as u64,
            assembled_hash: Hash::new([0x42; 32]), // wrong on purpose
        };

        let dest = dir.join("assembled.bin");
        let err = downloader
            .download_and_assemble(&manifest, &dest)
            .unwrap_err();
        assert!(err.to_string().contains("hash mismatch"));
        assert!(!dest.exists());
        assert!(!dest.with_extension("tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_tampered_shard_fails_integrity() {
        let dir = test_dir("tamper");
        let shard_dir = dir.join("shards");
        let downloader =
            ShardDownloader::new("http://invalid.invalid:1", shard_dir.clone()).unwrap();

        let shard: Vec<u8> = vec![0x33; 512];
        let data_ref = downloader.store().put(&shard).unwrap();

        // Corrupt the shard file on disk (filename = Hash Display = 0x-hex).
        let shard_path = shard_dir.join(format!("{}", data_ref.hash));
        assert!(shard_path.exists());
        std::fs::write(&shard_path, vec![0x44; 512]).unwrap();

        let manifest = ShardManifest {
            shards: vec![make_entry(&shard)],
            total_size_bytes: shard.len() as u64,
            assembled_hash: qfc_crypto::blake3_hash(&shard),
        };

        let dest = dir.join("assembled.bin");
        let err = downloader
            .download_and_assemble(&manifest, &dest)
            .unwrap_err();
        assert!(
            err.to_string().contains("integrity"),
            "expected integrity error, got: {}",
            err
        );
        assert!(!dest.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
