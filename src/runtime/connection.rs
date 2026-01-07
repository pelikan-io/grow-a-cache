//! Connection state machine for managing TCP connections.
//!
//! Each connection tracks its current state (reading, writing, etc.)
//! and associated resources (buffers, protocol type).

#![allow(dead_code)] // Some types will be used when io_uring is wired in

use crate::runtime::protocol::Protocol;
use slab::Slab;
use std::os::unix::io::RawFd;

/// Current state of a connection.
#[derive(Debug, Clone, Copy)]
pub enum ConnState {
    /// Waiting for data to be read.
    /// Buffer is selected by kernel from provided buffer ring.
    Reading,
    /// Writing response data.
    Writing {
        /// Buffer index holding response in write buffer pool.
        buf_idx: usize,
        /// Bytes already written.
        written: usize,
        /// Total bytes to write.
        total: usize,
    },
    /// Connection is being closed.
    Closing,
}

/// A single client connection.
#[derive(Debug)]
pub struct Connection {
    /// File descriptor for the socket.
    pub fd: RawFd,
    /// Current connection state.
    pub state: ConnState,
    /// Protocol detected/configured for this connection.
    pub protocol: Protocol,
}

impl Connection {
    /// Create a new connection in initial reading state.
    pub fn new(fd: RawFd, protocol: Protocol) -> Self {
        Self {
            fd,
            state: ConnState::Reading,
            protocol,
        }
    }

    /// Transition to writing state.
    pub fn start_writing(&mut self, buf_idx: usize, total: usize) {
        self.state = ConnState::Writing {
            buf_idx,
            written: 0,
            total,
        };
    }

    /// Transition back to reading state.
    pub fn start_reading(&mut self) {
        self.state = ConnState::Reading;
    }

    /// Mark connection for closing.
    pub fn close(&mut self) {
        self.state = ConnState::Closing;
    }
}

/// Registry of active connections using slab allocation.
///
/// Provides O(1) insert, lookup, and remove operations.
pub struct ConnectionRegistry {
    connections: Slab<Connection>,
    max_connections: usize,
}

impl ConnectionRegistry {
    /// Create a new registry with specified maximum capacity.
    pub fn new(max_connections: usize) -> Self {
        Self {
            connections: Slab::with_capacity(max_connections),
            max_connections,
        }
    }

    /// Insert a new connection into the registry.
    ///
    /// Returns `None` if the registry is at capacity.
    pub fn insert(&mut self, conn: Connection) -> Option<usize> {
        if self.connections.len() >= self.max_connections {
            return None;
        }
        Some(self.connections.insert(conn))
    }

    /// Get an immutable reference to a connection.
    pub fn get(&self, id: usize) -> Option<&Connection> {
        self.connections.get(id)
    }

    /// Get a mutable reference to a connection.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Connection> {
        self.connections.get_mut(id)
    }

    /// Remove a connection from the registry.
    pub fn remove(&mut self, id: usize) -> Option<Connection> {
        if self.connections.contains(id) {
            Some(self.connections.remove(id))
        } else {
            None
        }
    }

    /// Check if a connection exists.
    pub fn contains(&self, id: usize) -> bool {
        self.connections.contains(id)
    }

    /// Number of active connections.
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Check if there are no connections.
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Maximum number of connections allowed.
    pub fn capacity(&self) -> usize {
        self.max_connections
    }

    /// Iterate over all connections.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Connection)> {
        self.connections.iter()
    }

    /// Iterate over all connections mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut Connection)> {
        self.connections.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_transitions() {
        let mut conn = Connection::new(42, Protocol::Memcached);

        assert!(matches!(conn.state, ConnState::Reading));

        conn.start_writing(1, 100);
        assert!(matches!(
            conn.state,
            ConnState::Writing {
                buf_idx: 1,
                written: 0,
                total: 100
            }
        ));

        conn.start_reading();
        assert!(matches!(conn.state, ConnState::Reading));

        conn.close();
        assert!(matches!(conn.state, ConnState::Closing));
    }

    #[test]
    fn test_connection_registry() {
        let mut registry = ConnectionRegistry::new(2);

        let c1 = Connection::new(10, Protocol::Memcached);
        let c2 = Connection::new(11, Protocol::Resp);
        let c3 = Connection::new(12, Protocol::Memcached);

        let id1 = registry.insert(c1).unwrap();
        let id2 = registry.insert(c2).unwrap();

        // At capacity
        assert!(registry.insert(c3).is_none());

        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get(id1).unwrap().fd, 10);
        assert_eq!(registry.get(id2).unwrap().protocol, Protocol::Resp);

        registry.remove(id1);
        assert!(!registry.contains(id1));
        assert_eq!(registry.len(), 1);
    }
}
