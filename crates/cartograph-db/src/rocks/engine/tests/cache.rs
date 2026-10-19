use super::{TestResult, block_cache};
use rocksdb::{BlockBasedOptions, DB, Options, WriteBatch, statistics::Ticker};

fn large_index_key(ordinal: u32) -> Vec<u8> {
    let mut key = vec![b'x'; 512];
    key[..4].copy_from_slice(&ordinal.to_be_bytes());
    key
}

fn create_existing_monolithic_index(path: &std::path::Path) -> TestResult {
    let mut table = BlockBasedOptions::default();
    // Force a large binary index in a small fixture. The read-side cache must
    // work for already-written indexes, without a compaction or format change.
    table.set_block_size(256);
    table.set_index_shortening(0);
    let mut options = Options::default();
    options.create_if_missing(true);
    options.set_write_buffer_size(32 * 1_048_576);
    options.set_disable_auto_compactions(true);
    options.set_block_based_table_factory(&table);
    let database = DB::open(&options, path)?;
    let mut batch = WriteBatch::default();
    for ordinal in 0..8192 {
        batch.put(large_index_key(ordinal), b"same-value");
    }
    database.write(&batch)?;
    database.flush()?;
    Ok(())
}

#[test]
fn existing_large_index_stays_cached_across_independent_point_reads() -> TestResult {
    let directory = tempfile::tempdir()?;
    create_existing_monolithic_index(directory.path())?;
    let capacity = 64 * 1_048_576;
    let cache = block_cache(capacity);
    let mut table = BlockBasedOptions::default();
    table.set_block_cache(&cache);
    table.set_cache_index_and_filter_blocks(true);
    let mut options = Options::default();
    options.set_block_based_table_factory(&table);
    options.enable_statistics();
    let database = DB::open_for_read_only(&options, directory.path(), false)?;
    let read = |ordinal| -> TestResult {
        let key = large_index_key(ordinal);
        let mut iterator = database.raw_iterator();
        iterator.seek(&key);
        assert_eq!(iterator.key(), Some(key.as_slice()));
        assert_eq!(iterator.value(), Some(b"same-value".as_slice()));
        iterator.status()?;
        Ok(())
    };
    read(0)?;
    assert!(options.get_ticker_count(Ticker::BlockCacheIndexBytesInsert) > 1_048_576);
    let misses = options.get_ticker_count(Ticker::BlockCacheIndexMiss);
    for ordinal in (0..8192).step_by(97).rev() {
        read(ordinal)?;
    }
    assert_eq!(
        options.get_ticker_count(Ticker::BlockCacheIndexMiss),
        misses
    );
    assert!(options.get_ticker_count(Ticker::BlockCacheIndexHit) > 80);
    assert_eq!(cache.get_capacity(), capacity);
    assert!(cache.get_usage() < capacity);
    Ok(())
}
