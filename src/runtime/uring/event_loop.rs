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
    let max_value_size = config.max_value_size;

    // Calculate ring entries - cap at 4096 to limit memory usage
    // With 64KB buffers: 4096 * 64KB = 256MB per worker for the read ring
    let ring_entries = std::cmp::min(
        (max_connections as u16).next_power_of_two(),
        4096,
    );

    // Create provided buffer ring for reads (kernel selects buffers)
    let read_buf_ring = BufRing::new(&ring, ring_entries, buffer_size, READ_BGID)?;

    // Create write buffer pool - larger to accommodate chain buffers for large values
    // Base: write buffer per connection + extra for chains
    let write_pool_size = std::cmp::min(max_connections * 2, 8192);
    let mut write_buffers = BufferPool::new(write_pool_size, buffer_size);

    let mut connections = ConnectionRegistry::new(max_connections);
    let mut tokens = TokenAllocator::new(max_connections * 2);

    // Submit initial accept
    submit_accept(&mut ring, &mut tokens, listener_fd)?;

    info!(
        worker = worker_id,
        ring_entries = ring_entries,
        max_value_size,
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
                        max_value_size,
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
    max_value_size: usize,
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
    let buffer_size = write_buffers.buffer_size();

    // Get connection state
    let conn = match connections.get_mut(conn_id) {
        Some(c) => c,
        None => {
            read_buf_ring.recycle_buffer(bid);
            return Ok(());
        }
    };

    let protocol = conn.protocol;
    let accumulated = conn.read_accumulated;

    // Get or allocate accumulation buffer
    let accum_buf_idx = match conn.read_buf_idx {
        Some(idx) => idx,
        None => {
            // Allocate a buffer for accumulation
            match write_buffers.alloc() {
                Some(idx) => {
                    conn.read_buf_idx = Some(idx);
                    idx
                }
                None => {
                    warn!(conn_id, "No buffer available for read accumulation");
                    read_buf_ring.recycle_buffer(bid);
                    close_connection(connections, write_buffers, conn_id);
                    return Ok(());
                }
            }
        }
    };

    // Copy new data from provided buffer to accumulation buffer
    let new_data = &read_buf_ring.get_buffer_slice(bid)[..n];
    let total_len = accumulated + n;

    if total_len > buffer_size {
        // Data exceeds buffer size - would need chained buffers
        warn!(conn_id, "Read data exceeds buffer size");
        read_buf_ring.recycle_buffer(bid);
        close_connection(connections, write_buffers, conn_id);
        return Ok(());
    }

    // Copy to accumulation buffer
    {
        let accum_buf = write_buffers.get_mut(accum_buf_idx);
        accum_buf[accumulated..total_len].copy_from_slice(new_data);
    }

    // Recycle provided buffer now that we've copied the data
    read_buf_ring.recycle_buffer(bid);

    // Update accumulated count
    let conn = connections.get_mut(conn_id).unwrap();
    conn.read_accumulated = total_len;

    // Copy input data to avoid borrow conflict with write buffer allocation
    let input_copy: Vec<u8> = write_buffers.get(accum_buf_idx)[..total_len].to_vec();

    // Allocate a write buffer for the response
    let write_buf_idx = match write_buffers.alloc() {
        Some(idx) => idx,
        None => {
            warn!(conn_id, "No write buffer available");
            close_connection(connections, write_buffers, conn_id);
            return Ok(());
        }
    };

    let write_buf = write_buffers.get_mut(write_buf_idx);
    let result = match protocol {
        Protocol::Memcached => process_memcached(&input_copy, write_buf, storage, max_value_size),
        Protocol::Resp => process_resp(&input_copy, write_buf, storage, max_value_size),
        Protocol::Ping => process_ping(&input_copy, write_buf, storage),
        Protocol::Echo => process_echo(&input_copy, write_buf, storage, max_value_size),
    };

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
            // Need more data - keep accumulated data and resubmit read
            write_buffers.free(write_buf_idx);
            submit_read(ring, tokens, connections, conn_id)?;
        }
        ProcessResult::NeedChain { .. } => {
            // Large value support for io_uring will be added in a follow-up
            // For now, reject as not implemented
            warn!(conn_id, "Large value support not yet implemented for io_uring");
            write_buffers.free(write_buf_idx);
            close_connection(connections, write_buffers, conn_id);
        }
        ProcessResult::Response {
            consumed,
            response_len,
        } => {
            // Clear accumulated data (or shift unconsumed data to start)
            if consumed < total_len {
                let accum_buf = write_buffers.get_mut(accum_buf_idx);
                accum_buf.copy_within(consumed..total_len, 0);
                conn.read_accumulated = total_len - consumed;
            } else {
                conn.read_accumulated = 0;
            }

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
        ProcessResult::LargeResponse { consumed, response_data } => {
            // Clear accumulated data
            if consumed < total_len {
                let accum_buf = write_buffers.get_mut(accum_buf_idx);
                accum_buf.copy_within(consumed..total_len, 0);
                conn.read_accumulated = total_len - consumed;
            } else {
                conn.read_accumulated = 0;
            }

            // Large response - need to use multiple buffers
            // For now, copy to write buffer if it fits, otherwise reject
            if response_data.len() <= write_buffers.buffer_size() {
                let write_buf = write_buffers.get_mut(write_buf_idx);
                write_buf[..response_data.len()].copy_from_slice(&response_data);
                conn.start_writing(write_buf_idx, response_data.len());
                submit_write(
                    ring,
                    tokens,
                    connections,
                    write_buffers,
                    conn_id,
                    response_data.len(),
                )?;
            } else {
                // TODO: Implement multi-buffer write for io_uring
                warn!(conn_id, "Large response support not yet implemented for io_uring");
                write_buffers.free(write_buf_idx);
                close_connection(connections, write_buffers, conn_id);
            }
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

        // Return read accumulation buffer to pool if we have one
        if let Some(buf_idx) = conn.read_buf_idx {
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
