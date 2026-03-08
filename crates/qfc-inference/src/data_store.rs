//! Content-addressed data store for large inference I/O
//!
//! Inference tasks involving images, audio, or generated content can have
//! input/output data in the MB range. This data cannot be gossiped via P2P
//! or stored inline in blocks. Instead, we store data by its Blake3 hash
//! and transfer the actual bytes via a dedicated fetch protocol.
//!
//! Flow:
//! 1. Submitter stores data locally → gets back `DataRef { hash, size }`
//! 2. Task/proof only includes the `DataRef` (32-byte hash + 8-byte size)
//! 3. Miners/verifiers fetch actual bytes by hash from the submitter or peers
//! 4. After fetching, verify `blake3(bytes) == hash` before using

use qfc_types::Hash;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::InferenceError;

/// Maximum inline data size in bytes (64 KB).
/// Data smaller than this is embedded directly in the task/proof.
/// Data larger than this must use content-addressed storage.
pub const MAX_INLINE_SIZE: usize = 64 * 1024;

/// Reference to content-addressed data (stored externally).
/// Only the hash and size are gossiped via P2P — actual bytes are fetched separately.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    borsh::BorshSerialize,
    borsh::BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub struct DataRef {
    /// Blake3 hash of the data (content address)
    pub hash: Hash,
    /// Size of the data in bytes
    pub size: u64,
}

impl DataRef {
    pub fn new(hash: Hash, size: u64) -> Self {
        Self { hash, size }
    }

    /// Create a DataRef from raw data (computes hash)
    pub fn from_data(data: &[u8]) -> Self {
        Self {
            hash: qfc_crypto::blake3_hash(data),
            size: data.len() as u64,
        }
    }
}

impl std::fmt::Display for DataRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.hash, self.size)
    }
}

/// Input data for an inference task — either inline bytes or an external reference.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    borsh::BorshSerialize,
    borsh::BorshDeserialize,
    Serialize,
    Deserialize,
)]
pub enum TaskData {
    /// Small data embedded directly (≤ MAX_INLINE_SIZE)
    Inline(Vec<u8>),
    /// Large data stored externally, referenced by hash
    External(DataRef),
}

impl TaskData {
    /// Create TaskData from raw bytes, choosing inline or external automatically
    pub fn from_bytes(data: Vec<u8>) -> Self {
        if data.len() <= MAX_INLINE_SIZE {
            Self::Inline(data)
        } else {
            let data_ref = DataRef::from_data(&data);
            Self::External(data_ref)
        }
    }

    /// Get the hash of the data (whether inline or external)
    pub fn hash(&self) -> Hash {
        match self {
            Self::Inline(data) => qfc_crypto::blake3_hash(data),
            Self::External(r) => r.hash,
        }
    }

    /// Get the data size in bytes
    pub fn size(&self) -> u64 {
        match self {
            Self::Inline(data) => data.len() as u64,
            Self::External(r) => r.size,
        }
    }

    /// Check if data is stored externally (needs fetching)
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }
}

/// Content-addressed local data store backed by filesystem.
///
/// Data is stored as flat files named by their Blake3 hash hex.
/// Directory structure: `<base_dir>/<hash_hex>`
pub struct LocalDataStore {
    base_dir: PathBuf,
}

impl LocalDataStore {
    /// Create a new local data store at the given directory
    pub fn new(base_dir: PathBuf) -> Result<Self, InferenceError> {
        std::fs::create_dir_all(&base_dir).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to create data store dir: {}", e))
        })?;
        Ok(Self { base_dir })
    }

    /// Store data and return its content-addressed reference
    pub fn put(&self, data: &[u8]) -> Result<DataRef, InferenceError> {
        let hash = qfc_crypto::blake3_hash(data);
        let path = self.path_for_hash(&hash);

        // Skip if already stored (content-addressed = idempotent)
        if path.exists() {
            return Ok(DataRef::new(hash, data.len() as u64));
        }

        // Write atomically via temp file + rename
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, data).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to write data: {}", e))
        })?;
        std::fs::rename(&tmp_path, &path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Failed to rename data file: {}", e))
        })?;

        Ok(DataRef::new(hash, data.len() as u64))
    }

    /// Retrieve data by hash, verifying integrity
    pub fn get(&self, hash: &Hash) -> Result<Vec<u8>, InferenceError> {
        let path = self.path_for_hash(hash);
        let data = std::fs::read(&path).map_err(|e| {
            InferenceError::ExecutionFailed(format!("Data not found for hash {}: {}", hash, e))
        })?;

        // Verify integrity
        let actual_hash = qfc_crypto::blake3_hash(&data);
        if actual_hash != *hash {
            return Err(InferenceError::ExecutionFailed(format!(
                "Data integrity check failed: expected {}, got {}",
                hash, actual_hash
            )));
        }

        Ok(data)
    }

    /// Check if data exists in the store
    pub fn contains(&self, hash: &Hash) -> bool {
        self.path_for_hash(hash).exists()
    }

    /// Delete data by hash
    pub fn delete(&self, hash: &Hash) -> Result<(), InferenceError> {
        let path = self.path_for_hash(hash);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                InferenceError::ExecutionFailed(format!("Failed to delete data: {}", e))
            })?;
        }
        Ok(())
    }

    /// Get total size of all stored data in bytes
    pub fn total_size_bytes(&self) -> u64 {
        std::fs::read_dir(&self.base_dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Resolve TaskData to actual bytes (fetching from store if external)
    pub fn resolve(&self, task_data: &TaskData) -> Result<Vec<u8>, InferenceError> {
        match task_data {
            TaskData::Inline(data) => Ok(data.clone()),
            TaskData::External(data_ref) => self.get(&data_ref.hash),
        }
    }

    fn path_for_hash(&self, hash: &Hash) -> PathBuf {
        self.base_dir.join(format!("{}", hash))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_ref_from_data() {
        let data = b"hello world";
        let data_ref = DataRef::from_data(data);
        assert_eq!(data_ref.size, 11);
        assert_eq!(data_ref.hash, qfc_crypto::blake3_hash(data));
    }

    #[test]
    fn test_task_data_inline() {
        let small = vec![1u8; 100];
        let td = TaskData::from_bytes(small.clone());
        assert!(!td.is_external());
        assert_eq!(td.size(), 100);
    }

    #[test]
    fn test_task_data_external() {
        let large = vec![0u8; MAX_INLINE_SIZE + 1];
        let td = TaskData::from_bytes(large.clone());
        assert!(td.is_external());
        assert_eq!(td.size(), (MAX_INLINE_SIZE + 1) as u64);
    }

    #[test]
    fn test_local_data_store() {
        let dir = std::env::temp_dir().join(format!("qfc_data_test_{}", std::process::id()));
        let store = LocalDataStore::new(dir.clone()).unwrap();

        // Put data
        let data = b"test data for content-addressed storage";
        let data_ref = store.put(data).unwrap();
        assert_eq!(data_ref.size, data.len() as u64);

        // Get data
        let retrieved = store.get(&data_ref.hash).unwrap();
        assert_eq!(retrieved, data);

        // Contains check
        assert!(store.contains(&data_ref.hash));
        assert!(!store.contains(&Hash::ZERO));

        // Idempotent put
        let data_ref2 = store.put(data).unwrap();
        assert_eq!(data_ref.hash, data_ref2.hash);

        // Delete
        store.delete(&data_ref.hash).unwrap();
        assert!(!store.contains(&data_ref.hash));

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_inline() {
        let dir = std::env::temp_dir().join(format!("qfc_resolve_test_{}", std::process::id()));
        let store = LocalDataStore::new(dir.clone()).unwrap();

        let data = vec![42u8; 100];
        let td = TaskData::Inline(data.clone());
        let resolved = store.resolve(&td).unwrap();
        assert_eq!(resolved, data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_external() {
        let dir =
            std::env::temp_dir().join(format!("qfc_resolve_ext_test_{}", std::process::id()));
        let store = LocalDataStore::new(dir.clone()).unwrap();

        let data = vec![99u8; 1000];
        let data_ref = store.put(&data).unwrap();
        let td = TaskData::External(data_ref);
        let resolved = store.resolve(&td).unwrap();
        assert_eq!(resolved, data);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_integrity_check() {
        let dir =
            std::env::temp_dir().join(format!("qfc_integrity_test_{}", std::process::id()));
        let store = LocalDataStore::new(dir.clone()).unwrap();

        let data = b"original data";
        let data_ref = store.put(data).unwrap();

        // Tamper with the file
        let path = store.path_for_hash(&data_ref.hash);
        std::fs::write(&path, b"tampered data").unwrap();

        // Get should fail integrity check
        let result = store.get(&data_ref.hash);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("integrity check failed"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
