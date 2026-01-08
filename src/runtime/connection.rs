//! Connection state machine for managing TCP connections.
//!
//! Separates control plane (connection lifecycle) from data plane (request processing):
//! - Control plane: Accept → Handshake → Established → Closing
//! - Data plane: Reading ↔ Writing (only valid when Established)
//!
//! This separation enables future worker specialization (dedicated accept threads)
//! and TLS handshake support.

use crate::runtime::request::Protocol;
use slab::Slab;
use std::os::unix::io::RawFd;

/// Data plane state: request processing on an established connection.
///
/// Only valid when connection is in `ConnPhase::Established`.
#[derive(Debug, Clone, Copy)]
pub enum DataState {
    /// Waiting for data to be read.
    Reading {
        /// Bytes already read into buffer.
        /// For io_uring with provided buffers, this may be 0 (kernel selects buffer).
        filled: usize,
    },
    /// Writing response data.
    Writing {
        /// Buffer index holding response in write buffer pool.
        buf_idx: usize,
        /// Bytes already written.
        written: usize,
        /// Total bytes to write.
        total: usize,
    },
}

impl DataState {
    /// Create initial reading state.
    pub fn reading() -> Self {
        DataState::Reading { filled: 0 }
    }

    /// Create reading state with accumulated bytes.
    pub fn reading_with(filled: usize) -> Self {
        DataState::Reading { filled }
    }

    /// Create writing state.
    pub fn writing(buf_idx: usize, total: usize) -> Self {
        DataState::Writing {
            buf_idx,
            written: 0,
            total,
        }
    }
}

/// Control plane state: connection lifecycle phases.
#[derive(Debug, Clone, Copy)]
pub enum ConnPhase {
    /// Connection accepted, not yet ready for data.
    /// Future: could be used for initial setup before Established.
    #[allow(dead_code)]
    Accepting,
    /// TLS handshake in progress (future use).
    #[allow(dead_code)]
    Handshaking,
    /// Connection established, processing requests.
    Established(DataState),
    /// Connection is being closed.
    #[allow(dead_code)]
    Closing,
}

impl ConnPhase {
    /// Create an established connection in reading state.
    pub fn established() -> Self {
        ConnPhase::Established(DataState::reading())
    }

    /// Check if connection is established.
    #[allow(dead_code)]
    pub fn is_established(&self) -> bool {
        matches!(self, ConnPhase::Established(_))
    }

    /// Check if connection is closing.
    #[allow(dead_code)]
    pub fn is_closing(&self) -> bool {
        matches!(self, ConnPhase::Closing)
    }

    /// Get the data state if established.
    #[allow(dead_code)]
    pub fn data_state(&self) -> Option<&DataState> {
        match self {
            ConnPhase::Established(state) => Some(state),
            _ => None,
        }
    }

    /// Get mutable data state if established.
    #[allow(dead_code)]
    pub fn data_state_mut(&mut self) -> Option<&mut DataState> {
        match self {
            ConnPhase::Established(state) => Some(state),
            _ => None,
        }
    }
}

/// A single client connection.
#[derive(Debug)]
pub struct Connection {
    /// File descriptor for the socket.
    pub fd: RawFd,
    /// Current connection phase (control plane + data plane state).
    pub phase: ConnPhase,
    /// Protocol configured for this connection.
    pub protocol: Protocol,
}

impl Connection {
    /// Create a new connection in established reading state.
    ///
    /// Most connections transition directly to established after accept.
    pub fn new(fd: RawFd, protocol: Protocol) -> Self {
        Self {
            fd,
            phase: ConnPhase::established(),
            protocol,
        }
    }

    /// Create a new connection in accepting state.
    ///
    /// Use this for connections that need additional setup before being established.
    #[allow(dead_code)]
    pub fn new_accepting(fd: RawFd, protocol: Protocol) -> Self {
        Self {
            fd,
            phase: ConnPhase::Accepting,
            protocol,
        }
    }

    /// Transition to established state (from Accepting or Handshaking).
    #[allow(dead_code)]
    pub fn establish(&mut self) {
        self.phase = ConnPhase::established();
    }

    /// Transition to writing state.
    ///
    /// Panics if not in Established phase.
    pub fn start_writing(&mut self, buf_idx: usize, total: usize) {
        match &mut self.phase {
            ConnPhase::Established(data) => {
                *data = DataState::writing(buf_idx, total);
            }
            _ => panic!("Cannot start writing on non-established connection"),
        }
    }

    /// Transition back to reading state.
    ///
    /// Panics if not in Established phase.
    pub fn start_reading(&mut self) {
        match &mut self.phase {
            ConnPhase::Established(data) => {
                *data = DataState::reading();
            }
            _ => panic!("Cannot start reading on non-established connection"),
        }
    }

    /// Mark connection for closing.
    #[allow(dead_code)]
    pub fn close(&mut self) {
        self.phase = ConnPhase::Closing;
    }

    /// Check if connection is in reading state.
    #[allow(dead_code)]
    pub fn is_reading(&self) -> bool {
        matches!(
            self.phase,
            ConnPhase::Established(DataState::Reading { .. })
        )
    }

    /// Check if connection is in writing state.
    #[allow(dead_code)]
    pub fn is_writing(&self) -> bool {
        matches!(
            self.phase,
            ConnPhase::Established(DataState::Writing { .. })
        )
    }

    /// Get write state details if in writing state.
    #[allow(dead_code)]
    pub fn write_state(&self) -> Option<(usize, usize, usize)> {
        match self.phase {
            ConnPhase::Established(DataState::Writing {
                buf_idx,
                written,
                total,
            }) => Some((buf_idx, written, total)),
            _ => None,
        }
    }

    /// Update write progress.
    ///
    /// Returns true if write is complete.
    #[allow(dead_code)]
    pub fn advance_write(&mut self, bytes: usize) -> bool {
        match &mut self.phase {
            ConnPhase::Established(DataState::Writing { written, total, .. }) => {
                *written += bytes;
                *written >= *total
            }
            _ => false,
        }
    }
}

// Backwards compatibility: re-export ConnPhase as ConnState for gradual migration
// TODO: Remove after all usages are updated
#[allow(unused_imports)]
pub use ConnPhase as ConnState;

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
    #[allow(dead_code)]
    pub fn contains(&self, id: usize) -> bool {
        self.connections.contains(id)
    }

    /// Number of active connections.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Check if there are no connections.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Maximum number of connections allowed.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.max_connections
    }

    /// Iterate over all connections.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Connection)> {
        self.connections.iter()
    }

    /// Iterate over all connections mutably.
    #[allow(dead_code)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut Connection)> {
        self.connections.iter_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_data_state_creation() {
        let reading = DataState::reading();
        assert!(matches!(reading, DataState::Reading { filled: 0 }));

        let reading_with = DataState::reading_with(100);
        assert!(matches!(reading_with, DataState::Reading { filled: 100 }));

        let writing = DataState::writing(5, 200);
        assert!(matches!(
            writing,
            DataState::Writing {
                buf_idx: 5,
                written: 0,
                total: 200
            }
        ));
    }

    #[test]
    fn test_conn_phase_transitions() {
        let mut phase = ConnPhase::Accepting;
        assert!(!phase.is_established());

        phase = ConnPhase::established();
        assert!(phase.is_established());
        assert!(phase.data_state().is_some());

        phase = ConnPhase::Closing;
        assert!(phase.is_closing());
    }

    #[test]
    fn test_connection_state_transitions() {
        let mut conn = Connection::new(42, Protocol::Memcached);

        assert!(conn.is_reading());
        assert!(!conn.is_writing());

        conn.start_writing(1, 100);
        assert!(conn.is_writing());
        assert!(!conn.is_reading());
        assert_eq!(conn.write_state(), Some((1, 0, 100)));

        // Advance write partially
        assert!(!conn.advance_write(50));
        assert_eq!(conn.write_state(), Some((1, 50, 100)));

        // Complete write
        assert!(conn.advance_write(50));

        conn.start_reading();
        assert!(conn.is_reading());

        conn.close();
        assert!(matches!(conn.phase, ConnPhase::Closing));
    }

    #[test]
    fn test_connection_accepting_to_established() {
        let mut conn = Connection::new_accepting(42, Protocol::Resp);
        assert!(matches!(conn.phase, ConnPhase::Accepting));

        conn.establish();
        assert!(conn.is_reading());
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
