//! io_uring event loop for Linux.
//!
//! Completion-based model: submit operations to the ring,
//! then process completions in batches.

use crate::config::Config;
use crate::runtime::{
    BufferPool, ConnState, Connection, ConnectionRegistry, OpType, Protocol, TokenAllocator,
};
use crate::storage::Storage;
use io_uring::{opcode, types, IoUring};
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;
use std::thread;
use tracing::{debug, error, info, warn};

/// Run the io_uring-based server.
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
        ring_size = config.ring_size,
        protocol = ?protocol,
        "Starting io_uring runtime"
    );

    let mut handles = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let config = config.clone();
        let addr = addr;
        let storage = Arc::clone(&storage);

        let handle = thread::Builder::new()
            .name(format!("worker-{}", worker_id))
            .spawn(move || {
                // TODO: Wire protocol handlers into io_uring event loop
                let _ = (&storage, protocol); // Suppress unused warnings for now
                if let Err(e) = worker_loop(worker_id, addr, &config) {
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

fn worker_loop(worker_id: usize, addr: SocketAddr, config: &Config) -> io::Result<()> {
    // Create io_uring instance
    let mut ring: IoUring = IoUring::new(config.ring_size as u32)?;

    // Create listener with SO_REUSEPORT
    let listener = create_listener_with_reuseport(addr)?;
    let listener_fd = listener.as_raw_fd();

    let max_connections = config.max_connections;
    let buffer_size = config.buffer_size;
    let batch_size = config.batch_size;

    let mut buffers = BufferPool::new(max_connections, buffer_size);
    let mut connections = ConnectionRegistry::new(max_connections);
    let mut tokens = TokenAllocator::new(max_connections * 2); // headroom for concurrent ops

    // TODO: Register buffers with io_uring for faster validation
    // register_buffers(&ring, &buffers)?;

    // Submit initial accept
    submit_accept(&mut ring, &mut tokens, listener_fd)?;

    info!(worker = worker_id, "Worker started");

    loop {
        // Submit pending operations and wait for at least one completion
        ring.submit_and_wait(1)?;

        // Process completions in batch
        let mut processed = 0;
        while processed < batch_size {
            let cqe = match ring.completion().next() {
                Some(cqe) => cqe,
                None => break,
            };

            processed += 1;

            let token = cqe.user_data();
            let result = cqe.result();

            // Get and free the operation token
            let op = match tokens.free(token) {
                Some(op) => op,
                None => {
                    warn!("Unknown token in completion: {}", token);
                    continue;
                }
            };

            match op {
                OpType::Accept => {
                    handle_accept(
                        result,
                        &mut ring,
                        &mut tokens,
                        &mut connections,
                        &mut buffers,
                        listener_fd,
                        worker_id,
                    )?;
                }
                OpType::Read { conn_id, buf_idx } => {
                    handle_read(
                        result,
                        conn_id,
                        buf_idx,
                        &mut ring,
                        &mut tokens,
                        &mut connections,
                        &mut buffers,
                    )?;
                }
                OpType::Write { conn_id, buf_idx } => {
                    handle_write(
                        result,
                        conn_id,
                        buf_idx,
                        &mut ring,
                        &mut tokens,
                        &mut connections,
                        &mut buffers,
                    )?;
                }
            }
        }
    }
}

fn handle_accept(
    result: i32,
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &mut ConnectionRegistry,
    buffers: &mut BufferPool,
    listener_fd: RawFd,
    worker_id: usize,
) -> io::Result<()> {
    // Always re-arm accept
    submit_accept(ring, tokens, listener_fd)?;

    if result < 0 {
        let err = io::Error::from_raw_os_error(-result);
        warn!("Accept failed: {}", err);
        return Ok(());
    }

    let client_fd = result;

    // Allocate buffer for this connection
    let buf_idx = match buffers.alloc() {
        Some(idx) => idx,
        None => {
            warn!("Buffer pool exhausted, closing connection");
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
    };

    // TODO: Pass protocol from config through to here
    let protocol = crate::runtime::connection::Protocol::Memcached;
    let conn = Connection::new(client_fd, buf_idx, protocol);

    let conn_id = match connections.insert(conn) {
        Some(id) => id,
        None => {
            warn!("Connection limit reached, closing");
            buffers.free(buf_idx);
            unsafe { libc::close(client_fd) };
            return Ok(());
        }
    };

    debug!(
        worker = worker_id,
        conn_id,
        fd = client_fd,
        "Accepted connection"
    );

    // Submit read for the new connection
    submit_read(ring, tokens, connections, buffers, conn_id)?;

    Ok(())
}

fn handle_read(
    result: i32,
    conn_id: usize,
    buf_idx: usize,
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &mut ConnectionRegistry,
    buffers: &mut BufferPool,
) -> io::Result<()> {
    if result <= 0 {
        // EOF or error: close connection
        if result < 0 {
            let err = io::Error::from_raw_os_error(-result);
            debug!(conn_id, "Read error: {}", err);
        } else {
            debug!(conn_id, "Connection closed by peer");
        }
        close_connection(connections, buffers, conn_id);
        return Ok(());
    }

    let n = result as usize;
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => return Ok(()),
    };

    // Update filled amount
    if let ConnState::Reading { filled, .. } = &mut conn.state {
        *filled += n;
    }

    // TODO: Parse command, execute, prepare response
    // For now, echo back what we received
    let buf = buffers.get(buf_idx);
    let response_len = n; // Echo: response = input

    // Copy to write position (in real impl, prepare actual response)
    // For echo, data is already in buffer

    conn.start_writing(buf_idx, response_len);
    submit_write(ring, tokens, connections, buffers, conn_id, response_len)?;

    Ok(())
}

fn handle_write(
    result: i32,
    conn_id: usize,
    buf_idx: usize,
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &mut ConnectionRegistry,
    buffers: &mut BufferPool,
) -> io::Result<()> {
    if result <= 0 {
        if result < 0 {
            let err = io::Error::from_raw_os_error(-result);
            debug!(conn_id, "Write error: {}", err);
        }
        close_connection(connections, buffers, conn_id);
        return Ok(());
    }

    let n = result as usize;
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => return Ok(()),
    };

    if let ConnState::Writing { written, total, .. } = &mut conn.state {
        *written += n;

        if *written >= *total {
            // Write complete, start reading next command
            conn.start_reading(buf_idx);
            submit_read(ring, tokens, connections, buffers, conn_id)?;
        } else {
            // Partial write, continue
            let remaining = *total - *written;
            submit_write(ring, tokens, connections, buffers, conn_id, remaining)?;
        }
    }

    Ok(())
}

fn submit_accept(
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    listener_fd: RawFd,
) -> io::Result<()> {
    let token = tokens.alloc(OpType::Accept);

    let accept = opcode::Accept::new(
        types::Fd(listener_fd),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )
    .build()
    .user_data(token);

    unsafe {
        ring.submission().push(&accept).map_err(|_| {
            tokens.free(token);
            io::Error::new(io::ErrorKind::Other, "submission queue full")
        })?;
    }

    Ok(())
}

fn submit_read(
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &ConnectionRegistry,
    buffers: &mut BufferPool,
    conn_id: usize,
) -> io::Result<()> {
    let conn = connections
        .get(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let (buf_idx, offset) = match conn.state {
        ConnState::Reading { buf_idx, filled } => (buf_idx, filled),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not in reading state",
            ))
        }
    };

    let buf_ptr = buffers.get_ptr(buf_idx);
    let buf_len = buffers.buffer_size() - offset;

    let token = tokens.alloc(OpType::Read { conn_id, buf_idx });

    let read = opcode::Read::new(
        types::Fd(conn.fd),
        unsafe { buf_ptr.add(offset) },
        buf_len as u32,
    )
    .build()
    .user_data(token);

    unsafe {
        ring.submission().push(&read).map_err(|_| {
            tokens.free(token);
            io::Error::new(io::ErrorKind::Other, "submission queue full")
        })?;
    }

    Ok(())
}

fn submit_write(
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &ConnectionRegistry,
    buffers: &mut BufferPool,
    conn_id: usize,
    len: usize,
) -> io::Result<()> {
    let conn = connections
        .get(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let (buf_idx, offset) = match conn.state {
        ConnState::Writing {
            buf_idx, written, ..
        } => (buf_idx, written),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not in writing state",
            ))
        }
    };

    let buf_ptr = buffers.get_ptr(buf_idx);

    let token = tokens.alloc(OpType::Write { conn_id, buf_idx });

    let write = opcode::Write::new(
        types::Fd(conn.fd),
        unsafe { buf_ptr.add(offset) },
        len as u32,
    )
    .build()
    .user_data(token);

    unsafe {
        ring.submission().push(&write).map_err(|_| {
            tokens.free(token);
            io::Error::new(io::ErrorKind::Other, "submission queue full")
        })?;
    }

    Ok(())
}

fn close_connection(
    connections: &mut ConnectionRegistry,
    buffers: &mut BufferPool,
    conn_id: usize,
) {
    if let Some(conn) = connections.remove(conn_id) {
        // Return buffer to pool
        match conn.state {
            ConnState::Reading { buf_idx, .. } | ConnState::Writing { buf_idx, .. } => {
                buffers.free(buf_idx);
            }
            _ => {}
        }

        // Close the file descriptor
        unsafe { libc::close(conn.fd) };

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
