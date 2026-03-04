//! Database implementation using RocksDB

use crate::batch::{BatchOp, WriteBatch};
use crate::error::{Result, StorageError};
use crate::schema::{cf, meta, DB_VERSION};
use rocksdb::{
    BoundColumnFamily, DBWithThreadMode, MultiThreaded, Options, WriteBatch as RocksWriteBatch,
};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info};

type RocksDB = DBWithThreadMode<MultiThreaded>;

/// Storage configuration
#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Path to the database directory
    pub path: std::path::PathBuf,

    /// Block cache size in MB
    pub block_cache_size_mb: usize,

    /// Write buffer size in MB
    pub write_buffer_size_mb: usize,

    /// Maximum number of open files
    pub max_open_files: i32,

    /// Enable compression
    pub enable_compression: bool,

    /// Create if missing
    pub create_if_missing: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: std::path::PathBuf::from("./data/db"),
            block_cache_size_mb: 512,
            write_buffer_size_mb: 64,
            max_open_files: 1024,
            enable_compression: true,
            create_if_missing: true,
        }
    }
}

/// Database wrapper
pub struct Database {
    db: Arc<RocksDB>,
    config: StorageConfig,
}

impl Database {
    /// Open the database
    pub fn open(config: StorageConfig) -> Result<Self> {
        info!("Opening database at {:?}", config.path);

        let mut opts = Options::default();
        opts.create_if_missing(config.create_if_missing);
        opts.create_missing_column_families(true);
        opts.set_max_open_files(config.max_open_files);
        opts.set_write_buffer_size(config.write_buffer_size_mb * 1024 * 1024);

        if config.enable_compression {
            opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
        }

        // Create block cache
        let cache = rocksdb::Cache::new_lru_cache(config.block_cache_size_mb * 1024 * 1024);
        let mut block_opts = rocksdb::BlockBasedOptions::default();
        block_opts.set_block_cache(&cache);
        opts.set_block_based_table_factory(&block_opts);

        // Open with column families
        let cf_descriptors: Vec<_> = cf::ALL
            .iter()
            .map(|name| {
                let mut cf_opts = Options::default();
                cf_opts.set_compression_type(if config.enable_compression {
                    rocksdb::DBCompressionType::Lz4
                } else {
                    rocksdb::DBCompressionType::None
                });
                rocksdb::ColumnFamilyDescriptor::new(*name, cf_opts)
            })
            .collect();

        let db = RocksDB::open_cf_descriptors(&opts, &config.path, cf_descriptors)?;

        let db = Self {
            db: Arc::new(db),
            config,
        };

        // Initialize metadata if needed
        db.init_metadata()?;

        Ok(db)
    }

    /// Open a temporary database for testing
    pub fn open_temp() -> Result<Self> {
        let dir = tempfile::tempdir().map_err(|e| StorageError::Io(e.into()))?;
        let path = dir.path().to_path_buf();
        // Keep the directory by forgetting it (prevent cleanup on drop)
        std::mem::forget(dir);
        let config = StorageConfig {
            path,
            create_if_missing: true,
            ..Default::default()
        };
        Self::open(config)
    }

    fn init_metadata(&self) -> Result<()> {
        // Check/set database version
        if self.get(cf::METADATA, meta::DB_VERSION)?.is_none() {
            self.put(cf::METADATA, meta::DB_VERSION, &DB_VERSION.to_le_bytes())?;
            debug!("Initialized database version: {}", DB_VERSION);
        }
        Ok(())
    }

    fn get_cf(&self, cf_name: &str) -> Result<Arc<BoundColumnFamily<'_>>> {
        self.db
            .cf_handle(cf_name)
            .ok_or_else(|| StorageError::ColumnFamilyNotFound(cf_name.to_string()))
    }

    /// Get a value from a column family
    pub fn get(&self, cf_name: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let cf = self.get_cf(cf_name)?;
        Ok(self.db.get_cf(&cf, key)?)
    }

    /// Put a value in a column family
    pub fn put(&self, cf_name: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let cf = self.get_cf(cf_name)?;
        Ok(self.db.put_cf(&cf, key, value)?)
    }

    /// Delete a value from a column family
    pub fn delete(&self, cf_name: &str, key: &[u8]) -> Result<()> {
        let cf = self.get_cf(cf_name)?;
        Ok(self.db.delete_cf(&cf, key)?)
    }

    /// Check if a key exists
    pub fn contains(&self, cf_name: &str, key: &[u8]) -> Result<bool> {
        Ok(self.get(cf_name, key)?.is_some())
    }

    /// Write a batch of operations atomically
    pub fn write_batch(&self, batch: WriteBatch) -> Result<()> {
        let mut rocks_batch = RocksWriteBatch::default();

        for op in batch.ops() {
            match op {
                BatchOp::Put { cf, key, value } => {
                    let cf_handle = self.get_cf(cf)?;
                    rocks_batch.put_cf(&cf_handle, key, value);
                }
                BatchOp::Delete { cf, key } => {
                    let cf_handle = self.get_cf(cf)?;
                    rocks_batch.delete_cf(&cf_handle, key);
                }
            }
        }

        Ok(self.db.write(rocks_batch)?)
    }

    /// Get an iterator over a column family
    pub fn iter(&self, cf_name: &str) -> Result<impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .map(|r| r.unwrap()))
    }

    /// Get an iterator starting from a key
    pub fn iter_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
    ) -> Result<impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(
                &cf,
                rocksdb::IteratorMode::From(start_key, rocksdb::Direction::Forward),
            )
            .map(|r| r.unwrap()))
    }

    /// Get an iterator in reverse order
    pub fn iter_reverse(
        &self,
        cf_name: &str,
    ) -> Result<impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::End)
            .map(|r| r.unwrap()))
    }

    /// Flush the database
    pub fn flush(&self) -> Result<()> {
        for cf_name in cf::ALL {
            let cf = self.get_cf(cf_name)?;
            self.db.flush_cf(&cf)?;
        }
        Ok(())
    }

    /// Get database path
    pub fn path(&self) -> &Path {
        &self.config.path
    }

    /// Compact the database
    pub fn compact(&self) -> Result<()> {
        for cf_name in cf::ALL {
            let cf = self.get_cf(cf_name)?;
            self.db.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
        }
        Ok(())
    }
}

impl Clone for Database {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            config: self.config.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_database_open() {
        let db = Database::open_temp().unwrap();
        assert!(db.path().exists());
    }

    #[test]
    fn test_put_get() {
        let db = Database::open_temp().unwrap();

        db.put(cf::METADATA, b"test_key", b"test_value").unwrap();
        let value = db.get(cf::METADATA, b"test_key").unwrap();
        assert_eq!(value, Some(b"test_value".to_vec()));
    }

    #[test]
    fn test_delete() {
        let db = Database::open_temp().unwrap();

        db.put(cf::METADATA, b"key", b"value").unwrap();
        assert!(db.contains(cf::METADATA, b"key").unwrap());

        db.delete(cf::METADATA, b"key").unwrap();
        assert!(!db.contains(cf::METADATA, b"key").unwrap());
    }

    #[test]
    fn test_write_batch() {
        let db = Database::open_temp().unwrap();

        let mut batch = WriteBatch::new();
        batch.put(cf::METADATA, b"key1".to_vec(), b"value1".to_vec());
        batch.put(cf::METADATA, b"key2".to_vec(), b"value2".to_vec());

        db.write_batch(batch).unwrap();

        assert_eq!(
            db.get(cf::METADATA, b"key1").unwrap(),
            Some(b"value1".to_vec())
        );
        assert_eq!(
            db.get(cf::METADATA, b"key2").unwrap(),
            Some(b"value2".to_vec())
        );
    }

    #[test]
    fn test_iterator() {
        let db = Database::open_temp().unwrap();

        db.put(cf::METADATA, b"a", b"1").unwrap();
        db.put(cf::METADATA, b"b", b"2").unwrap();
        db.put(cf::METADATA, b"c", b"3").unwrap();

        let items: Vec<_> = db.iter(cf::METADATA).unwrap().collect();
        // Note: db_version is also in METADATA
        assert!(items.len() >= 3);
    }
}
