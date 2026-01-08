# v2 Planning: io_uring Runtime Implementation

## Goals

Build a custom io_uring-based runtime for Linux with mio/kqueue fallback for macOS, targeting 200K QPS per core with p99.9 < 2ms latency.

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| IO model | io_uring (Linux), mio/kqueue (macOS) | Completion-based IO enables batching; macOS lacks io_uring |
| Threading | Thread-per-core, share-nothing | Cache locality, no cross-thread synchronization for local ops |
| Buffer strategy | Registered buffers | Control over lifecycle; enables zero-copy for large values |
| Batching | Fixed size initially | Simpler; adaptive batching as follow-up |
| SQPOLL | Skip | Core overhead not justified; batching achieves similar benefit |
| Accept distribution | SO_REUSEPORT | Kernel distributes connections across workers |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                           Main Thread                            │
│  - Parse config                                                  │
│  - Spawn N worker threads (N = core count or configured)        │
│  - Wait for shutdown signal                                      │
└─────────────────────────────────────────────────────────────────┘
                              │
           ┌──────────────────┼──────────────────┐
           ▼                  ▼                  ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│   Worker 0      │ │   Worker 1      │ │   Worker N-1    │
│                 │ │                 │ │                 │
│ ┌─────────────┐ │ │ ┌─────────────┐ │ │ ┌─────────────┐ │
│ │ Listener    │ │ │ │ Listener    │ │ │ │ Listener    │ │
│ │ (SO_REUSE)  │ │ │ │ (SO_REUSE)  │ │ │ │ (SO_REUSE)  │ │
│ └─────────────┘ │ │ └─────────────┘ │ │ └─────────────┘ │
│                 │ │                 │ │                 │
│ ┌─────────────┐ │ │ ┌─────────────┐ │ │ ┌─────────────┐ │
│ │ io_uring    │ │ │ │ io_uring    │ │ │ │ io_uring    │ │
│ │ instance    │ │ │ │ instance    │ │ │ │ instance    │ │
│ └─────────────┘ │ │ └─────────────┘ │ │ └─────────────┘ │
│                 │ │                 │ │                 │
│ ┌─────────────┐ │ │ ┌─────────────┐ │ │ ┌─────────────┐ │
│ │ Buffer Pool │ │ │ │ Buffer Pool │ │ │ │ Buffer Pool │ │
│ └─────────────┘ │ │ └─────────────┘ │ │ └─────────────┘ │
│                 │ │                 │ │                 │
│ ┌─────────────┐ │ │ ┌─────────────┐ │ │ ┌─────────────┐ │
│ │ Connections │ │ │ │ Connections │ │ │ │ Connections │ │
│ │ HashMap     │ │ │ │ HashMap     │ │ │ │ HashMap     │ │
│ └─────────────┘ │ │ └─────────────┘ │ │ └─────────────┘ │
└─────────────────┘ └─────────────────┘ └─────────────────┘
```

## Scope

### Included

- io_uring event loop with batched submit (Linux)
- mio/kqueue event loop (macOS)
- Multi-threaded workers with SO_REUSEPORT
- Per-worker buffer pool (registered buffers on Linux, regular pool on macOS)
- Connection state machine (accept → read → process → write → read...)
- Protocol parsing integration (existing Memcached/RESP parsers)
- Graceful shutdown (drain in-flight, close connections)
- Basic metrics (connections, ops/sec, latency histogram)

### Explicitly Deferred

- Storage layer changes (stays as-is for now; revisit for concurrent access)
- Zero-copy for large values (threshold-based optimization)
- Adaptive batch sizing
- SQPOLL mode
- Multishot recv

## Implementation Plan

### Phase 1: Core Runtime Infrastructure

**1.1 Add dependencies**
```toml
[target.'cfg(target_os = "linux")'.dependencies]
io-uring = "0.6"

[target.'cfg(target_os = "macos")'.dependencies]
mio = { version = "0.8", features = ["os-poll", "net"] }

[dependencies]
libc = "0.2"
slab = "0.4"
```

**1.2 Create runtime module structure**
```
src/
  runtime/
    mod.rs           # Public API, platform dispatch
    worker.rs        # Common worker trait/interface
    buffer.rs        # Buffer pool management (shared)
    connection.rs    # Connection state machine (shared)
    token.rs         # Operation tracking (shared)
    uring/
      mod.rs         # io_uring implementation (Linux)
      loop.rs        # Event loop
    kqueue/
      mod.rs         # mio/kqueue implementation (macOS)
      loop.rs        # Event loop
```

**1.3 Implement Token/Operation tracking**

Track in-flight operations with efficient lookup:

```rust
enum OpType {
    Accept,
    Read { conn_id: usize, buf_idx: usize },
    Write { conn_id: usize, buf_idx: usize },
}

struct TokenAllocator {
    ops: Slab<OpType>,  // slab for O(1) lookup
}
```

### Phase 2: Buffer Management

**2.1 Per-worker buffer pool**

```rust
struct BufferPool {
    buffers: Vec<Vec<u8>>,      // Actual buffer storage
    free_list: Vec<usize>,       // Available buffer indices
    buffer_size: usize,          // Fixed size per buffer
}

impl BufferPool {
    fn new(count: usize, size: usize) -> Self;
    fn alloc(&mut self) -> Option<usize>;
    fn free(&mut self, idx: usize);
    fn get(&self, idx: usize) -> &[u8];
    fn get_mut(&mut self, idx: usize) -> &mut [u8];
}
```

**2.2 Register buffers with io_uring**

```rust
// During worker init
let iovecs: Vec<libc::iovec> = buffers.iter()
    .map(|b| libc::iovec { iov_base: b.as_ptr() as _, iov_len: b.len() })
    .collect();
ring.submitter().register_buffers(&iovecs)?;
```

### Phase 3: Connection State Machine

**3.1 Connection states**

```rust
enum ConnState {
    Reading { buf_idx: usize, filled: usize },
    Processing,
    Writing { buf_idx: usize, written: usize, total: usize },
}

struct Connection {
    fd: RawFd,
    state: ConnState,
    protocol: Protocol,  // Memcached or RESP
}
```

**3.2 Connection registry**

```rust
struct ConnectionRegistry {
    connections: Slab<Connection>,
    max_connections: usize,
}
```

### Phase 4: Event Loop

**4.1 Core loop structure**

```rust
fn worker_loop(worker_id: usize, config: &Config) -> Result<()> {
    let listener = create_listener_with_reuseport(&config.bind_addr)?;
    let mut ring = IoUring::new(config.ring_size)?;
    let mut buffers = BufferPool::new(config.max_connections, config.buffer_size);
    let mut connections = ConnectionRegistry::new(config.max_connections);
    let mut tokens = TokenAllocator::new();

    // Register buffers
    register_buffers(&ring, &buffers)?;

    // Submit initial accept
    submit_accept(&mut ring, &mut tokens, listener.as_raw_fd())?;

    loop {
        // Process completions (batched)
        let batch_size = config.batch_size.min(ring.completion().len().max(1));
        for _ in 0..batch_size {
            if let Some(cqe) = ring.completion().next() {
                handle_completion(
                    cqe,
                    &mut ring,
                    &mut tokens,
                    &mut connections,
                    &mut buffers,
                    listener.as_raw_fd(),
                )?;
            } else {
                break;
            }
        }

        // Submit all queued operations + wait for at least 1
        ring.submit_and_wait(1)?;
    }
}
```

**4.2 Completion handlers**

```rust
fn handle_completion(...) {
    let op = tokens.get(cqe.user_data());
    let result = cqe.result();

    match op {
        OpType::Accept => {
            if result >= 0 {
                let conn_fd = result;
                let conn_id = connections.insert(Connection::new(conn_fd));
                submit_read(&mut ring, &mut tokens, conn_id, ...)?;
            }
            // Re-arm accept
            submit_accept(&mut ring, &mut tokens, listener_fd)?;
        }

        OpType::Read { conn_id, buf_idx } => {
            if result <= 0 {
                // EOF or error: close connection
                close_connection(&mut connections, &mut buffers, conn_id);
            } else {
                let n = result as usize;
                let conn = connections.get_mut(conn_id);
                // Parse and process...
                // Then submit write or next read
            }
        }

        OpType::Write { conn_id, buf_idx } => {
            if result <= 0 {
                close_connection(&mut connections, &mut buffers, conn_id);
            } else {
                // Check if write complete, submit next read
            }
        }
    }
}
```

### Phase 5: macOS mio/kqueue Backend

**5.1 mio event loop structure**

Readiness-based (vs io_uring completion-based), but same worker model:

```rust
fn worker_loop_kqueue(worker_id: usize, config: &Config) -> Result<()> {
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(config.batch_size);

    let listener = create_listener_with_reuseport(&config.bind_addr)?;
    poll.registry().register(&mut listener, LISTENER_TOKEN, Interest::READABLE)?;

    let mut buffers = BufferPool::new(config.max_connections, config.buffer_size);
    let mut connections = ConnectionRegistry::new(config.max_connections);

    loop {
        poll.poll(&mut events, None)?;

        for event in events.iter() {
            match event.token() {
                LISTENER_TOKEN => {
                    // Accept new connections
                    loop {
                        match listener.accept() {
                            Ok((socket, _)) => {
                                let conn_id = connections.insert(...);
                                poll.registry().register(&mut socket, Token(conn_id), Interest::READABLE)?;
                            }
                            Err(ref e) if e.kind() == WouldBlock => break,
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
                Token(conn_id) => {
                    let conn = connections.get_mut(conn_id);
                    if event.is_readable() {
                        // Read into buffer, parse, process
                    }
                    if event.is_writable() {
                        // Write response
                    }
                }
            }
        }
    }
}
```

**5.2 Key differences from io_uring**

| Aspect | io_uring | mio/kqueue |
|--------|----------|------------|
| Model | Completion-based | Readiness-based |
| Syscalls | Batched via ring | Per read()/write() |
| Buffer ownership | Kernel holds during op | Userspace always owns |
| Accept pattern | Submit accept, get fd in CQE | poll() ready, call accept() |

**5.3 Shared abstractions**

- `BufferPool` — Same implementation, no registered buffers
- `ConnectionRegistry` — Same Slab-based tracking
- `Connection` state machine — Same states, different event triggers
- Protocol handlers — Identical

### Phase 6: Integration

**6.1 Wire up protocol handlers**

Reuse existing `src/protocols/` parsers:

```rust
fn process_command(
    conn: &mut Connection,
    buf: &[u8],
    storage: &Storage,  // Shared reference for now
) -> Response {
    match conn.protocol {
        Protocol::Memcached => parse_and_execute_memcached(buf, storage),
        Protocol::Resp => parse_and_execute_resp(buf, storage),
    }
}
```

**6.2 Update main.rs**

```rust
fn main() {
    let config = Config::load()?;

    #[cfg(target_os = "linux")]
    {
        runtime::uring::run(config)?;
    }

    #[cfg(target_os = "macos")]
    {
        runtime::kqueue::run(config)?;
    }
}
```

### Phase 7: Testing and Benchmarking

**7.1 Unit tests**
- Buffer pool alloc/free
- Token allocator
- Connection state transitions

**7.2 Integration tests**
- Single connection lifecycle
- Multiple concurrent connections
- Protocol correctness (Memcached, RESP)

**7.3 Benchmarks**
- Baseline: current Tokio implementation
- io_uring: new runtime
- Compare: throughput, p50, p99, p99.9

## Configuration

New config options:

```toml
[runtime]
workers = 0           # 0 = auto-detect cores
ring_size = 4096      # io_uring queue depth
buffer_size = 16384   # Per-connection buffer (16KB)
max_connections = 10000  # Per worker
batch_size = 64       # Fixed batch size (TODO: adaptive)
```

## Dependencies

```toml
[target.'cfg(target_os = "linux")'.dependencies]
io-uring = "0.6"

[dependencies]
libc = "0.2"
```

## Future Work (TODO)

1. **Adaptive batch sizing** — Scale batch size based on `completion().len()` and target latency
2. **Zero-copy large values** — Two-phase read for values >32KB
3. **SQPOLL evaluation** — Shared SQPOLL thread if syscall overhead still significant
4. **Multishot recv** — Single submission for multiple completions
5. **Storage integration** — Thread-local storage shards, cross-shard channels

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| io_uring kernel version requirements | Check kernel version at startup; graceful error |
| Complexity vs Tokio | Start simple (no SQPOLL, no multishot); iterate |
| Storage contention | Defer; current RwLock works for initial benchmarks |
| Platform divergence | Shared abstractions (BufferPool, Connection, Protocol); platform-specific only in event loop |
| mio/kqueue lower performance than io_uring | Expected and acceptable per assumptions (macOS for smaller workloads) |

## Success Criteria

1. Passes all existing protocol tests on both Linux and macOS
2. Achieves ≥200K QPS per core on Linux (io_uring)
3. Runs correctly on macOS (mio/kqueue) — performance targets relaxed
4. p99.9 latency < 2ms under load on Linux
5. Memory usage within 2x of Tokio baseline
6. Clean shutdown without resource leaks
