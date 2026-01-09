//! Per-worker buffer pool management.
//!
//! Provides fixed-size buffer allocation without per-operation malloc overhead.
//! On Linux with io_uring, buffers can be registered with the kernel for
//! faster buffer validation during operations.
//!
//! ## Buffer Chains
//!
//! For large values that exceed a single buffer size, `BufferChain` provides
//! a way to chain multiple buffers together. This keeps memory bounded by the
//! pool size while supporting arbitrarily large values (up to `max_value_size`).

#![allow(dead_code)] // Some methods will be used when features are wired in

use std::borrow::Cow;
use std::io::IoSlice;

/// Per-worker buffer pool with fixed-size buffers.
///
/// Buffers are pre-allocated and reused to avoid allocation overhead
/// on the hot path. The pool tracks which buffers are in use via a free list.
pub struct BufferPool {
    /// Actual buffer storage.
    buffers: Vec<Vec<u8>>,
    /// Stack of available buffer indices (LIFO for cache locality).
    free_list: Vec<usize>,
    /// Size of each buffer.
    buffer_size: usize,
}

impl BufferPool {
    /// Create a new buffer pool.
    ///
    /// # Arguments
    /// * `count` - Number of buffers to pre-allocate
    /// * `size` - Size of each buffer in bytes
    pub fn new(count: usize, size: usize) -> Self {
        let mut buffers = Vec::with_capacity(count);
        let mut free_list = Vec::with_capacity(count);

        for i in 0..count {
            buffers.push(vec![0u8; size]);
            free_list.push(i);
        }

        Self {
            buffers,
            free_list,
            buffer_size: size,
        }
    }

    /// Allocate a buffer from the pool.
    ///
    /// Returns `None` if no buffers are available.
    pub fn alloc(&mut self) -> Option<usize> {
        self.free_list.pop()
    }

    /// Return a buffer to the pool.
    ///
    /// # Panics
    /// Panics if `idx` is out of bounds (debug builds only).
    pub fn free(&mut self, idx: usize) {
        debug_assert!(idx < self.buffers.len(), "buffer index out of bounds");
        self.free_list.push(idx);
    }

    /// Get an immutable reference to a buffer.
    ///
    /// # Panics
    /// Panics if `idx` is out of bounds.
    pub fn get(&self, idx: usize) -> &[u8] {
        &self.buffers[idx]
    }

    /// Get a mutable reference to a buffer.
    ///
    /// # Panics
    /// Panics if `idx` is out of bounds.
    pub fn get_mut(&mut self, idx: usize) -> &mut [u8] {
        &mut self.buffers[idx]
    }

    /// Get a mutable pointer to a buffer for FFI.
    ///
    /// # Safety
    /// Caller must ensure the buffer is not accessed through other references
    /// while the pointer is in use.
    pub fn get_ptr(&mut self, idx: usize) -> *mut u8 {
        self.buffers[idx].as_mut_ptr()
    }

    /// Get the size of each buffer.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Get the total number of buffers.
    pub fn capacity(&self) -> usize {
        self.buffers.len()
    }

    /// Get the number of available buffers.
    pub fn available(&self) -> usize {
        self.free_list.len()
    }

    /// Get raw buffer data for io_uring buffer registration.
    ///
    /// Returns an iterator over (ptr, len) pairs suitable for building iovecs.
    pub fn as_iovecs(&self) -> impl Iterator<Item = (*const u8, usize)> + '_ {
        self.buffers.iter().map(|b| (b.as_ptr(), b.len()))
    }

    /// Allocate multiple buffers at once.
    ///
    /// Returns `None` if not enough buffers are available, leaving the pool unchanged.
    pub fn alloc_many(&mut self, count: usize) -> Option<Vec<usize>> {
        if self.free_list.len() < count {
            return None;
        }
        let mut indices = Vec::with_capacity(count);
        for _ in 0..count {
            indices.push(self.free_list.pop().unwrap());
        }
        Some(indices)
    }

    /// Free multiple buffers at once.
    pub fn free_many(&mut self, indices: impl IntoIterator<Item = usize>) {
        for idx in indices {
            debug_assert!(idx < self.buffers.len(), "buffer index out of bounds");
            self.free_list.push(idx);
        }
    }
}

/// Error returned when buffer chain operations fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// No buffers available in the pool.
    PoolExhausted,
    /// Value exceeds maximum allowed size.
    ValueTooLarge,
}

/// A chain of buffer indices representing data that spans multiple buffers.
///
/// Used for large values that exceed a single buffer size. The chain maintains
/// indices into a `BufferPool` and tracks the total bytes stored.
///
/// # Example
///
/// ```ignore
/// let mut pool = BufferPool::new(100, 64 * 1024);
/// let mut chain = BufferChain::new(pool.buffer_size());
///
/// // Append data, allocating buffers as needed
/// chain.append(large_data, &mut pool)?;
///
/// // Read back as contiguous data (may copy if multi-buffer)
/// let data = chain.as_contiguous(&pool);
///
/// // Release buffers back to pool
/// chain.release(&mut pool);
/// ```
#[derive(Debug)]
pub struct BufferChain {
    /// Buffer indices in order.
    buffers: Vec<usize>,
    /// Total bytes stored (last buffer may be partially filled).
    len: usize,
    /// Size of each buffer (from pool).
    buffer_size: usize,
}

impl BufferChain {
    /// Create a new empty buffer chain.
    pub fn new(buffer_size: usize) -> Self {
        Self {
            buffers: Vec::new(),
            len: 0,
            buffer_size,
        }
    }

    /// Create a chain with a single pre-allocated buffer.
    pub fn with_buffer(buffer_idx: usize, buffer_size: usize) -> Self {
        Self {
            buffers: vec![buffer_idx],
            len: 0,
            buffer_size,
        }
    }

    /// Total bytes stored in the chain.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Check if the chain is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of buffers in the chain.
    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Size of each buffer.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Get the buffer indices (for iteration).
    pub fn buffer_indices(&self) -> &[usize] {
        &self.buffers
    }

    /// Append data to the chain, allocating buffers as needed.
    ///
    /// Returns `Err(ChainError::PoolExhausted)` if the pool runs out of buffers.
    pub fn append(&mut self, data: &[u8], pool: &mut BufferPool) -> Result<(), ChainError> {
        let mut offset = 0;

        while offset < data.len() {
            // Get current buffer or allocate a new one
            let (buf_idx, buf_offset) = self.current_buffer_and_offset(pool)?;
            let buf = pool.get_mut(buf_idx);
            let available = self.buffer_size - buf_offset;
            let to_copy = available.min(data.len() - offset);

            buf[buf_offset..buf_offset + to_copy].copy_from_slice(&data[offset..offset + to_copy]);
            self.len += to_copy;
            offset += to_copy;
        }

        Ok(())
    }

    /// Get or allocate the current buffer for writing.
    ///
    /// Returns (buffer_index, offset_in_buffer).
    fn current_buffer_and_offset(
        &mut self,
        pool: &mut BufferPool,
    ) -> Result<(usize, usize), ChainError> {
        if self.buffers.is_empty() {
            // Need first buffer
            let idx = pool.alloc().ok_or(ChainError::PoolExhausted)?;
            self.buffers.push(idx);
            Ok((idx, 0))
        } else {
            let offset_in_last = self.len % self.buffer_size;
            if offset_in_last == 0 && self.len > 0 {
                // Last buffer is full, need a new one
                let idx = pool.alloc().ok_or(ChainError::PoolExhausted)?;
                self.buffers.push(idx);
                Ok((idx, 0))
            } else {
                // Still room in last buffer
                Ok((*self.buffers.last().unwrap(), offset_in_last))
            }
        }
    }

    /// Get the data as a contiguous slice.
    ///
    /// If the chain spans multiple buffers, this assembles them into a new Vec.
    /// For single-buffer chains, returns a borrowed slice (zero-copy).
    pub fn as_contiguous<'a>(&'a self, pool: &'a BufferPool) -> Cow<'a, [u8]> {
        if self.buffers.is_empty() {
            Cow::Borrowed(&[])
        } else if self.buffers.len() == 1 {
            Cow::Borrowed(&pool.get(self.buffers[0])[..self.len])
        } else {
            Cow::Owned(self.assemble(pool))
        }
    }

    /// Assemble all buffers into a contiguous Vec.
    pub fn assemble(&self, pool: &BufferPool) -> Vec<u8> {
        let mut result = Vec::with_capacity(self.len);
        let mut remaining = self.len;

        for &buf_idx in &self.buffers {
            let buf = pool.get(buf_idx);
            let chunk_len = remaining.min(self.buffer_size);
            result.extend_from_slice(&buf[..chunk_len]);
            remaining -= chunk_len;
        }

        result
    }

    /// Iterate over chunks in the chain.
    ///
    /// Each chunk is a slice of the corresponding buffer, with the last
    /// chunk potentially being smaller than `buffer_size`.
    pub fn chunks<'a>(&'a self, pool: &'a BufferPool) -> impl Iterator<Item = &'a [u8]> + 'a {
        let mut remaining = self.len;
        let buffer_size = self.buffer_size;

        self.buffers.iter().map(move |&buf_idx| {
            let buf = pool.get(buf_idx);
            let chunk_len = remaining.min(buffer_size);
            remaining -= chunk_len;
            &buf[..chunk_len]
        })
    }

    /// Create IoSlice views for scatter-gather I/O.
    ///
    /// Returns slices starting from the given byte offset (for resuming partial writes).
    pub fn io_slices<'a>(
        &'a self,
        pool: &'a BufferPool,
        start_offset: usize,
    ) -> Vec<IoSlice<'a>> {
        if start_offset >= self.len {
            return Vec::new();
        }

        let mut slices = Vec::with_capacity(self.buffers.len());
        let mut skip = start_offset;
        let mut remaining = self.len - start_offset;

        for &buf_idx in &self.buffers {
            let buf = pool.get(buf_idx);
            let chunk_len = remaining.min(self.buffer_size);

            if skip >= chunk_len {
                skip -= chunk_len;
                continue;
            }

            let slice = &buf[skip..skip + chunk_len - skip.min(chunk_len)];
            if !slice.is_empty() {
                slices.push(IoSlice::new(&buf[skip..chunk_len]));
            }
            skip = 0;
            remaining -= chunk_len;
        }

        slices
    }

    /// Clear the chain without releasing buffers.
    ///
    /// Use this to reuse buffers for a new request on the same connection.
    pub fn clear(&mut self) {
        self.len = 0;
        // Keep buffers allocated for reuse
    }

    /// Release all buffers back to the pool and reset the chain.
    pub fn release(&mut self, pool: &mut BufferPool) {
        for idx in self.buffers.drain(..) {
            pool.free(idx);
        }
        self.len = 0;
    }

    /// Take ownership of buffer indices, leaving the chain empty.
    ///
    /// The caller is responsible for freeing these buffers.
    pub fn take_buffers(&mut self) -> Vec<usize> {
        self.len = 0;
        std::mem::take(&mut self.buffers)
    }

    /// Set the length (for when data is written directly to buffers).
    ///
    /// # Safety
    /// Caller must ensure that `len` bytes have actually been written
    /// to the chain's buffers.
    pub fn set_len(&mut self, len: usize) {
        self.len = len;
    }

    /// Add a buffer to the chain (for pre-allocation).
    pub fn push_buffer(&mut self, buf_idx: usize) {
        self.buffers.push(buf_idx);
    }

    /// Get a slice of the first N bytes (for header parsing).
    ///
    /// Returns `None` if the chain has fewer than `n` bytes.
    /// Note: Only works efficiently if `n <= buffer_size` (single buffer).
    pub fn first_n_bytes<'a>(&'a self, n: usize, pool: &'a BufferPool) -> Option<&'a [u8]> {
        if self.len < n {
            return None;
        }
        if n <= self.buffer_size && !self.buffers.is_empty() {
            Some(&pool.get(self.buffers[0])[..n])
        } else {
            None // Would need to span buffers, caller should use as_contiguous
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_pool_basic() {
        let mut pool = BufferPool::new(4, 1024);

        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.available(), 4);
        assert_eq!(pool.buffer_size(), 1024);

        // Allocate all buffers
        let b0 = pool.alloc().unwrap();
        let b1 = pool.alloc().unwrap();
        let b2 = pool.alloc().unwrap();
        let b3 = pool.alloc().unwrap();

        assert_eq!(pool.available(), 0);
        assert!(pool.alloc().is_none());

        // Free and reallocate
        pool.free(b1);
        assert_eq!(pool.available(), 1);

        let b4 = pool.alloc().unwrap();
        assert_eq!(b4, b1); // LIFO reuse

        // Write and read
        pool.get_mut(b0)[0] = 42;
        assert_eq!(pool.get(b0)[0], 42);

        pool.free(b0);
        pool.free(b2);
        pool.free(b3);
        pool.free(b4);
        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn test_buffer_chain_single_buffer() {
        let mut pool = BufferPool::new(4, 1024);
        let mut chain = BufferChain::new(pool.buffer_size());

        // Append small data (fits in one buffer)
        let data = b"hello world";
        chain.append(data, &mut pool).unwrap();

        assert_eq!(chain.len(), 11);
        assert_eq!(chain.buffer_count(), 1);
        assert_eq!(pool.available(), 3);

        // Read back
        let result = chain.as_contiguous(&pool);
        assert_eq!(&*result, data);

        // Should be borrowed (zero-copy)
        assert!(matches!(result, Cow::Borrowed(_)));

        // Release
        chain.release(&mut pool);
        assert_eq!(pool.available(), 4);
        assert_eq!(chain.len(), 0);
    }

    #[test]
    fn test_buffer_chain_multi_buffer() {
        let mut pool = BufferPool::new(10, 100); // Small buffers for testing
        let mut chain = BufferChain::new(pool.buffer_size());

        // Append data that spans 3 buffers (250 bytes)
        let data: Vec<u8> = (0..250u8).collect();
        chain.append(&data, &mut pool).unwrap();

        assert_eq!(chain.len(), 250);
        assert_eq!(chain.buffer_count(), 3); // 100 + 100 + 50
        assert_eq!(pool.available(), 7);

        // Read back
        let result = chain.as_contiguous(&pool);
        assert_eq!(&*result, &data[..]);

        // Should be owned (assembled)
        assert!(matches!(result, Cow::Owned(_)));

        // Test chunks iterator
        let chunks: Vec<_> = chain.chunks(&pool).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 100);
        assert_eq!(chunks[1].len(), 100);
        assert_eq!(chunks[2].len(), 50);

        // Release
        chain.release(&mut pool);
        assert_eq!(pool.available(), 10);
    }

    #[test]
    fn test_buffer_chain_append_incremental() {
        let mut pool = BufferPool::new(10, 100);
        let mut chain = BufferChain::new(pool.buffer_size());

        // Append in multiple small chunks
        chain.append(b"hello ", &mut pool).unwrap();
        chain.append(b"world", &mut pool).unwrap();

        assert_eq!(chain.len(), 11);
        assert_eq!(chain.buffer_count(), 1);

        let result = chain.as_contiguous(&pool);
        assert_eq!(&*result, b"hello world");

        chain.release(&mut pool);
    }

    #[test]
    fn test_buffer_chain_pool_exhausted() {
        let mut pool = BufferPool::new(2, 100);
        let mut chain = BufferChain::new(pool.buffer_size());

        // Fill the pool
        let data = vec![0u8; 200];
        chain.append(&data, &mut pool).unwrap();
        assert_eq!(pool.available(), 0);

        // Try to append more - should fail
        let result = chain.append(&[0u8; 10], &mut pool);
        assert_eq!(result, Err(ChainError::PoolExhausted));

        chain.release(&mut pool);
    }

    #[test]
    fn test_buffer_chain_first_n_bytes() {
        let mut pool = BufferPool::new(4, 100);
        let mut chain = BufferChain::new(pool.buffer_size());

        let data = b"HEADER:value data here";
        chain.append(data, &mut pool).unwrap();

        // Can get first N bytes efficiently
        let header = chain.first_n_bytes(7, &pool).unwrap();
        assert_eq!(header, b"HEADER:");

        // Returns None if not enough data
        assert!(chain.first_n_bytes(100, &pool).is_none());

        chain.release(&mut pool);
    }

    #[test]
    fn test_buffer_chain_clear_reuse() {
        let mut pool = BufferPool::new(4, 100);
        let mut chain = BufferChain::new(pool.buffer_size());

        chain.append(b"first request", &mut pool).unwrap();
        assert_eq!(chain.buffer_count(), 1);
        let first_buf = chain.buffer_indices()[0];

        // Clear without releasing
        chain.clear();
        assert_eq!(chain.len(), 0);
        assert_eq!(chain.buffer_count(), 1); // Buffer still allocated

        // Reuse for next request
        chain.append(b"second request", &mut pool).unwrap();
        assert_eq!(chain.buffer_indices()[0], first_buf); // Same buffer

        chain.release(&mut pool);
    }
}
