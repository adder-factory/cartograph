//! One extraction-cache contract shared while the persistence service is ported.

use cartograph_db::{
    CartographDatabase, NativeDatabase, NativeParseCacheBatchWrite, NativeParseCacheEntry,
    NativeParseCacheError, NativeParseCacheKey, NativeParseCacheRecord, NativeParseCacheWrite,
};
use std::{future::Future, pin::Pin};

type CacheFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, NativeParseCacheError>> + Send + 'a>>;

pub(super) trait CacheStore: Send + Sync {
    fn load_native_parse_cache<'a>(
        &'a self,
        key: &'a NativeParseCacheKey,
    ) -> CacheFuture<'a, Option<NativeParseCacheRecord>>;
    fn load_native_parse_cache_batch<'a>(
        &'a self,
        keys: &'a [NativeParseCacheKey],
    ) -> CacheFuture<'a, Vec<Option<NativeParseCacheRecord>>>;
    fn store_native_parse_cache<'a>(
        &'a self,
        key: &'a NativeParseCacheKey,
        payload: &'a [u8],
    ) -> CacheFuture<'a, NativeParseCacheWrite>;
    fn store_native_parse_cache_batch(
        &self,
        entries: Vec<NativeParseCacheEntry>,
    ) -> CacheFuture<'_, NativeParseCacheBatchWrite>;
    fn evict_native_parse_cache<'a>(
        &'a self,
        key: &'a NativeParseCacheKey,
    ) -> CacheFuture<'a, bool>;
}

macro_rules! cache_store {
    ($database:ty) => {
        impl CacheStore for $database {
            fn load_native_parse_cache<'a>(
                &'a self,
                key: &'a NativeParseCacheKey,
            ) -> CacheFuture<'a, Option<NativeParseCacheRecord>> {
                Box::pin(<$database>::load_native_parse_cache(self, key))
            }
            fn load_native_parse_cache_batch<'a>(
                &'a self,
                keys: &'a [NativeParseCacheKey],
            ) -> CacheFuture<'a, Vec<Option<NativeParseCacheRecord>>> {
                Box::pin(<$database>::load_native_parse_cache_batch(self, keys))
            }
            fn store_native_parse_cache<'a>(
                &'a self,
                key: &'a NativeParseCacheKey,
                payload: &'a [u8],
            ) -> CacheFuture<'a, NativeParseCacheWrite> {
                Box::pin(<$database>::store_native_parse_cache(self, key, payload))
            }
            fn store_native_parse_cache_batch(
                &self,
                entries: Vec<NativeParseCacheEntry>,
            ) -> CacheFuture<'_, NativeParseCacheBatchWrite> {
                Box::pin(<$database>::store_native_parse_cache_batch(self, entries))
            }
            fn evict_native_parse_cache<'a>(
                &'a self,
                key: &'a NativeParseCacheKey,
            ) -> CacheFuture<'a, bool> {
                Box::pin(<$database>::evict_native_parse_cache(self, key))
            }
        }
    };
}

cache_store!(CartographDatabase);
cache_store!(NativeDatabase);
