//! mio event loop implementation.
//!
//! Readiness-based model: poll tells us when sockets are ready,
//! then we perform non-blocking read/write syscalls.
//! Uses epoll on Linux, kqueue on macOS.

use crate::config::Config;
use crate::runtime::protocol::{process_echo, process_memcached, process_ping, process_resp};
use crate::runtime::{BufferPool, ProcessResult, Protocol};
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

/// Connection state for mio backend.
#[derive(Debug, Clone, Copy)]
enum ConnState {
    /// Waiting for data to be read.
    Reading {
        /// Bytes already read into buffer.
        filled: usize,
    },
    /// Writing response data.
    Writing {
        /// Bytes already written.
        written: usize,
        /// Total bytes to write.
        total: usize,
    },
}

/// Per-worker connection state for mio backend.
struct MioConnection {
    stream: TcpStream,
    state: ConnState,
    read_buf_idx: usize,
    write_buf_idx: usize,
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

    // Need 2 buffers per connection: one for read, one for write
    let mut buffers = BufferPool::new(max_connections * 2, buffer_size);
    let mut connections: Slab<MioConnection> = Slab::with_capacity(max_connections);

    info!(worker = worker_id, "Worker started");

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
                    state: ConnState::Reading { filled: 0 },
                    read_buf_idx,
                    write_buf_idx,
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
) -> io::Result<()> {
    if !connections.contains(conn_id) {
        return Ok(());
    }

    if event.is_readable() {
        handle_readable(conn_id, poll, connections, buffers, storage)?;
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
) -> io::Result<()> {
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let filled = match conn.state {
        ConnState::Reading { filled } => filled,
        _ => return Ok(()), // Not in reading state
    };

    let read_buf_idx = conn.read_buf_idx;
    let write_buf_idx = conn.write_buf_idx;
    let protocol = conn.protocol;

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
        Protocol::Memcached => process_memcached(&input_copy, write_buf, storage),
        Protocol::Resp => process_resp(&input_copy, write_buf, storage),
        Protocol::Ping => process_ping(&input_copy, write_buf, storage),
        Protocol::Echo => process_echo(&input_copy, write_buf, storage),
    };

    // Re-borrow connection after buffer operations
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    match result {
        ProcessResult::NeedData => {
            // Need more data, stay in reading state with updated fill level
            conn.state = ConnState::Reading {
                filled: total_filled,
            };
            // Already registered for readable
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

            // Transition to writing
            conn.state = ConnState::Writing {
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

    let (written, total) = match conn.state {
        ConnState::Writing { written, total } => (written, total),
        _ => return Ok(()), // Not in writing state
    };

    let write_buf_idx = conn.write_buf_idx;

    let buf = buffers.get(write_buf_idx);
    let n = match conn.stream.write(&buf[written..total]) {
        Ok(0) => {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        Ok(n) => n,
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(e),
    };

    // Re-borrow after buffer access
    let conn = connections
        .get_mut(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let new_written = written + n;
    if new_written >= total {
        // Write complete, go back to reading
        conn.state = ConnState::Reading { filled: 0 };
        poll.registry()
            .reregister(&mut conn.stream, Token(conn_id), Interest::READABLE)?;
    } else {
        // Partial write, continue
        conn.state = ConnState::Writing {
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
