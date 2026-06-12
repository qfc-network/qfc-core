//! Database implementation using RocksDB

use crate::batch::{BatchOp, WriteBatch};
use crate::error::{Result, StorageError};
use crate::schema::{cf, meta, DB_VERSION};
use rocksdb::statistics::StatsLevel;
use rocksdb::{
    BlockBasedOptions, BoundColumnFamily, Cache, DBWithThreadMode, MultiThreaded, Options,
    WriteBatch as RocksWriteBatch,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info};

// Re-exported so consumers (e.g. an observability exporter) can query tickers
// and histograms without taking a direct `rocksdb` dependency.
pub use rocksdb::statistics::{Histogram, Ticker};

/// Integer-valued RocksDB property names accepted by
/// [`Database::property_int`] / [`Database::property_int_cf`].
///
/// Unlike statistics tickers, properties are computed on demand from live
/// engine state and are available even when
/// [`StorageConfig::enable_statistics`] is off.
pub mod props {
    /// Approximate size of active + unflushed immutable memtables (bytes).
    pub const CUR_SIZE_ALL_MEM_TABLES: &str = "rocksdb.cur-size-all-mem-tables";
    /// Estimated bytes compaction needs to rewrite to bring the LSM tree back
    /// in shape (the compaction-debt / backlog signal).
    pub const ESTIMATE_PENDING_COMPACTION_BYTES: &str = "rocksdb.estimate-pending-compaction-bytes";
    /// Number of immutable memtables not yet flushed.
    pub const NUM_IMMUTABLE_MEM_TABLE: &str = "rocksdb.num-immutable-mem-table";
    /// Number of SST files at level 0 (write-stall trigger when high).
    pub const NUM_FILES_AT_LEVEL0: &str = "rocksdb.num-files-at-level0";
    /// Memory used by the (shared) block cache, in bytes.
    pub const BLOCK_CACHE_USAGE: &str = "rocksdb.block-cache-usage";
    /// 1 if writes are completely stopped (hard write stall), else 0.
    pub const IS_WRITE_STOPPED: &str = "rocksdb.is-write-stopped";
    /// Current delayed-write rate in bytes/s; 0 means writes are not delayed.
    pub const ACTUAL_DELAYED_WRITE_RATE: &str = "rocksdb.actual-delayed-write-rate";
}

/// Plain-value snapshot of a RocksDB statistics histogram, returned by
/// [`Database::statistics_histogram`]. Values are in the histogram's native
/// unit (microseconds for `*Micros` histograms); `sum`/`count` are cumulative
/// since DB open, so an exporter can publish them as monotonic counters.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HistogramStats {
    pub median: f64,
    pub average: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub sum: u64,
    pub count: u64,
}

type RocksDB = DBWithThreadMode<MultiThreaded>;

/// A key-value pair yielded by database iterators.
pub type KvPair = (Box<[u8]>, Box<[u8]>);

const MIB: usize = 1024 * 1024;

/// Column families whose dominant access pattern is exact-key point lookups
/// (hash-keyed, effectively random). These get a bloom filter (~10 bits/key,
/// ~1% false-positive rate) plus index/filter blocks cached and pinned, so a
/// negative lookup usually costs zero disk reads.
///
/// - `TRANSACTIONS`, `RECEIPTS`: get-by-tx-hash from RPC and chain.
/// - `STATE`: trie nodes fetched by node hash during execution (hottest CF).
/// - `CODE`: contract code by code hash.
/// - `TX_INDEX`, `ETH_TX_INDEX`, `BLOCK_HASH_INDEX`: hash -> location indexes,
///   pure point lookups, frequently queried for keys that do not exist.
/// - `TASKS`: inference tasks fetched by 32-byte task id.
///
/// Deliberately *not* in this list:
/// - `BLOCK_HEADERS`, `BLOCK_BODIES`, `REWARDS`, `CHECKPOINTS`, `WORK_PROOFS`:
///   keyed by dense sequential u64-BE heights/epochs; lookups rarely miss, so
///   bloom filters would be pure overhead.
/// - `UNDELEGATIONS`, `DELEGATIONS`, `MINER_EARNINGS`, `VALIDATORS`: consumed
///   via prefix scans / full iteration (state_db.rs, RPC earnings scan), where
///   whole-key blooms do not help.
/// - `METADATA`: a handful of hot keys, always in memtable or block cache.
const POINT_LOOKUP_CFS: &[&str] = &[
    cf::TRANSACTIONS,
    cf::RECEIPTS,
    cf::STATE,
    cf::CODE,
    cf::TX_INDEX,
    cf::ETH_TX_INDEX,
    cf::BLOCK_HASH_INDEX,
    cf::TASKS,
];

/// Column families that receive the bulk of write traffic during block import
/// and therefore get a larger memtable share (see write-buffer math in
/// [`Database::open`]).
const WRITE_HEAVY_CFS: &[&str] = &[cf::STATE, cf::BLOCK_BODIES, cf::TRANSACTIONS, cf::RECEIPTS];

/// Storage configuration
#[derive(Clone, Debug)]
pub struct StorageConfig {
    /// Path to the database directory
    pub path: std::path::PathBuf,

    /// Block cache size in MB (one cache shared by all column families)
    pub block_cache_size_mb: usize,

    /// Total write buffer (memtable) budget in MB across all column families
    pub write_buffer_size_mb: usize,

    /// Maximum number of open files
    pub max_open_files: i32,

    /// Enable compression
    pub enable_compression: bool,

    /// Create if missing
    pub create_if_missing: bool,

    /// Collect RocksDB statistics (tickers + histograms, excluding detailed
    /// timers). Costs a few percent of throughput; off by default. When
    /// enabled, read them via [`Database::statistics`] /
    /// [`Database::statistics_ticker`].
    pub enable_statistics: bool,
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
            enable_statistics: false,
        }
    }
}

/// Database wrapper
pub struct Database {
    db: Arc<RocksDB>,
    /// DB-level options retained after open: they own the statistics handle
    /// shared with the running DB, which is the only way to read tickers back.
    db_opts: Arc<Options>,
    config: StorageConfig,
    /// For [`Database::open_temp`]: keeps the temp directory alive while any
    /// clone of this database exists, and removes it from disk afterwards.
    /// Declared after `db` so the RocksDB instance is closed before the
    /// directory is deleted.
    temp_dir: Option<Arc<tempfile::TempDir>>,
}

impl Database {
    /// Open the database
    pub fn open(config: StorageConfig) -> Result<Self> {
        info!("Opening database at {:?}", config.path);

        let mut opts = Options::default();
        opts.create_if_missing(config.create_if_missing);
        opts.create_missing_column_families(true);
        opts.set_max_open_files(config.max_open_files);

        // Memtable budget math
        // --------------------
        // `set_db_write_buffer_size` is a *DB-wide* cap: when the aggregate
        // size of all memtables across all CFs reaches it, RocksDB flushes the
        // largest one. Per-CF `write_buffer_size` below is only an upper bound
        // per memtable, so total memtable memory stays <= write_buffer_size_mb
        // (default 64 MB) regardless of CF count (currently 18).
        opts.set_db_write_buffer_size(config.write_buffer_size_mb * MIB);

        if config.enable_compression {
            opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
        }

        if config.enable_statistics {
            opts.enable_statistics();
            opts.set_statistics_level(StatsLevel::ExceptDetailedTimers);
        }

        // One block cache shared by every column family (and the default CF).
        // Total block-cache memory is therefore block_cache_size_mb (default
        // 512 MB), not per-CF. For point-lookup CFs the index and filter
        // blocks are charged to this same cache (and L0 ones pinned), so they
        // are bounded too instead of growing off-heap per open file.
        let cache = Cache::new_lru_cache(config.block_cache_size_mb * MIB);

        let mut base_block_opts = BlockBasedOptions::default();
        base_block_opts.set_block_cache(&cache);
        opts.set_block_based_table_factory(&base_block_opts);

        let mut point_lookup_block_opts = BlockBasedOptions::default();
        point_lookup_block_opts.set_block_cache(&cache);
        // ~10 bits/key full (non-block-based) bloom filter: ~1% false
        // positives, ~1.25 bytes/key.
        point_lookup_block_opts.set_bloom_filter(10.0, false);
        point_lookup_block_opts.set_cache_index_and_filter_blocks(true);
        point_lookup_block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);

        // Per-CF memtable upper bounds. Write-heavy CFs (state trie, block
        // bodies, transactions, receipts) get total/4 each; the long tail gets
        // total/8. With the 64 MB default that is 16 MB / 8 MB. The nominal
        // sum over 18 CFs (4*16 + 14*8 = 176 MB) exceeds the budget on
        // purpose: the DB-wide cap above is what actually bounds memory, while
        // the generous per-CF bound lets whichever CF is hot use a meaningful
        // chunk of the budget before flushing.
        let heavy_buf = ((config.write_buffer_size_mb / 4).max(8)) * MIB;
        let light_buf = ((config.write_buffer_size_mb / 8).max(4)) * MIB;

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
                if POINT_LOOKUP_CFS.contains(name) {
                    cf_opts.set_block_based_table_factory(&point_lookup_block_opts);
                } else {
                    cf_opts.set_block_based_table_factory(&base_block_opts);
                }
                cf_opts.set_write_buffer_size(if WRITE_HEAVY_CFS.contains(name) {
                    heavy_buf
                } else {
                    light_buf
                });
                rocksdb::ColumnFamilyDescriptor::new(*name, cf_opts)
            })
            .collect();

        let db = RocksDB::open_cf_descriptors(&opts, &config.path, cf_descriptors)?;

        let db = Self {
            db: Arc::new(db),
            db_opts: Arc::new(opts),
            config,
            temp_dir: None,
        };

        // Initialize metadata if needed
        db.init_metadata()?;

        Ok(db)
    }

    /// Open a temporary database for testing. The backing directory is
    /// removed from disk when the last clone of the returned database drops.
    pub fn open_temp() -> Result<Self> {
        let dir = tempfile::tempdir().map_err(StorageError::Io)?;
        let config = StorageConfig {
            path: dir.path().to_path_buf(),
            create_if_missing: true,
            ..Default::default()
        };
        let mut db = Self::open(config)?;
        db.temp_dir = Some(Arc::new(dir));
        Ok(db)
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

    /// Check if a key exists.
    ///
    /// Uses `key_may_exist_cf` first: on bloom-filtered CFs a definite
    /// negative is answered from the filter/memtable without touching data
    /// blocks. A "maybe" is confirmed with a pinned get (no value copy).
    pub fn contains(&self, cf_name: &str, key: &[u8]) -> Result<bool> {
        let cf = self.get_cf(cf_name)?;
        if !self.db.key_may_exist_cf(&cf, key) {
            return Ok(false);
        }
        Ok(self.db.get_pinned_cf(&cf, key)?.is_some())
    }

    /// Write a batch of operations atomically
    pub fn write_batch(&self, batch: WriteBatch) -> Result<()> {
        let mut rocks_batch = RocksWriteBatch::default();

        // Batches routinely contain many ops against the same few CFs (e.g.
        // a block commit writing hundreds of state nodes). Resolve each CF
        // handle once instead of taking the cf_handle lock per op.
        let mut handles: HashMap<&str, Arc<BoundColumnFamily<'_>>> = HashMap::new();

        for op in batch.ops() {
            let cf_name = match op {
                BatchOp::Put { cf, .. } | BatchOp::Delete { cf, .. } => cf.as_str(),
            };
            if !handles.contains_key(cf_name) {
                handles.insert(cf_name, self.get_cf(cf_name)?);
            }
            let handle = &handles[cf_name];
            match op {
                BatchOp::Put { key, value, .. } => rocks_batch.put_cf(handle, key, value),
                BatchOp::Delete { key, .. } => rocks_batch.delete_cf(handle, key),
            }
        }

        Ok(self.db.write(rocks_batch)?)
    }

    /// Write a batch of operations atomically with `WriteOptions::set_sync(true)`.
    ///
    /// The RocksDB WAL is fsynced before this returns, making the batch — and,
    /// because the WAL is sequential, every previously written batch — durable
    /// against power loss and kernel panic, not just process crash.
    ///
    /// Intended for canonical block commits; see
    /// `docs/adr/0001-block-commit-durability.md` for the durability policy.
    pub fn write_batch_sync(&self, batch: WriteBatch) -> Result<()> {
        let mut rocks_batch = RocksWriteBatch::default();

        // Same per-batch CF handle caching as `write_batch`.
        let mut handles: HashMap<&str, Arc<BoundColumnFamily<'_>>> = HashMap::new();

        for op in batch.ops() {
            let cf_name = match op {
                BatchOp::Put { cf, .. } | BatchOp::Delete { cf, .. } => cf.as_str(),
            };
            if !handles.contains_key(cf_name) {
                handles.insert(cf_name, self.get_cf(cf_name)?);
            }
            let handle = &handles[cf_name];
            match op {
                BatchOp::Put { key, value, .. } => rocks_batch.put_cf(handle, key, value),
                BatchOp::Delete { key, .. } => rocks_batch.delete_cf(handle, key),
            }
        }

        let mut write_opts = rocksdb::WriteOptions::default();
        write_opts.set_sync(true);
        Ok(self.db.write_opt(rocks_batch, &write_opts)?)
    }

    /// Get an iterator over a column family.
    ///
    /// NOTE: panics if RocksDB reports an error mid-iteration (e.g. a corrupt
    /// SST). Prefer [`Database::try_iter`], which propagates the error;
    /// this method is kept for existing callers and will be migrated.
    pub fn iter(&self, cf_name: &str) -> Result<impl Iterator<Item = KvPair> + '_> {
        Ok(self
            .try_iter(cf_name)?
            .map(|r| r.expect("RocksDB iterator error (corrupt SST?)")))
    }

    /// Get an iterator starting from a key.
    ///
    /// NOTE: panics on mid-iteration RocksDB errors; prefer
    /// [`Database::try_iter_from`].
    pub fn iter_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
    ) -> Result<impl Iterator<Item = KvPair> + '_> {
        Ok(self
            .try_iter_from(cf_name, start_key)?
            .map(|r| r.expect("RocksDB iterator error (corrupt SST?)")))
    }

    /// Get an iterator in reverse order.
    ///
    /// NOTE: panics on mid-iteration RocksDB errors; prefer
    /// [`Database::try_iter_reverse`].
    pub fn iter_reverse(&self, cf_name: &str) -> Result<impl Iterator<Item = KvPair> + '_> {
        Ok(self
            .try_iter_reverse(cf_name)?
            .map(|r| r.expect("RocksDB iterator error (corrupt SST?)")))
    }

    /// Get an iterator over a column family, propagating iteration errors
    /// (e.g. corrupt SST / checksum mismatch) instead of panicking.
    pub fn try_iter(&self, cf_name: &str) -> Result<impl Iterator<Item = Result<KvPair>> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::Start)
            .map(|r| r.map_err(StorageError::from)))
    }

    /// Get an iterator starting from a key, propagating iteration errors.
    pub fn try_iter_from(
        &self,
        cf_name: &str,
        start_key: &[u8],
    ) -> Result<impl Iterator<Item = Result<KvPair>> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(
                &cf,
                rocksdb::IteratorMode::From(start_key, rocksdb::Direction::Forward),
            )
            .map(|r| r.map_err(StorageError::from)))
    }

    /// Get an iterator in reverse order, propagating iteration errors.
    pub fn try_iter_reverse(
        &self,
        cf_name: &str,
    ) -> Result<impl Iterator<Item = Result<KvPair>> + '_> {
        let cf = self.get_cf(cf_name)?;
        Ok(self
            .db
            .iterator_cf(&cf, rocksdb::IteratorMode::End)
            .map(|r| r.map_err(StorageError::from)))
    }

    /// Whether RocksDB statistics collection is enabled for this database.
    pub fn statistics_enabled(&self) -> bool {
        self.config.enable_statistics
    }

    /// Full RocksDB statistics dump (tickers + histograms) as a string.
    ///
    /// Returns `None` unless [`StorageConfig::enable_statistics`] was set when
    /// the database was opened.
    pub fn statistics(&self) -> Option<String> {
        if !self.config.enable_statistics {
            return None;
        }
        self.db_opts.get_statistics()
    }

    /// Cumulative value of a single statistics ticker (e.g.
    /// [`Ticker::BlockCacheHit`]).
    ///
    /// Returns `None` unless statistics collection is enabled.
    pub fn statistics_ticker(&self, ticker: Ticker) -> Option<u64> {
        if !self.config.enable_statistics {
            return None;
        }
        Some(self.db_opts.get_ticker_count(ticker))
    }

    /// Snapshot of a statistics histogram (e.g.
    /// [`Histogram::WalFileSyncMicros`]) as plain values.
    ///
    /// Returns `None` unless statistics collection is enabled.
    pub fn statistics_histogram(&self, histogram: Histogram) -> Option<HistogramStats> {
        if !self.config.enable_statistics {
            return None;
        }
        let data = self.db_opts.get_histogram_data(histogram);
        Some(HistogramStats {
            median: data.median(),
            average: data.average(),
            p95: data.p95(),
            p99: data.p99(),
            max: data.max(),
            sum: data.sum(),
            count: data.count(),
        })
    }

    /// Integer-valued DB-level property (e.g. [`props::BLOCK_CACHE_USAGE`]).
    ///
    /// Computed on demand from live engine state; works regardless of
    /// [`StorageConfig::enable_statistics`]. Returns `Ok(None)` if the
    /// property does not exist or is not integer-valued.
    pub fn property_int(&self, name: &str) -> Result<Option<u64>> {
        Ok(self.db.property_int_value(name)?)
    }

    /// Integer-valued per-column-family property (e.g.
    /// [`props::CUR_SIZE_ALL_MEM_TABLES`]).
    ///
    /// Computed on demand; works regardless of
    /// [`StorageConfig::enable_statistics`].
    pub fn property_int_cf(&self, cf_name: &str, name: &str) -> Result<Option<u64>> {
        let cf = self.get_cf(cf_name)?;
        Ok(self.db.property_int_value_cf(&cf, name)?)
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

    /// Crate-internal handle to the underlying RocksDB instance (used by the
    /// snapshot module to create file-level checkpoints).
    pub(crate) fn rocksdb(&self) -> &DBWithThreadMode<MultiThreaded> {
        &self.db
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
            db_opts: Arc::clone(&self.db_opts),
            config: self.config.clone(),
            temp_dir: self.temp_dir.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_temp_with(f: impl FnOnce(&mut StorageConfig)) -> (Database, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = StorageConfig {
            path: dir.path().to_path_buf(),
            create_if_missing: true,
            ..Default::default()
        };
        f(&mut config);
        (Database::open(config).unwrap(), dir)
    }

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
    fn test_contains_on_bloom_filtered_cf() {
        // cf::STATE has a bloom filter + key_may_exist fast path; verify
        // correctness for present, absent, and flushed (SST-resident) keys.
        let db = Database::open_temp().unwrap();

        db.put(cf::STATE, b"present", b"v").unwrap();
        assert!(db.contains(cf::STATE, b"present").unwrap());
        assert!(!db.contains(cf::STATE, b"absent").unwrap());

        db.flush().unwrap();
        assert!(db.contains(cf::STATE, b"present").unwrap());
        assert!(!db.contains(cf::STATE, b"absent").unwrap());

        db.delete(cf::STATE, b"present").unwrap();
        assert!(!db.contains(cf::STATE, b"present").unwrap());
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
    fn test_write_batch_multi_cf() {
        // Exercises the per-batch CF handle cache across several CFs.
        let db = Database::open_temp().unwrap();

        let mut batch = WriteBatch::new();
        batch.put(cf::STATE, b"s1".to_vec(), b"v1".to_vec());
        batch.put(cf::TRANSACTIONS, b"t1".to_vec(), b"v2".to_vec());
        batch.put(cf::STATE, b"s2".to_vec(), b"v3".to_vec());
        batch.delete(cf::STATE, b"s1".to_vec());

        db.write_batch(batch).unwrap();

        assert_eq!(db.get(cf::STATE, b"s1").unwrap(), None);
        assert_eq!(db.get(cf::STATE, b"s2").unwrap(), Some(b"v3".to_vec()));
        assert_eq!(
            db.get(cf::TRANSACTIONS, b"t1").unwrap(),
            Some(b"v2".to_vec())
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

    #[test]
    fn test_try_iter() {
        let db = Database::open_temp().unwrap();

        db.put(cf::STATE, b"a", b"1").unwrap();
        db.put(cf::STATE, b"b", b"2").unwrap();

        let items: Vec<_> = db
            .try_iter(cf::STATE)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(items.len(), 2);

        let from_b: Vec<_> = db
            .try_iter_from(cf::STATE, b"b")
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(from_b.len(), 1);

        let rev: Vec<_> = db
            .try_iter_reverse(cf::STATE)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rev.first().unwrap().0.as_ref(), b"b");
    }

    #[test]
    fn test_statistics_disabled_by_default() {
        let db = Database::open_temp().unwrap();
        assert!(!db.statistics_enabled());
        assert!(db.statistics().is_none());
        assert!(db.statistics_ticker(Ticker::BytesWritten).is_none());
    }

    #[test]
    fn test_statistics_enabled() {
        let (db, _dir) = open_temp_with(|c| c.enable_statistics = true);
        assert!(db.statistics_enabled());

        db.put(cf::STATE, b"k", b"some value bytes").unwrap();
        let _ = db.get(cf::STATE, b"k").unwrap();

        let stats = db.statistics().expect("stats string when enabled");
        assert!(stats.contains("rocksdb"));

        let written = db
            .statistics_ticker(Ticker::BytesWritten)
            .expect("ticker when enabled");
        assert!(written > 0, "BytesWritten should count the put");
    }

    #[test]
    fn test_statistics_histogram() {
        // Disabled by default…
        let db = Database::open_temp().unwrap();
        assert!(db
            .statistics_histogram(Histogram::WalFileSyncMicros)
            .is_none());
        drop(db);

        // …and populated by a sync (WAL-fsync) write when enabled.
        let (db, _dir) = open_temp_with(|c| c.enable_statistics = true);
        let mut batch = WriteBatch::new();
        batch.put(cf::METADATA, b"k".to_vec(), b"v".to_vec());
        db.write_batch_sync(batch).unwrap();

        let h = db
            .statistics_histogram(Histogram::WalFileSyncMicros)
            .expect("histogram when enabled");
        assert!(h.count >= 1, "sync write should record a WAL fsync");
        assert!(h.sum > 0);
    }

    #[test]
    fn test_property_int_works_without_statistics() {
        // Properties are live engine state, independent of enable_statistics.
        let db = Database::open_temp().unwrap();
        assert!(!db.statistics_enabled());

        db.put(cf::STATE, b"key", b"value").unwrap();

        let memtable = db
            .property_int_cf(cf::STATE, props::CUR_SIZE_ALL_MEM_TABLES)
            .unwrap()
            .expect("memtable size property");
        assert!(memtable > 0, "memtable should be non-empty after a put");

        for name in [
            props::ESTIMATE_PENDING_COMPACTION_BYTES,
            props::NUM_IMMUTABLE_MEM_TABLE,
            props::NUM_FILES_AT_LEVEL0,
        ] {
            assert!(
                db.property_int_cf(cf::STATE, name).unwrap().is_some(),
                "per-CF property `{name}` should be integer-valued"
            );
        }

        for name in [
            props::BLOCK_CACHE_USAGE,
            props::IS_WRITE_STOPPED,
            props::ACTUAL_DELAYED_WRITE_RATE,
        ] {
            assert!(
                db.property_int(name).unwrap().is_some(),
                "DB property `{name}` should be integer-valued"
            );
        }

        // Unknown property: Ok(None), not an error.
        assert!(db.property_int("rocksdb.does-not-exist").unwrap().is_none());
    }

    #[test]
    fn test_property_int_cf_unknown_cf() {
        let db = Database::open_temp().unwrap();
        assert!(db
            .property_int_cf("no_such_cf", props::CUR_SIZE_ALL_MEM_TABLES)
            .is_err());
    }

    #[test]
    fn test_open_temp_cleans_up_directory() {
        let db = Database::open_temp().unwrap();
        let path = db.path().to_path_buf();
        assert!(path.exists());

        let clone = db.clone();
        drop(db);
        // Still alive while a clone exists
        assert!(path.exists());

        drop(clone);
        assert!(!path.exists(), "temp dir should be removed on last drop");
    }
}
