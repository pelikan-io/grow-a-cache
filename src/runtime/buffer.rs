//! Per-worker buffer pool management.
//!
//! Provides fixed-size buffer allocation without per-operation malloc overhead.
//! On Linux with io_uring, buffers can be registered with the kernel for
//! faster buffer validation during operations.

#![allow(dead_code)] // Some methods will be used when io_uring is wired in

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
}
