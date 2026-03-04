//! Caching and state.
//!
//! We keep two persistent caches:
//! - all currently open streams
//! - a stream segment cache (optional).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use bytes::Bytes;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::media::StreamIndex;

static CACHE: OnceLock<SegmentCache> = OnceLock::new();

/// Initialize the global segment cache.
/// This function should be called once at application startup.
pub fn init_segment_cache(config: SegmentCacheConfig) {
    let _ = CACHE.set(SegmentCache::new(config));
}

/// Retrieve the global cache stats
pub fn segment_cache_stats() -> SegmentCacheStats {
    if let Some(c) = CACHE.get() {
        c.stats()
    } else {
        SegmentCacheStats {
            entry_count: 0,
            total_size_bytes: 0,
            memory_limit_bytes: 0,
            oldest_entry_age_secs: 0,
        }
    }
}

/// Access the global cache internal instance
pub(crate) fn segment_cache() -> Option<&'static SegmentCache> {
    CACHE.get()
}

/// Cache configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentCacheConfig {
    /// Maximum memory usage for segment cache in megabytes
    pub max_memory_mb: usize,

    /// Maximum number of segments to cache
    pub max_segments: usize,

    /// Time-to-live for cached segments in seconds
    pub ttl_secs: u64,

    /// Number of segments to pre-generate ahead (0 = disabled)
    #[serde(default)]
    pub lookahead: usize,
}

impl Default for SegmentCacheConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: 512,
            max_segments: 100, // ~400 seconds of content at 4s/segment
            ttl_secs: 300,     // 5 minutes
            lookahead: 0,      // disabled by default
        }
    }
}

impl SegmentCacheConfig {
    /// Get maximum memory in bytes
    pub fn max_memory_bytes(&self) -> usize {
        self.max_memory_mb * 1024 * 1024
    }
}

/// Cache entry with metadata
#[derive(Debug, Clone)]
pub(crate) struct CacheEntry {
    pub data: Bytes,
    pub created_at: SystemTime,
    pub last_accessed: SystemTime,
    pub access_count: usize,
}

impl CacheEntry {
    pub fn new(data: Bytes) -> Self {
        let now = SystemTime::now();
        Self {
            data,
            created_at: now,
            last_accessed: now,
            access_count: 1,
        }
    }

    pub fn touch(&mut self) {
        self.last_accessed = SystemTime::now();
        self.access_count += 1;
    }

    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().map(|d| d.as_secs()).unwrap_or(0)
    }

    pub fn is_expired(&self, ttl_secs: u64) -> bool {
        self.age_secs() > ttl_secs
    }
}

/// LRU cache for HLS segments
pub struct SegmentCache {
    /// Cache entries (key -> entry)
    entries: DashMap<String, CacheEntry>,
    /// Per-key generation locks for dedup (double-checked locking)
    generation_locks: DashMap<String, Arc<Mutex<()>>>,
    /// Current memory usage in bytes
    memory_bytes: AtomicUsize,
    /// Cache configuration
    config: SegmentCacheConfig,
}

impl SegmentCache {
    /// Create a new segment cache
    pub fn new(config: SegmentCacheConfig) -> Self {
        Self {
            entries: DashMap::new(),
            generation_locks: DashMap::new(),
            memory_bytes: AtomicUsize::new(0),
            config,
        }
    }

    /// Generate cache key from components
    pub fn make_key(stream_id: &str, segment_key: &str) -> String {
        format!("{}:{}", stream_id, segment_key)
    }

    /// Get a cached segment
    pub fn get(&self, stream_id: &str, segment_key: &str) -> Option<Bytes> {
        let key = Self::make_key(stream_id, segment_key);

        if let Some(mut entry) = self.entries.get_mut(&key) {
            entry.touch();
            Some(entry.data.clone())
        } else {
            None
        }
    }

    #[allow(dead_code)]
    pub fn contains(&self, stream_id: &str, segment_key: &str) -> bool {
        let key = Self::make_key(stream_id, segment_key);
        self.entries.contains_key(&key)
    }

    /// Cache a segment
    pub fn insert(&self, stream_id: &str, segment_key: &str, data: Bytes) {
        let key = Self::make_key(stream_id, segment_key);
        let size = data.len();

        // Check memory limit before inserting
        let current = self.memory_bytes.load(Ordering::Relaxed);
        if current + size > self.config.max_memory_bytes() {
            // Evict entries to make room
            self.evict_if_needed(size);
        }

        // Check segment count limit
        if self.entries.len() >= self.config.max_segments {
            self.evict_if_needed(size);
        }

        self.entries.insert(key, CacheEntry::new(data));
        self.memory_bytes.fetch_add(size, Ordering::Relaxed);
    }

    /// Evict entries if needed to make room for new data.
    fn evict_if_needed(&self, needed_size: usize) {
        let target = self.config.max_memory_bytes() / 2;

        // Phase 1: drop expired entries
        self.entries
            .retain(|_, entry| !entry.is_expired(self.config.ttl_secs));

        // Recompute true memory usage
        let true_usage: usize = self.entries.iter().map(|e| e.value().data.len()).sum();
        self.memory_bytes.store(true_usage, Ordering::Relaxed);

        // Phase 2: LRU eviction if still over budget
        if true_usage + needed_size > self.config.max_memory_bytes() {
            let mut candidates: Vec<(SystemTime, String, usize)> = self
                .entries
                .iter()
                .map(|e| {
                    (
                        e.value().last_accessed,
                        e.key().clone(),
                        e.value().data.len(),
                    )
                })
                .collect();

            candidates.sort_unstable_by_key(|(t, _, _)| *t);

            let mut freed = 0usize;
            for (_, key, size) in candidates {
                if freed >= target {
                    break;
                }
                if self.entries.remove(&key).is_some() {
                    freed += size;
                }
            }

            let after: usize = self.entries.iter().map(|e| e.value().data.len()).sum();
            self.memory_bytes.store(after, Ordering::Relaxed);
        }
    }

    /// Clear stream cache
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[allow(dead_code)]
    pub fn memory_usage(&self) -> usize {
        self.memory_bytes.load(Ordering::Relaxed)
    }

    pub fn remove_stream(&self, stream_id: &str) {
        self.entries.retain(|key, _| !key.starts_with(stream_id));
        let usage: usize = self.entries.iter().map(|e| e.value().data.len()).sum();
        self.memory_bytes.store(usage, Ordering::Relaxed);
    }

    /// Get cache statistics
    pub fn stats(&self) -> SegmentCacheStats {
        let mut count = 0;
        let mut total_size = 0;
        let mut oldest_age = 0;

        for entry in self.entries.iter() {
            count += 1;
            total_size += entry.value().data.len();
            let age = entry.value().age_secs();
            if age > oldest_age {
                oldest_age = age;
            }
        }

        SegmentCacheStats {
            entry_count: count,
            total_size_bytes: total_size,
            memory_limit_bytes: self.config.max_memory_bytes(),
            oldest_entry_age_secs: oldest_age,
        }
    }

    /// Acquire a per-key generation lock.
    ///
    /// Returns an `Arc<Mutex<()>>` that callers should lock before generating.
    /// Multiple callers for the same key get the same mutex, enabling
    /// double-checked locking to avoid duplicate generation.
    pub fn acquire_generation_lock(&self, stream_id: &str, segment_key: &str) -> Arc<Mutex<()>> {
        let key = Self::make_key(stream_id, segment_key);
        self.generation_locks
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Remove a generation lock after the segment has been cached.
    pub fn cleanup_generation_lock(&self, stream_id: &str, segment_key: &str) {
        let key = Self::make_key(stream_id, segment_key);
        self.generation_locks.remove(&key);
    }

    /// Get the configured look-ahead count.
    pub fn lookahead(&self) -> usize {
        self.config.lookahead
    }
}

/// Cache statistics
#[derive(Debug)]
pub struct SegmentCacheStats {
    pub entry_count: usize,
    pub total_size_bytes: usize,
    pub memory_limit_bytes: usize,
    pub oldest_entry_age_secs: u64,
}

impl Default for SegmentCache {
    fn default() -> Self {
        Self::new(SegmentCacheConfig::default())
    }
}

pub(crate) static STREAMS_BY_ID: std::sync::OnceLock<
    dashmap::DashMap<String, std::sync::Arc<StreamIndex>>,
> = std::sync::OnceLock::new();

/// Retrieve a tracked media stream by its generated stream ID
pub(crate) fn get_stream_by_id(stream_id: &str) -> Option<std::sync::Arc<StreamIndex>> {
    STREAMS_BY_ID
        .get_or_init(dashmap::DashMap::new)
        .get(stream_id)
        .map(|r| r.value().clone())
}

/// Remove a tracked media stream by its generated stream ID
///
/// Returns true if the stream was found and removed, false otherwise.
pub fn remove_stream_by_id(stream_id: &str) -> bool {
    if let Some(_media) = STREAMS_BY_ID
        .get_or_init(dashmap::DashMap::new)
        .remove(stream_id)
    {
        if let Some(c) = crate::cache::segment_cache() {
            c.remove_stream(stream_id);
        }
        return true;
    }
    false
}

/// Active stream metadata
#[derive(serde::Serialize, Clone, Debug)]
pub struct ActiveStreamInfo {
    pub stream_id: String,
    pub path: String,
    pub duration: f64,
}

/// Fetch a list of active streams
pub fn active_streams() -> Vec<ActiveStreamInfo> {
    STREAMS_BY_ID
        .get_or_init(dashmap::DashMap::new)
        .iter()
        .map(|r| ActiveStreamInfo {
            stream_id: r.value().stream_id.clone(),
            path: r.value().source_path.to_string_lossy().to_string(),
            duration: r.value().duration_secs,
        })
        .collect()
}

/// Remove expired streams from tracking and cache
pub fn cleanup_expired_streams() -> usize {
    const STREAM_TIMEOUT_SECS: u64 = 600; // 10 minutes

    let mut streams_to_remove = Vec::new();

    for entry in STREAMS_BY_ID.get_or_init(dashmap::DashMap::new).iter() {
        if entry.value().time_since_last_access() > STREAM_TIMEOUT_SECS {
            streams_to_remove.push(entry.key().clone());
        }
    }

    let mut count = 0;
    for stream_id in streams_to_remove {
        remove_stream_by_id(&stream_id);
        count += 1;
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    #[test]
    fn test_cache_entry_creation() {
        let data = Bytes::from("test data");
        let entry = CacheEntry::new(data.clone());

        assert_eq!(entry.data, data);
        assert_eq!(entry.access_count, 1);
        assert!(entry.age_secs() < 2);
    }

    #[test]
    fn test_cache_entry_touch() {
        let data = Bytes::from("test");
        let mut entry = CacheEntry::new(data);

        std::thread::sleep(Duration::from_millis(10));
        entry.touch();

        assert_eq!(entry.access_count, 2);
    }

    #[test]
    fn test_cache_insert_get() {
        let cache = SegmentCache::new(SegmentCacheConfig::default());
        let data = Bytes::from("segment data");

        cache.insert("stream1", "video:0", data.clone());

        assert!(cache.contains("stream1", "video:0"));
        assert_eq!(cache.get("stream1", "video:0"), Some(data));
    }

    #[test]
    fn test_cache_miss() {
        let cache = SegmentCache::new(SegmentCacheConfig::default());

        assert!(!cache.contains("stream1", "video:0"));
        assert_eq!(cache.get("stream1", "video:0"), None);
    }

    #[test]
    fn test_cache_remove_stream() {
        let cache = SegmentCache::new(SegmentCacheConfig::default());

        cache.insert("stream1", "video:0", Bytes::from("v0"));
        cache.insert("stream1", "video:1", Bytes::from("v1"));
        cache.insert("stream1", "audio:0", Bytes::from("a0"));
        cache.insert("stream2", "video:0", Bytes::from("v0"));

        cache.remove_stream("stream1");

        assert!(!cache.contains("stream1", "video:0"));
        assert!(!cache.contains("stream1", "video:1"));
        assert!(!cache.contains("stream1", "audio:0"));
        assert!(cache.contains("stream2", "video:0"));
    }

    #[test]
    fn test_cache_stats() {
        let cache = SegmentCache::new(SegmentCacheConfig::default());

        cache.insert("stream1", "video:0", Bytes::from("data"));

        let stats = cache.stats();
        assert_eq!(stats.entry_count, 1);
        assert!(stats.total_size_bytes > 0);
    }

    #[test]
    fn test_cache_make_key() {
        let key = SegmentCache::make_key("abc123", "video:5");
        assert_eq!(key, "abc123:video:5");
    }

    #[test]
    fn test_cache_len_and_empty() {
        let cache = SegmentCache::new(SegmentCacheConfig::default());
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);

        cache.insert("s1", "v:0", Bytes::from("x"));
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);
    }
}
