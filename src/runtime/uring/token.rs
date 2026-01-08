//! Operation token tracking for io_uring completion correlation.
//!
//! Each submitted operation gets a unique token (user_data) that identifies
//! the operation type and associated resources when the completion arrives.

#![allow(dead_code)] // Will be used when io_uring is wired in

use slab::Slab;

/// Type of in-flight operation.
#[derive(Debug, Clone, Copy)]
pub enum OpType {
    /// Accept operation on listener socket.
    Accept,
    /// Read operation on a connection.
    /// Buffer is selected by kernel from provided buffer ring.
    Read {
        /// Connection identifier in the registry.
        conn_id: usize,
    },
    /// Write operation on a connection.
    Write {
        /// Connection identifier in the registry.
        conn_id: usize,
        /// Buffer index in the buffer pool.
        buf_idx: usize,
    },
}

/// Allocator for operation tokens with O(1) lookup.
///
/// Uses a slab to efficiently allocate and deallocate tokens,
/// providing stable identifiers for in-flight operations.
pub struct TokenAllocator {
    ops: Slab<OpType>,
}

impl TokenAllocator {
    /// Create a new token allocator with specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            ops: Slab::with_capacity(capacity),
        }
    }

    /// Allocate a new token for an operation.
    ///
    /// Returns the token (user_data value for io_uring).
    pub fn alloc(&mut self, op: OpType) -> u64 {
        self.ops.insert(op) as u64
    }

    /// Get the operation type for a token.
    ///
    /// Returns None if the token is invalid or already freed.
    pub fn get(&self, token: u64) -> Option<OpType> {
        self.ops.get(token as usize).copied()
    }

    /// Free a token, making it available for reuse.
    ///
    /// Returns the operation type that was associated with the token.
    pub fn free(&mut self, token: u64) -> Option<OpType> {
        let idx = token as usize;
        if self.ops.contains(idx) {
            Some(self.ops.remove(idx))
        } else {
            None
        }
    }

    /// Number of currently allocated tokens.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Check if there are no allocated tokens.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_allocator() {
        let mut alloc = TokenAllocator::new(16);

        let t1 = alloc.alloc(OpType::Accept);
        let t2 = alloc.alloc(OpType::Read { conn_id: 1 });

        assert_eq!(alloc.len(), 2);

        // Verify we can retrieve operations
        assert!(matches!(alloc.get(t1), Some(OpType::Accept)));
        assert!(matches!(alloc.get(t2), Some(OpType::Read { conn_id: 1 })));

        // Free and verify
        alloc.free(t1);
        assert!(alloc.get(t1).is_none());
        assert_eq!(alloc.len(), 1);

        // Allocate reuses slot
        let t3 = alloc.alloc(OpType::Accept);
        assert_eq!(t3, t1); // Slab reuses slots
    }
}
