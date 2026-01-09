//! mio event loop implementation.
//!
//! Readiness-based model: poll tells us when sockets are ready,
//! then we perform non-blocking read/write syscalls.
//! Uses epoll on Linux, kqueue on macOS.
//!
//! ## Large Value Support
//!
//! For values larger than the buffer size, we use `BufferChain` to accumulate
//! data across multiple pool buffers. This keeps memory bounded while supporting
//! values up to `max_value_size`.

use crate::config::Config;
use crate::runtime::request::{process_echo, process_memcached, process_ping, process_resp};
use crate::runtime::{BufferChain, BufferPool, ChainError, DataState, ProcessResult, Protocol};
use crate::storage::Storage;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use slab::Slab;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use tracing::{debug, error, info, warn};

const LISTENER_TOKEN: Token = Token(usize::MAX);

/// Per-worker connection state for mio backend.
///
/// Uses shared `DataState` for read/write state tracking,
/// but wraps the mio `TcpStream` directly.
///
/// For large values, `read_chain` accumulates data beyond the primary buffer,
/// and `write_chain` holds multi-buffer responses for scatter-gather writes.
struct MioConnection {
    stream: TcpStream,
    /// Data plane state (reading/writing)
    data_state: DataState,
    /// Primary read buffer (always allocated)
    read_buf_idx: usize,
    /// Primary write buffer (always allocated)
    write_buf_idx: usize,
    /// Chain for accumulating large reads (beyond primary buffer)
    read_chain: Option<BufferChain>,
    /// Chain for large writes (populated from response data)
    write_chain: Option<BufferChain>,
    protocol: Protocol,
}

/// Run the mio-based server.
pub fn run(config: Config, storage: Arc<Storage>, protocol: Protocol) -> io::Result<()> {
    let num_workers = if config.workers == 0 {
        num_cpus()
    } else {
        config.workers
    };

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    info!(
        workers = num_workers,
        addr = %addr,
        protocol = ?protocol,
        "Starting mio runtime"
    );

    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let config = config.clone();
        let storage = Arc::clone(&storage);

        let handle = thread::Builder::new()
            .name(format!("worker-{worker_id}"))
            .spawn(move || {
                if let Err(e) = worker_loop(worker_id, addr, &config, storage, protocol) {
                    error!(worker = worker_id, error = %e, "Worker failed");
                }
            })?;

        handles.push(handle);
    }

    // Wait for all workers
    for handle in handles {
        let _ = handle.join();
    }

    Ok(())
}

fn worker_loop(
    worker_id: usize,
    addr: SocketAddr,
    config: &Config,
    storage: Arc<Storage>,
    protocol: Protocol,
) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(config.batch_size);

    // Create listener with SO_REUSEPORT for kernel load balancing
    let listener = create_listener_with_reuseport(addr)?;
    let mut listener = TcpListener::from_std(listener);
    poll.registry()
        .register(&mut listener, LISTENER_TOKEN, Interest::READABLE)?;

    let max_connections = config.max_connections;
    let buffer_size = config.buffer_size;
    let max_value_size = config.max_value_size;

    // Buffer pool sizing:
    // - 2 buffers per connection (read + write)
    // - Extra buffers for chains (large values)
    // With 10k connections and 64KB buffers: 10k * 2 = 20k buffers = 1.25GB base
    // Add 50% more for chains: ~1.9GB total per worker
    let pool_size = max_connections * 3;
    let mut buffers = BufferPool::new(pool_size, buffer_size);
    let mut connections: Slab<MioConnection> = Slab::with_capacity(max_connections);

    info!(
        worker = worker_id,
        pool_buffers = pool_size,
        buffer_size,
        max_value_size,
        "Worker started"
    );

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            match event.token() {
                LISTENER_TOKEN => {
                    accept_connections(
                        &listener,
                        &mut poll,
                        &mut connections,
                        &mut buffers,
                        max_connections,
                        worker_id,
                        protocol,
                    )?;
                }
                Token(conn_id) => {
                    if let Err(e) = handle_connection_event(
                        conn_id,
                        event,
                        &mut poll,
                        &mut connections,
                        &mut buffers,
                        &storage,
                        max_value_size,
                    ) {
                        debug!(conn_id, error = %e, "Connection error");
                        close_connection(&mut poll, &mut connections, &mut buffers, conn_id);
                    }
                }
            }
        }
    }
}

fn accept_connections(
    listener: &TcpListener,
    poll: &mut Poll,
    connections: &mut Slab<MioConnection>,
    buffers: &mut BufferPool,
    max_connections: usize,
    worker_id: usize,
    protocol: Protocol,
) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, peer_addr)) => {
                if connections.len() >= max_connections {
                    warn!("Connection limit reached");
                    continue;
                }

                // Allocate read buffer
                let read_buf_idx = match buffers.alloc() {
                    Some(idx) => idx,
                    None => {
                        warn!("Buffer pool exhausted, rejecting connection");
                        continue;
                    }
                };

                // Allocate write buffer
                let write_buf_idx = match buffers.alloc() {
                    Some(idx) => idx,
                    None => {
                        warn!("Buffer pool exhausted, rejecting connection");
                        buffers.free(read_buf_idx);
                        continue;
                    }
                };

                let conn_id = connections.insert(MioConnection {
                    stream,
                    data_state: DataState::reading(),
                    read_buf_idx,
                    write_buf_idx,
                    read_chain: None,
                    write_chain: None,
                    protocol,
                });

                // Re-borrow after insert
                let conn = &mut connections[conn_id];
                poll.registry()
                    .register(&mut conn.stream, Token(conn_id), Interest::READABLE)?;

                debug!(
                    worker = worker_id,
                    conn_id,
                    peer = %peer_addr,
                    "Accepted connection"
                );
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                error!("Accept error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

fn handle_connection_event(
    conn_id: usize,
    event: &mio::event::Event,
    poll: &mut Poll,
    connections: &mut Slab<MioConnection>,
    buffers: &mut BufferPool,
    storage: &Arc<Storage>,
    max_value_size: usize,
) -> io::Result<()> {
    if !connections.contains(conn_id) {
        return Ok(());
    }

    if event.is_readable() {
        handle_readable(conn_id, poll, connections, buffers, storage, max_value_size)?;
    }

    // Re-check connection exists (may have been removed)
    if !connections.contains(conn_id) {
        return Ok(());
    }

    if event.is_writable() {
        handle_writable(conn_id, poll, connections, buffers)?;
    }

    Ok(())
}

fn handle_readable(
    conn_id: usize,
    poll: &mut Poll,
    connections: &mut Slab<MioConnection>,
    buffers: &mut BufferPool,
    storage: &Arc<Storage>,
    max_value_size: usize,
) -> io::Result<()> {
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let filled = match conn.data_state {
        DataState::Reading { filled } => filled,
        _ => return Ok(()), // Not in reading state
    };

    let read_buf_idx = conn.read_buf_idx;
    let write_buf_idx = conn.write_buf_idx;
    let protocol = conn.protocol;
    let buffer_size = buffers.buffer_size();

    // Read into read buffer
    let read_buf = buffers.get_mut(read_buf_idx);
    let n = match conn.stream.read(&mut read_buf[filled..]) {
        Ok(0) => {
            // EOF
            return Err(io::Error::new(io::ErrorKind::ConnectionReset, "EOF"));
        }
        Ok(n) => n,
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(e),
    };

    let total_filled = filled + n;

    // Process command(s) in the read buffer
    // We need to split borrows: get read data, then write buffer separately
    let input = &buffers.get(read_buf_idx)[..total_filled];
    let input_copy: Vec<u8> = input.to_vec(); // Copy to avoid borrow conflict

    let write_buf = buffers.get_mut(write_buf_idx);
    let result = match protocol {
        Protocol::Memcached => process_memcached(&input_copy, write_buf, storage, max_value_size),
        Protocol::Resp => process_resp(&input_copy, write_buf, storage, max_value_size),
        Protocol::Ping => process_ping(&input_copy, write_buf, storage),
        Protocol::Echo => process_echo(&input_copy, write_buf, storage, max_value_size),
    };

    // Re-borrow connection after buffer operations
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    match result {
        ProcessResult::NeedData => {
            // Need more data, stay in reading state with updated fill level
            conn.data_state = DataState::reading_with(total_filled);
            // Already registered for readable
        }
        ProcessResult::NeedChain { command_len, value_len } => {
            // Large value detected - need to accumulate into chain
            if value_len > max_value_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("value too large: {} > {}", value_len, max_value_size),
                ));
            }

            // Calculate how many chain buffers we need
            let total_needed = command_len + value_len + 2; // +2 for \r\n
            let chain_bytes_needed = total_needed.saturating_sub(buffer_size);
            let chain_buffers_needed = (chain_bytes_needed + buffer_size - 1) / buffer_size;

            // Initialize read chain if needed
            let chain = conn.read_chain.get_or_insert_with(|| BufferChain::new(buffer_size));

            // Allocate chain buffers
            if chain.buffer_count() < chain_buffers_needed {
                let to_alloc = chain_buffers_needed - chain.buffer_count();
                match buffers.alloc_many(to_alloc) {
                    Some(indices) => {
                        // Re-borrow conn to access chain
                        let conn = connections.get_mut(conn_id).unwrap();
                        if let Some(chain) = &mut conn.read_chain {
                            for idx in indices {
                                chain.push_buffer(idx);
                            }
                        }
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            "buffer pool exhausted for large value",
                        ));
                    }
                }
            }

            // Stay in reading state with current fill level
            let conn = connections.get_mut(conn_id).unwrap();
            conn.data_state = DataState::reading_with(total_filled);
        }
        ProcessResult::Response {
            consumed,
            response_len,
        } => {
            // Move unconsumed data to start of read buffer if needed
            if consumed < total_filled {
                let read_buf = buffers.get_mut(read_buf_idx);
                read_buf.copy_within(consumed..total_filled, 0);
            }

            // Re-borrow conn after buffer op
            let conn = connections
                .get_mut(conn_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

            // Release any read chain buffers
            if let Some(mut chain) = conn.read_chain.take() {
                chain.release(buffers);
            }

            // Transition to writing
            conn.data_state = DataState::writing(write_buf_idx, response_len);

            // Register for writable
            poll.registry()
                .reregister(&mut conn.stream, Token(conn_id), Interest::WRITABLE)?;
        }
        ProcessResult::LargeResponse { consumed, response_data } => {
            // Response is too large for single buffer - use write chain
            // Move unconsumed data to start of read buffer if needed
            if consumed < total_filled {
                let read_buf = buffers.get_mut(read_buf_idx);
                read_buf.copy_within(consumed..total_filled, 0);
            }

            // Re-borrow conn after buffer op
            let conn = connections
                .get_mut(conn_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

            // Release any read chain buffers
            if let Some(mut chain) = conn.read_chain.take() {
                chain.release(buffers);
            }

            // Create write chain and populate with response data
            let mut write_chain = BufferChain::new(buffer_size);
            if let Err(ChainError::PoolExhausted) = write_chain.append(&response_data, buffers) {
                write_chain.release(buffers);
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "buffer pool exhausted for large response",
                ));
            }

            let response_len = write_chain.len();
            conn.write_chain = Some(write_chain);

            // Transition to writing with chain
            // Use buf_idx = usize::MAX to signal chain write
            conn.data_state = DataState::Writing {
                buf_idx: usize::MAX,
                written: 0,
                total: response_len,
            };

            // Register for writable
            poll.registry()
                .reregister(&mut conn.stream, Token(conn_id), Interest::WRITABLE)?;
        }
        ProcessResult::Quit => {
            // Client quit, close connection
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "client quit",
            ));
        }
        ProcessResult::Error => {
            // Protocol error
            return Err(io::Error::new(io::ErrorKind::InvalidData, "protocol error"));
        }
    }

    Ok(())
}

fn handle_writable(
    conn_id: usize,
    poll: &mut Poll,
    connections: &mut Slab<MioConnection>,
    buffers: &mut BufferPool,
) -> io::Result<()> {
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let (write_buf_idx, written, total) = match conn.data_state {
        DataState::Writing {
            buf_idx,
            written,
            total,
        } => (buf_idx, written, total),
        _ => return Ok(()), // Not in writing state
    };

    // Check if we're writing from a chain (buf_idx == usize::MAX) or single buffer
    let n = if write_buf_idx == usize::MAX {
        // Chain write using writev
        let chain = conn.write_chain.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing write chain")
        })?;

        let io_slices = chain.io_slices(buffers, written);
        if io_slices.is_empty() {
            0
        } else {
            match conn.stream.write_vectored(&io_slices) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
                }
                Ok(n) => n,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    } else {
        // Single buffer write
        let buf = buffers.get(write_buf_idx);
        match conn.stream.write(&buf[written..total]) {
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
    };

    // Re-borrow after buffer access
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let new_written = written + n;
    if new_written >= total {
        // Write complete - release chain if used
        if let Some(mut chain) = conn.write_chain.take() {
            chain.release(buffers);
        }

        // Go back to reading
        conn.data_state = DataState::reading();
        poll.registry()
            .reregister(&mut conn.stream, Token(conn_id), Interest::READABLE)?;
    } else {
        // Partial write, continue
        conn.data_state = DataState::Writing {
            buf_idx: write_buf_idx,
            written: new_written,
            total,
        };
    }

    Ok(())
}

fn close_connection(
    poll: &mut Poll,
    connections: &mut Slab<MioConnection>,
    buffers: &mut BufferPool,
    conn_id: usize,
) {
    if let Some(mut conn) = connections.try_remove(conn_id) {
        let _ = poll.registry().deregister(&mut conn.stream);
        buffers.free(conn.read_buf_idx);
        buffers.free(conn.write_buf_idx);

        // Release any chain buffers
        if let Some(mut chain) = conn.read_chain.take() {
            chain.release(buffers);
        }
        if let Some(mut chain) = conn.write_chain.take() {
            chain.release(buffers);
        }

        debug!(conn_id, "Connection closed");
    }
}

/// Create a TCP listener with SO_REUSEPORT for kernel load balancing.
fn create_listener_with_reuseport(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let socket = socket2::Socket::new(
        match addr {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        },
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;

    socket.set_reuse_port(true)?;
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;

    Ok(socket.into())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
