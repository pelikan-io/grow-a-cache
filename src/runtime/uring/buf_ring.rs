//! Provided buffer ring for io_uring.
//!
//! Uses the kernel's buffer selection mechanism (kernel 5.19+) for efficient
//! buffer management. The kernel selects buffers from the ring automatically,
//! eliminating the need to pre-assign buffers to connections.

#![allow(dead_code)] // Some methods reserved for future optimizations

use io_uring::types::BufRingEntry;
use io_uring::IoUring;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::io;
use std::sync::atomic::{AtomicU16, Ordering};

/// Buffer group ID for read operations.
pub const READ_BGID: u16 = 0;

/// A provided buffer ring registered with io_uring.
///
/// The ring header occupies the first entry, with actual buffers starting at index 1.
/// Buffers are recycled by re-adding them to the ring after use.
pub struct BufRing {
    /// Pointer to the ring memory (ring entries followed by buffer data).
    ring_ptr: *mut BufRingEntry,
    /// Pointer to the buffer data area.
    buffers_ptr: *mut u8,
    /// Layout for deallocation.
    ring_layout: Layout,
    /// Layout for buffer deallocation.
    buffers_layout: Layout,
    /// Number of ring entries (must be power of 2).
    ring_entries: u16,
    /// Size of each buffer.
    buffer_size: usize,
    /// Current tail position (updated atomically).
    tail: AtomicU16,
    /// Buffer group ID.
    bgid: u16,
}

impl BufRing {
    /// Create a new buffer ring.
    ///
    /// # Arguments
    /// * `ring` - The io_uring instance to register with.
    /// * `ring_entries` - Number of ring entries (must be power of 2).
    /// * `buffer_size` - Size of each buffer.
    /// * `bgid` - Buffer group ID.
    pub fn new(
        ring: &IoUring,
        ring_entries: u16,
        buffer_size: usize,
        bgid: u16,
    ) -> io::Result<Self> {
        // Ring entries must be power of 2
        if !ring_entries.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ring_entries must be power of 2",
            ));
        }

        // Allocate ring entries (page-aligned for kernel requirements)
        let ring_size = std::mem::size_of::<BufRingEntry>() * ring_entries as usize;
        let ring_layout = Layout::from_size_align(ring_size, 4096)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let ring_ptr = unsafe { alloc_zeroed(ring_layout) as *mut BufRingEntry };
        if ring_ptr.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "failed to allocate ring",
            ));
        }

        // Allocate buffer space
        let buffers_size = buffer_size * ring_entries as usize;
        let buffers_layout = Layout::from_size_align(buffers_size, 4096)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let buffers_ptr = unsafe { alloc_zeroed(buffers_layout) as *mut u8 };
        if buffers_ptr.is_null() {
            unsafe { dealloc(ring_ptr as *mut u8, ring_layout) };
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "failed to allocate buffers",
            ));
        }

        let buf_ring = Self {
            ring_ptr,
            buffers_ptr,
            ring_layout,
            buffers_layout,
            ring_entries,
            buffer_size,
            tail: AtomicU16::new(0),
            bgid,
        };

        // Initialize all buffer entries in the ring
        for i in 0..ring_entries {
            buf_ring.add_buffer(i);
        }

        // Register the buffer ring with io_uring
        unsafe {
            ring.submitter().register_buf_ring_with_flags(
                ring_ptr as u64,
                ring_entries,
                bgid,
                0,
            )?;
        }

        Ok(buf_ring)
    }

    /// Get the buffer group ID.
    #[inline]
    pub fn bgid(&self) -> u16 {
        self.bgid
    }

    /// Get the size of each buffer.
    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Get a pointer to a buffer by its ID.
    #[inline]
    pub fn get_buffer(&self, bid: u16) -> *mut u8 {
        unsafe { self.buffers_ptr.add(bid as usize * self.buffer_size) }
    }

    /// Get a slice to a buffer by its ID.
    #[inline]
    pub fn get_buffer_slice(&self, bid: u16) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.buffers_ptr.add(bid as usize * self.buffer_size),
                self.buffer_size,
            )
        }
    }

    /// Get a mutable slice to a buffer by its ID.
    #[inline]
    pub fn get_buffer_slice_mut(&mut self, bid: u16) -> &mut [u8] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.buffers_ptr.add(bid as usize * self.buffer_size),
                self.buffer_size,
            )
        }
    }

    /// Return a buffer to the ring for reuse.
    ///
    /// This must be called after processing a buffer received from a completion.
    pub fn recycle_buffer(&self, bid: u16) {
        self.add_buffer(bid);
    }

    /// Add a buffer to the ring at the current tail position.
    fn add_buffer(&self, bid: u16) {
        let tail = self.tail.load(Ordering::Relaxed);
        let idx = tail & (self.ring_entries - 1);

        unsafe {
            let entry = self.ring_ptr.add(idx as usize);
            (*entry).set_addr(self.buffers_ptr.add(bid as usize * self.buffer_size) as u64);
            (*entry).set_len(self.buffer_size as u32);
            (*entry).set_bid(bid);
        }

        // Update tail with release ordering to ensure entry is visible
        let new_tail = tail.wrapping_add(1);
        self.tail.store(new_tail, Ordering::Release);

        // Write tail to the ring header
        unsafe {
            let tail_ptr = BufRingEntry::tail(self.ring_ptr) as *mut u16;
            std::ptr::write_volatile(tail_ptr, new_tail);
        }
    }
}

impl Drop for BufRing {
    fn drop(&mut self) {
        // Note: We should unregister the buffer ring here, but we need the IoUring reference.
        // The caller should ensure unregistration happens before dropping.
        unsafe {
            dealloc(self.buffers_ptr, self.buffers_layout);
            dealloc(self.ring_ptr as *mut u8, self.ring_layout);
        }
    }
}

// Safety: BufRing is thread-local and not shared between threads.
// Each worker has its own BufRing instance.
unsafe impl Send for BufRing {}
