//! io_uring event loop for Linux.
//!
//! Completion-based model: submit operations to the ring,
//! then process completions in batches.
//!
//! Uses provided buffer rings for kernel-managed buffer selection on reads.

use super::buf_ring::{BufRing, READ_BGID};
use crate::config::Config;
use crate::runtime::request::{
    process_echo, process_memcached, process_ping, process_resp, ProcessResult,
};
use crate::runtime::{
    BufferPool, ConnPhase, Connection, ConnectionRegistry, DataState, OpType, Protocol,
    TokenAllocator,
};
use crate::storage::Storage;
use io_uring::cqueue::buffer_select;
use io_uring::squeue::Flags;
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
        let storage = Arc::clone(&storage);

        let handle = thread::Builder::new()
            .name(format!("worker-{}", worker_id))
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
    // Create io_uring instance
    let mut ring: IoUring = IoUring::new(config.ring_size as u32)?;

    // Create listener with SO_REUSEPORT
    let listener = create_listener_with_reuseport(addr)?;
    let listener_fd = listener.as_raw_fd();

    let max_connections = config.max_connections;
    let buffer_size = config.buffer_size;
    let batch_size = config.batch_size;

    // Calculate ring entries - cap at 4096 to limit memory usage
    // With 64KB buffers: 4096 * 64KB = 256MB per worker for the read ring
    let ring_entries = std::cmp::min(
        (max_connections as u16).next_power_of_two(),
        4096,
    );

    // Create provided buffer ring for reads (kernel selects buffers)
    let read_buf_ring = BufRing::new(&ring, ring_entries, buffer_size, READ_BGID)?;

    // Create write buffer pool - smaller than max_connections since not all
    // connections are writing simultaneously
    let write_pool_size = std::cmp::min(max_connections, 4096);
    let mut write_buffers = BufferPool::new(write_pool_size, buffer_size);

    let mut connections = ConnectionRegistry::new(max_connections);
    let mut tokens = TokenAllocator::new(max_connections * 2);

    // Submit initial accept
    submit_accept(&mut ring, &mut tokens, listener_fd)?;

    info!(
        worker = worker_id,
        ring_entries = ring_entries,
        "Worker started with buffer ring"
    );

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
            let flags = cqe.flags();

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
                        listener_fd,
                        worker_id,
                        protocol,
                    )?;
                }
                OpType::Read { conn_id } => {
                    // Extract buffer ID from CQE flags (kernel selected this buffer)
                    let buf_id = buffer_select(flags);

                    handle_read(
                        result,
                        conn_id,
                        buf_id,
                        flags,
                        &mut ring,
                        &mut tokens,
                        &mut connections,
                        &read_buf_ring,
                        &mut write_buffers,
                        &storage,
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
                        &mut write_buffers,
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
    listener_fd: RawFd,
    worker_id: usize,
    protocol: Protocol,
) -> io::Result<()> {
    // Always re-arm accept
    submit_accept(ring, tokens, listener_fd)?;

    if result < 0 {
        let err = io::Error::from_raw_os_error(-result);
        warn!("Accept failed: {}", err);
        return Ok(());
    }

    let client_fd = result;

    let conn = Connection::new(client_fd, protocol);

    let conn_id = match connections.insert(conn) {
        Some(id) => id,
        None => {
            warn!("Connection limit reached, closing");
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

    // Submit read for the new connection (kernel will select buffer)
    submit_read(ring, tokens, connections, conn_id)?;

    Ok(())
}

fn handle_read(
    result: i32,
    conn_id: usize,
    buf_id: Option<u16>,
    _flags: u32,
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &mut ConnectionRegistry,
    read_buf_ring: &BufRing,
    write_buffers: &mut BufferPool,
    storage: &Arc<Storage>,
) -> io::Result<()> {
    if result <= 0 {
        // EOF or error: close connection
        if result < 0 {
            let err = io::Error::from_raw_os_error(-result);
            debug!(conn_id, "Read error: {}", err);
        } else {
            debug!(conn_id, "Connection closed by peer");
        }
        // Recycle buffer if we got one
        if let Some(bid) = buf_id {
            read_buf_ring.recycle_buffer(bid);
        }
        close_connection(connections, write_buffers, conn_id);
        return Ok(());
    }

    // Kernel must have provided a buffer for successful read
    let bid = match buf_id {
        Some(bid) => bid,
        None => {
            warn!(conn_id, "Read completed without buffer ID");
            close_connection(connections, write_buffers, conn_id);
            return Ok(());
        }
    };

    let n = result as usize;
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => {
            read_buf_ring.recycle_buffer(bid);
            return Ok(());
        }
    };

    let protocol = conn.protocol;

    // Get the read data from the kernel-selected buffer
    let input = &read_buf_ring.get_buffer_slice(bid)[..n];

    // Allocate a write buffer for the response
    let write_buf_idx = match write_buffers.alloc() {
        Some(idx) => idx,
        None => {
            warn!(conn_id, "No write buffer available");
            read_buf_ring.recycle_buffer(bid);
            close_connection(connections, write_buffers, conn_id);
            return Ok(());
        }
    };

    let write_buf = write_buffers.get_mut(write_buf_idx);
    let result = match protocol {
        Protocol::Memcached => process_memcached(input, write_buf, storage),
        Protocol::Resp => process_resp(input, write_buf, storage),
        Protocol::Ping => process_ping(input, write_buf, storage),
        Protocol::Echo => process_echo(input, write_buf, storage),
    };

    // Recycle read buffer now that we've processed the data
    read_buf_ring.recycle_buffer(bid);

    // Re-borrow connection after buffer operations
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => {
            write_buffers.free(write_buf_idx);
            return Ok(());
        }
    };

    match result {
        ProcessResult::NeedData => {
            // Need more data - resubmit read
            // Note: This is a simplified handling; a full implementation would
            // need to accumulate partial data across multiple reads
            write_buffers.free(write_buf_idx);
            submit_read(ring, tokens, connections, conn_id)?;
        }
        ProcessResult::Response {
            consumed: _,
            response_len,
        } => {
            // Transition to writing
            conn.start_writing(write_buf_idx, response_len);
            submit_write(
                ring,
                tokens,
                connections,
                write_buffers,
                conn_id,
                response_len,
            )?;
        }
        ProcessResult::Quit => {
            write_buffers.free(write_buf_idx);
            close_connection(connections, write_buffers, conn_id);
        }
        ProcessResult::Error => {
            write_buffers.free(write_buf_idx);
            close_connection(connections, write_buffers, conn_id);
        }
    }

    Ok(())
}

fn handle_write(
    result: i32,
    conn_id: usize,
    buf_idx: usize,
    ring: &mut IoUring,
    tokens: &mut TokenAllocator,
    connections: &mut ConnectionRegistry,
    write_buffers: &mut BufferPool,
) -> io::Result<()> {
    if result <= 0 {
        if result < 0 {
            let err = io::Error::from_raw_os_error(-result);
            debug!(conn_id, "Write error: {}", err);
        }
        write_buffers.free(buf_idx);
        close_connection(connections, write_buffers, conn_id);
        return Ok(());
    }

    let n = result as usize;
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => {
            write_buffers.free(buf_idx);
            return Ok(());
        }
    };

    if let ConnPhase::Established(DataState::Writing { written, total, .. }) = &mut conn.phase {
        *written += n;

        if *written >= *total {
            // Write complete, free write buffer and go back to reading
            write_buffers.free(buf_idx);
            conn.start_reading();
            submit_read(ring, tokens, connections, conn_id)?;
        } else {
            // Partial write, continue
            let remaining = *total - *written;
            submit_write(ring, tokens, connections, write_buffers, conn_id, remaining)?;
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
    conn_id: usize,
) -> io::Result<()> {
    let conn = connections
        .get(conn_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connection not found"))?;

    let token = tokens.alloc(OpType::Read { conn_id });

    // Use Recv with BUFFER_SELECT - kernel will pick a buffer from our ring
    let recv = opcode::Recv::new(types::Fd(conn.fd), std::ptr::null_mut(), 0)
        .buf_group(READ_BGID)
        .build()
        .flags(Flags::BUFFER_SELECT)
        .user_data(token);

    unsafe {
        ring.submission().push(&recv).map_err(|_| {
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

    let (buf_idx, offset) = match conn.phase {
        ConnPhase::Established(DataState::Writing {
            buf_idx, written, ..
        }) => (buf_idx, written),
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
    write_buffers: &mut BufferPool,
    conn_id: usize,
) {
    if let Some(conn) = connections.remove(conn_id) {
        // Return write buffer to pool if we have one
        if let ConnPhase::Established(DataState::Writing { buf_idx, .. }) = conn.phase {
            write_buffers.free(buf_idx);
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
