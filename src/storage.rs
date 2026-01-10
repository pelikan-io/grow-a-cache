//! In-memory storage module with expiration and memory limits.
//!
//! Provides a thread-safe key-value store with:
//! - Automatic expiration of items
//! - Memory usage tracking and capping
//! - LRU eviction when memory limit is reached
//! - CAS (compare-and-swap) support

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::{debug, info, trace};

/// A single cached item
#[derive(Debug, Clone)]
pub struct CacheItem {
    /// The stored value
    pub value: Vec<u8>,
    /// Memcached flags (opaque 32-bit value stored with item)
    pub flags: u32,
    /// Absolute expiration time (None = never expires)
    pub expires_at: Option<Instant>,
    /// CAS unique token for compare-and-swap operations
    pub cas_unique: u64,
    /// Last access time for LRU eviction
    pub last_accessed: Instant,
}

impl CacheItem {
    /// Calculate the approximate memory usage of this item
    pub fn memory_size(&self) -> usize {
        std::mem::size_of::<Self>() + self.value.len()
    }

    /// Check if this item has expired
    pub fn is_expired(&self) -> bool {
        if let Some(expires_at) = self.expires_at {
            Instant::now() >= expires_at
        } else {
            false
        }
    }
}

/// Result of a storage operation
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum StorageResult {
    /// Operation succeeded
    Stored,
    /// Item was not stored (e.g., add on existing key)
    NotStored,
    /// Item exists (for add/replace checks)
    Exists,
    /// Item not found
    NotFound,
    /// CAS mismatch - item was modified since last fetch
    CasMismatch,
    /// Successfully deleted
    Deleted,
}

/// Thread-safe in-memory cache storage
pub struct Storage {
    /// The actual storage
    data: RwLock<HashMap<String, CacheItem>>,
    /// Current memory usage in bytes
    memory_used: AtomicU64,
    /// Maximum memory allowed
    max_memory: usize,
    /// Default TTL in seconds (0 = no expiration)
    default_ttl: u64,
    /// CAS unique counter
    cas_counter: AtomicU64,
    /// Access order for LRU (key -> access sequence number)
    access_order: RwLock<HashMap<String, u64>>,
    /// Access sequence counter
    access_counter: AtomicU64,
}

impl Storage {
    /// Create a new storage instance
    pub fn new(max_memory: usize, default_ttl: u64) -> Arc<Self> {
        info!(
            max_memory_mb = max_memory / 1024 / 1024,
            default_ttl, "Initializing storage"
        );
        Arc::new(Self {
            data: RwLock::new(HashMap::new()),
            memory_used: AtomicU64::new(0),
            max_memory,
            default_ttl,
            cas_counter: AtomicU64::new(1),
            access_order: RwLock::new(HashMap::new()),
            access_counter: AtomicU64::new(0),
        })
    }

    /// Generate a new CAS unique token
    fn next_cas_unique(&self) -> u64 {
        self.cas_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Record an access to a key for LRU tracking
    fn record_access(&self, key: &str) {
        let seq = self.access_counter.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut order) = self.access_order.write() {
            order.insert(key.to_string(), seq);
        }
    }

    /// Calculate expiration time from TTL
    fn calculate_expiry(&self, ttl: u64) -> Option<Instant> {
        let effective_ttl = if ttl == 0 { self.default_ttl } else { ttl };
        if effective_ttl == 0 {
            None
        } else {
            // Memcached treats values > 30 days as Unix timestamps
            // For simplicity, we treat all values as relative seconds
            Some(Instant::now() + Duration::from_secs(effective_ttl))
        }
    }

    /// Get an item from storage
    pub fn get(&self, key: &str) -> Option<CacheItem> {
        let data = self.data.read().ok()?;
        if let Some(item) = data.get(key) {
            if item.is_expired() {
                trace!(key, "Item expired on access");
                drop(data);
                self.delete(key);
                return None;
            }
            self.record_access(key);
            Some(item.clone())
        } else {
            None
        }
    }

    /// Get multiple items from storage
    pub fn get_multi(&self, keys: &[&str]) -> Vec<(String, CacheItem)> {
        let data = self.data.read().unwrap();
        let mut results = Vec::new();
        let mut expired_keys = Vec::new();

        for &key in keys {
            if let Some(item) = data.get(key) {
                if item.is_expired() {
                    expired_keys.push(key.to_string());
                } else {
                    self.record_access(key);
                    results.push((key.to_string(), item.clone()));
                }
            }
        }

        drop(data);

        // Clean up expired items
        for key in expired_keys {
            self.delete(&key);
        }

        results
    }

    /// Set an item in storage
    pub fn set(&self, key: &str, value: Vec<u8>, flags: u32, ttl: u64) -> StorageResult {
        let item = CacheItem {
            value,
            flags,
            expires_at: self.calculate_expiry(ttl),
            cas_unique: self.next_cas_unique(),
            last_accessed: Instant::now(),
        };

        let new_size = item.memory_size() + key.len();

        // Check if we need to evict items
        self.ensure_memory_available(new_size);

        let mut data = self.data.write().unwrap();

        // Account for old item's memory if replacing
        if let Some(old_item) = data.get(key) {
            let old_size = old_item.memory_size() + key.len();
            self.memory_used
                .fetch_sub(old_size as u64, Ordering::SeqCst);
        }

        self.memory_used
            .fetch_add(new_size as u64, Ordering::SeqCst);
        data.insert(key.to_string(), item);
        self.record_access(key);

        trace!(
            key,
            memory_used = self.memory_used.load(Ordering::SeqCst),
            "Item stored"
        );
        StorageResult::Stored
    }

    /// Add an item only if it doesn't exist
    pub fn add(&self, key: &str, value: Vec<u8>, flags: u32, ttl: u64) -> StorageResult {
        // Check if key exists and is not expired
        {
            let data = self.data.read().unwrap();
            if let Some(item) = data.get(key) {
                if !item.is_expired() {
                    return StorageResult::NotStored;
                }
            }
        }

        self.set(key, value, flags, ttl)
    }

    /// Replace an item only if it exists
    pub fn replace(&self, key: &str, value: Vec<u8>, flags: u32, ttl: u64) -> StorageResult {
        // Check if key exists and is not expired
        {
            let data = self.data.read().unwrap();
            match data.get(key) {
                Some(item) if !item.is_expired() => {}
                _ => return StorageResult::NotStored,
            }
        }

        self.set(key, value, flags, ttl)
    }

    /// CAS (compare-and-swap) - update only if CAS token matches
    pub fn cas(
        &self,
        key: &str,
        value: Vec<u8>,
        flags: u32,
        ttl: u64,
        cas_unique: u64,
    ) -> StorageResult {
        let mut data = self.data.write().unwrap();

        match data.get(key) {
            None => StorageResult::NotFound,
            Some(item) if item.is_expired() => {
                // Treat expired items as not found
                let old_size = item.memory_size() + key.len();
                data.remove(key);
                self.memory_used
                    .fetch_sub(old_size as u64, Ordering::SeqCst);
                StorageResult::NotFound
            }
            Some(item) if item.cas_unique != cas_unique => StorageResult::CasMismatch,
            Some(old_item) => {
                let old_size = old_item.memory_size() + key.len();

                let new_item = CacheItem {
                    value,
                    flags,
                    expires_at: self.calculate_expiry(ttl),
                    cas_unique: self.next_cas_unique(),
                    last_accessed: Instant::now(),
                };
                let new_size = new_item.memory_size() + key.len();

                // Update memory tracking
                self.memory_used
                    .fetch_sub(old_size as u64, Ordering::SeqCst);

                // Ensure we have memory (release lock temporarily)
                drop(data);
                self.ensure_memory_available(new_size);
                data = self.data.write().unwrap();

                self.memory_used
                    .fetch_add(new_size as u64, Ordering::SeqCst);
                data.insert(key.to_string(), new_item);
                self.record_access(key);

                StorageResult::Stored
            }
        }
    }

    /// Delete an item from storage
    pub fn delete(&self, key: &str) -> StorageResult {
        let mut data = self.data.write().unwrap();
        if let Some(item) = data.remove(key) {
            let size = item.memory_size() + key.len();
            self.memory_used.fetch_sub(size as u64, Ordering::SeqCst);
            if let Ok(mut order) = self.access_order.write() {
                order.remove(key);
            }
            trace!(key, "Item deleted");
            StorageResult::Deleted
        } else {
            StorageResult::NotFound
        }
    }

    /// Append data to an existing item
    pub fn append(&self, key: &str, data_to_append: &[u8]) -> StorageResult {
        let mut data = self.data.write().unwrap();

        match data.get_mut(key) {
            None => StorageResult::NotStored,
            Some(item) if item.is_expired() => {
                let old_size = item.memory_size() + key.len();
                data.remove(key);
                self.memory_used
                    .fetch_sub(old_size as u64, Ordering::SeqCst);
                StorageResult::NotStored
            }
            Some(item) => {
                let additional_size = data_to_append.len();

                // Check memory limit
                let current_used = self.memory_used.load(Ordering::SeqCst) as usize;
                if current_used + additional_size > self.max_memory {
                    drop(data);
                    self.ensure_memory_available(additional_size);
                    data = self.data.write().unwrap();

                    // Re-check if item still exists
                    match data.get_mut(key) {
                        Some(item) if !item.is_expired() => {
                            item.value.extend_from_slice(data_to_append);
                            item.cas_unique = self.next_cas_unique();
                            item.last_accessed = Instant::now();
                            self.memory_used
                                .fetch_add(additional_size as u64, Ordering::SeqCst);
                            self.record_access(key);
                            StorageResult::Stored
                        }
                        _ => StorageResult::NotStored,
                    }
                } else {
                    item.value.extend_from_slice(data_to_append);
                    item.cas_unique = self.next_cas_unique();
                    item.last_accessed = Instant::now();
                    self.memory_used
                        .fetch_add(additional_size as u64, Ordering::SeqCst);
                    self.record_access(key);
                    StorageResult::Stored
                }
            }
        }
    }

    /// Prepend data to an existing item
    pub fn prepend(&self, key: &str, data_to_prepend: &[u8]) -> StorageResult {
        let mut data = self.data.write().unwrap();

        match data.get_mut(key) {
            None => StorageResult::NotStored,
            Some(item) if item.is_expired() => {
                let old_size = item.memory_size() + key.len();
                data.remove(key);
                self.memory_used
                    .fetch_sub(old_size as u64, Ordering::SeqCst);
                StorageResult::NotStored
            }
            Some(item) => {
                let additional_size = data_to_prepend.len();

                // Check memory limit
                let current_used = self.memory_used.load(Ordering::SeqCst) as usize;
                if current_used + additional_size > self.max_memory {
                    drop(data);
                    self.ensure_memory_available(additional_size);
                    data = self.data.write().unwrap();

                    // Re-check if item still exists
                    match data.get_mut(key) {
                        Some(item) if !item.is_expired() => {
                            let mut new_value = data_to_prepend.to_vec();
                            new_value.extend_from_slice(&item.value);
                            item.value = new_value;
                            item.cas_unique = self.next_cas_unique();
                            item.last_accessed = Instant::now();
                            self.memory_used
                                .fetch_add(additional_size as u64, Ordering::SeqCst);
                            self.record_access(key);
                            StorageResult::Stored
                        }
                        _ => StorageResult::NotStored,
                    }
                } else {
                    let mut new_value = data_to_prepend.to_vec();
                    new_value.extend_from_slice(&item.value);
                    item.value = new_value;
                    item.cas_unique = self.next_cas_unique();
                    item.last_accessed = Instant::now();
                    self.memory_used
                        .fetch_add(additional_size as u64, Ordering::SeqCst);
                    self.record_access(key);
                    StorageResult::Stored
                }
            }
        }
    }

    /// Ensure enough memory is available, evicting LRU items if necessary
    fn ensure_memory_available(&self, needed: usize) {
        let mut current = self.memory_used.load(Ordering::SeqCst) as usize;

        while current + needed > self.max_memory {
            if let Some(key_to_evict) = self.find_lru_key() {
                debug!(key = %key_to_evict, "Evicting LRU item");
                self.delete(&key_to_evict);
                current = self.memory_used.load(Ordering::SeqCst) as usize;
            } else {
                // No items to evict
                break;
            }
        }
    }

    /// Find the least recently used key
    fn find_lru_key(&self) -> Option<String> {
        let order = self.access_order.read().ok()?;
        let data = self.data.read().ok()?;

        // Find the key with lowest access sequence number
        let mut min_seq = u64::MAX;
        let mut lru_key = None;

        for (key, &seq) in order.iter() {
            // Only consider non-expired items that still exist
            if let Some(item) = data.get(key) {
                if !item.is_expired() && seq < min_seq {
                    min_seq = seq;
                    lru_key = Some(key.clone());
                }
            }
        }

        // If no valid key found in order, pick any key from data
        if lru_key.is_none() {
            lru_key = data.keys().next().cloned();
        }

        lru_key
    }

    /// Remove all expired items from storage.
    /// Currently called lazily on access, but provided for future background cleanup.
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) -> usize {
        let mut expired_keys = Vec::new();

        // First pass: find expired keys
        {
            let data = self.data.read().unwrap();
            for (key, item) in data.iter() {
                if item.is_expired() {
                    expired_keys.push(key.clone());
                }
            }
        }

        // Second pass: remove expired items
        let count = expired_keys.len();
        for key in expired_keys {
            self.delete(&key);
        }

        if count > 0 {
            info!(count, "Cleaned up expired items");
        }

        count
    }

    /// Flush all items from storage
    pub fn flush_all(&self) {
        let mut data = self.data.write().unwrap();
        let mut order = self.access_order.write().unwrap();

        data.clear();
        order.clear();
        self.memory_used.store(0, Ordering::SeqCst);

        info!("Flushed all items");
    }

    /// Get statistics about the storage
    pub fn stats(&self) -> StorageStats {
        let data = self.data.read().unwrap();
        StorageStats {
            item_count: data.len(),
            memory_used: self.memory_used.load(Ordering::SeqCst) as usize,
            max_memory: self.max_memory,
            cas_counter: self.cas_counter.load(Ordering::SeqCst),
        }
    }
}

/// Storage statistics
#[derive(Debug)]
#[allow(dead_code)]
pub struct StorageStats {
    pub item_count: usize,
    pub memory_used: usize,
    pub max_memory: usize,
    pub cas_counter: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_basic_set_get() {
        let storage = Storage::new(1024 * 1024, 0);

        let result = storage.set("key1", b"value1".to_vec(), 0, 0);
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value1");
        assert_eq!(item.flags, 0);
    }

    #[test]
    fn test_get_nonexistent() {
        let storage = Storage::new(1024 * 1024, 0);
        assert!(storage.get("nonexistent").is_none());
    }

    #[test]
    fn test_delete() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);
        assert!(storage.get("key1").is_some());

        let result = storage.delete("key1");
        assert_eq!(result, StorageResult::Deleted);
        assert!(storage.get("key1").is_none());

        let result = storage.delete("key1");
        assert_eq!(result, StorageResult::NotFound);
    }

    #[test]
    fn test_add_existing() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);

        let result = storage.add("key1", b"value2".to_vec(), 0, 0);
        assert_eq!(result, StorageResult::NotStored);

        // Value should remain unchanged
        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value1");
    }

    #[test]
    fn test_add_new() {
        let storage = Storage::new(1024 * 1024, 0);

        let result = storage.add("key1", b"value1".to_vec(), 0, 0);
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value1");
    }

    #[test]
    fn test_replace_existing() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);

        let result = storage.replace("key1", b"value2".to_vec(), 0, 0);
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value2");
    }

    #[test]
    fn test_replace_nonexistent() {
        let storage = Storage::new(1024 * 1024, 0);

        let result = storage.replace("key1", b"value1".to_vec(), 0, 0);
        assert_eq!(result, StorageResult::NotStored);
    }

    #[test]
    fn test_cas_success() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);
        let item = storage.get("key1").unwrap();
        let cas = item.cas_unique;

        let result = storage.cas("key1", b"value2".to_vec(), 0, 0, cas);
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value2");
    }

    #[test]
    fn test_cas_mismatch() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);
        let item = storage.get("key1").unwrap();
        let cas = item.cas_unique;

        // Modify the item
        storage.set("key1", b"value2".to_vec(), 0, 0);

        // Try CAS with old token
        let result = storage.cas("key1", b"value3".to_vec(), 0, 0, cas);
        assert_eq!(result, StorageResult::CasMismatch);

        // Value should remain value2
        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"value2");
    }

    #[test]
    fn test_cas_not_found() {
        let storage = Storage::new(1024 * 1024, 0);

        let result = storage.cas("nonexistent", b"value".to_vec(), 0, 0, 1);
        assert_eq!(result, StorageResult::NotFound);
    }

    #[test]
    fn test_expiration() {
        let storage = Storage::new(1024 * 1024, 0);

        // Set with 1 second TTL
        storage.set("key1", b"value1".to_vec(), 0, 1);

        // Should exist immediately
        assert!(storage.get("key1").is_some());

        // Wait for expiration
        thread::sleep(Duration::from_millis(1100));

        // Should be expired now
        assert!(storage.get("key1").is_none());
    }

    #[test]
    fn test_memory_limit() {
        // Create storage with 500 byte limit
        let storage = Storage::new(500, 0);

        // Add items until we hit the limit
        for i in 0..20 {
            let key = format!("key{i}");
            let value = vec![0u8; 50];
            storage.set(&key, value, 0, 0);
        }

        // Memory should be at or below limit
        let stats = storage.stats();
        assert!(stats.memory_used <= 500);
    }

    #[test]
    fn test_get_multi() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);
        storage.set("key2", b"value2".to_vec(), 0, 0);
        storage.set("key3", b"value3".to_vec(), 0, 0);

        let results = storage.get_multi(&["key1", "key2", "nonexistent"]);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_append() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"Hello".to_vec(), 0, 0);

        let result = storage.append("key1", b" World".as_ref());
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"Hello World");
    }

    #[test]
    fn test_prepend() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"World".to_vec(), 0, 0);

        let result = storage.prepend("key1", b"Hello ".as_ref());
        assert_eq!(result, StorageResult::Stored);

        let item = storage.get("key1").unwrap();
        assert_eq!(item.value, b"Hello World");
    }

    #[test]
    fn test_flush_all() {
        let storage = Storage::new(1024 * 1024, 0);

        storage.set("key1", b"value1".to_vec(), 0, 0);
        storage.set("key2", b"value2".to_vec(), 0, 0);

        storage.flush_all();

        assert!(storage.get("key1").is_none());
        assert!(storage.get("key2").is_none());

        let stats = storage.stats();
        assert_eq!(stats.item_count, 0);
        assert_eq!(stats.memory_used, 0);
    }
}
