# v2 Discussion: Threading and Runtime Architecture

---
## Session: 2025-12-29 - Threading Model and Runtime Selection

### Context
Revisiting the threading and runtime decisions for v2. Started by analyzing the current architecture's limitations and exploring alternatives that could achieve the target of 200K QPS/core with p99.9 < 2ms latency.

### Key Points Explored

#### Current Architecture Assessment

The v1 architecture uses Tokio multi-threaded runtime with task-per-connection:

- **Single accept loop** — All connections funnel through one `listener.accept()`, becoming a bottleneck at scale
- **Shared storage with RwLock** — `RwLock<HashMap>` for data + separate `RwLock<HashMap>` for LRU tracking
- **Every GET takes a write lock** — `record_access()` for LRU tracking serializes all read operations
- **O(n) LRU scan** — `find_lru_key()` iterates entire HashMap to find minimum, unusable at 1M+ keys
- **Unused workers config** — `workers` option is parsed but never wired to Tokio runtime builder

Syscall analysis for a single GET (worst reasonable case):
- Uncontended: 3-4 syscalls (epoll_wait, read, write, maybe epoll_ctl)
- With lock contention: 5-8 syscalls (adds futex wait/wake)

#### Storage Sharding Options

**Option 1: Sharded locks (pragmatic)**
- Network task hashes key → picks shard → acquires that shard's lock
- Handover cost: near zero (function call + lock)
- Lock contention drops by factor of N shards
- Works with existing Tokio model
- Recommendation: Use `parking_lot::RwLock` (faster, no poisoning)

**Option 2: Thread-per-shard with channels**
- Each shard on dedicated OS thread, owns plain HashMap (no locks)
- Network tasks send `(Command, oneshot::Sender)` to shard channel
- Handover cost: ~50-100ns channel round-trip
- Zero lock contention, excellent cache locality
- More complex, channel overhead visible at 200K QPS

**Option 3: Thread-per-core hybrid**
- One OS thread per core with LocalSet
- Each core owns listener (SO_REUSEPORT) + local shard
- Cross-shard via channels (rare if keys distribute well)
- Maximum performance, maximum complexity

Conclusion: Option 1 (sharded locks) is pragmatic for v2. Gets 90% of benefit with 20% of effort.

#### Tokio vs mio-direct

**Tokio advantages:**
- Automatic load balancing via work-stealing
- async/await readability
- Battle-tested edge case handling
- Rich ecosystem (tokio-util, tower, tracing integration)
- Debugging: tokio-console, async backtraces, runtime metrics

**Tokio disadvantages:**
- No control over connection-to-thread mapping
- Tasks migrate between cores → cache thrashing
- Work-stealing overhead (~50-100ns per steal)
- ~10-15% overhead for machinery not needed by simple cache ops

**mio-direct advantages:**
- Perfect cache locality (connection + data stay on one core)
- No locks for local shard (single-threaded)
- Predictable latency, fewer syscalls
- Total control

**mio-direct disadvantages:**
- Must handle: partial reads/writes, buffer management, backpressure, timer wheels, graceful shutdown
- No async/await — callback-style or manual state machines
- Debugging harder (no task correlation, just "fd 23 is readable")
- ~150+ lines for what Tokio does in ~50 lines

Syscall comparison per GET:
- Tokio: ~700-1500ns total (includes ~100-200ns scheduler overhead)
- mio-direct: ~600-1300ns total
- Difference: ~10-15%, or 2-4% of 5µs budget at 200K QPS

#### io_uring Implications

io_uring fundamentally changes the model from readiness-based to completion-based:
- epoll/kqueue: "socket ready" → you call read()
- io_uring: you submit read() → "read completed with data"

**io_uring benefits:**
- Batching: 1 syscall for N operations vs N syscalls
- SQPOLL mode: kernel polls submission queue, ~0 syscalls for submit
- Registered buffers: skip per-op buffer validation
- Multishot recv: one submission, multiple completions

Syscall comparison for 100 concurrent GETs:
- epoll: ~300 syscalls
- io_uring basic: ~3-6 syscalls
- io_uring + SQPOLL: ~1-2 syscalls

**Critical insight:** Using io_uring with Tokio (via tokio-uring) requires `!Send` futures, which means no work-stealing — you're forced into thread-per-core model anyway. At that point, Tokio's main advantages disappear.

**Platform reality:**
- Linux: io_uring (best) or epoll (fallback)
- macOS: kqueue only (no io_uring equivalent)
- Windows: IOCP (completion-based, similar model to io_uring)

This matches assumption E1: "Linux for high-scale deployments, macOS for smaller workloads."

#### compio as Alternative Runtime

compio: thread-per-core runtime with io_uring/IOCP/kqueue backends, ~16 months old.

| Metric | compio | Tokio |
|--------|--------|-------|
| Age | ~16 months | ~9 years |
| Stars | 1.4k | ~27k |
| Downloads | ~500k | 400M+ |
| Contributors | 34 | 600+ |
| LTS policy | None | Yes |

**compio ecosystem gaps:**
- No equivalent to tokio-console
- No connection pooling, HTTP client, database drivers
- Limited Stack Overflow / blog coverage
- 2 primary maintainers (bus factor risk)

**compio observability:**
- Uses `tracing` via `compio-log` wrapper
- Has `instrument!` macro for function-level spans
- Syscall errors are logged with proper classification
- Lacks task-centric correlation that makes Tokio debugging special

For a cache server with simple request lifecycle, compio's observability is adequate.

#### Debugging and Observability

**Tokio provides (that others don't):**
- `tokio-console`: live task inspector (state, poll times, waker stats)
- Async backtraces: see logical async call stack, not just event loop position
- Task correlation: "Task 47 spawned at server.rs:58, awaiting read at handler.rs:42"
- Runtime metrics: active_tasks, injection_queue_depth, worker_steal_count

**What's the same everywhere:**
- `tracing` crate works
- `perf`/`flamegraph`/`strace` work
- Custom metrics you add yourself

**Tokio's RuntimeMetrics overhead:**
- `handle.metrics()`: Arc clone (~10-20ns)
- `metrics.worker_poll_count(n)`: atomic load (~1-5ns)
- No heap allocations — metrics live as atomics in scheduler state

Tokio is not opinionated about telemetry implementation. It provides hooks (tracing spans, raw counters), not histograms or export formats. You choose: Prometheus, StatsD, HDR histograms, whatever.

#### Context Propagation

For carrying request/client context through syscall-level functions, don't need custom runtime:

1. **Thread-local context** — Works with any runtime in thread-per-core model
2. **tracing spans** — Context propagates automatically through span hierarchy
3. **Explicit passing** — Just pass `RequestContext` struct through

Custom runtime only justified for:
- Per-operation context in completion path
- Custom scheduling based on context
- Metrics integrated at syscall level

#### Buffer Management for Completion IO

With completion-based IO (io_uring/IOCP), kernel owns buffer until operation completes. Strategies:

1. **Buffer-per-operation**: Simple but 200K mallocs/sec becomes bottleneck
2. **Buffer pool**: Reuse buffers, but lock contention if shared
3. **Registered buffers** (io_uring): Pre-register with kernel, faster
4. **Provided buffer rings** (io_uring): Kernel picks buffer, efficient batching
5. **Custom slab allocator**: Fixed-size slabs for common value sizes

compio has basic `BufferPool`, but not registered/provided buffer optimization.

Cache-specific optimizations that might justify custom runtime:
- Read buffer → storage value without copy
- Write response directly from storage buffer
- Buffer sizes tuned to value size distribution

### Decisions Made

1. **Keep Tokio for v2** — Pragmatic choice. Sharding storage is higher impact than runtime change.
2. **Shard storage with parking_lot::RwLock** — Option 1, ~64 shards for 32-core target
3. **Fix O(n) LRU** — Required regardless of runtime choice
4. **Defer io_uring/custom runtime** — Revisit after benchmarking sharded Tokio
5. **Use tracing spans for context** — No runtime changes needed for observability

### Open Items

1. Benchmark sharded storage with Tokio to validate 200K QPS/core is achievable
2. If Tokio falls short, evaluate compio vs custom mio-based runtime
3. Buffer management optimization — profile first, optimize if malloc is bottleneck
4. io_uring for Linux — future milestone after v2 baseline established

### Assumptions Updated

During this discussion, refined/added to `docs/ASSUMPTIONS.md`:
- E1: macOS added as production target (not just dev)
- O2: 200K QPS per core (was 100K-1M per instance)
- O4: p99.9 < 2ms (was p99 < 1ms)
- Workload #6: Multi-key operations bounded (configurable limit)
- Others #2: Pipelining supported (both protocols, in-order response)
- O7: Eviction is core capability (not graceful degradation)

---

## Session: 2026-01-02 - io_uring Deep Dive and Implementation Planning

### Context
Following up on the previous session's decision to defer io_uring, this session reversed course and dove deep into io_uring implementation details, buffer management strategies, and batching trade-offs to prepare for a custom mio+io_uring runtime.

### Key Points Explored

#### Buffer Management Strategies

**Per-connection buffer:**
- Natural model for TCP streams—accumulate partial reads until complete command
- At 100K connections × 16KB = 1.6GB memory (acceptable for dedicated cache servers)
- Simple, works with readiness-based IO (epoll/kqueue)

**io_uring buffer options:**

| Strategy | Description | Trade-off |
|----------|-------------|-----------|
| Provided buffer rings | Kernel picks buffer from pool | No per-connection ownership; kernel decides |
| Registered buffers | Pre-register pool, you specify buffer index | Full control, skip per-op validation |
| Regular buffers | Standard allocation | Per-op setup cost (~tens of ns) |

**Key insight:** Provided buffer rings and zero-copy are fundamentally incompatible. Ring expects buffers returned; zero-copy means buffer becomes storage value.

**Decision:** Use registered buffers with explicit assignment for control over buffer lifecycle, especially for zero-copy large value path.

#### Zero-Copy Threshold Analysis

Two-phase read (zero-copy) adds one extra io_uring submission. Trade-off vs memcpy:

| Payload | memcpy cost (@50GB/s) | Extra submission | Winner |
|---------|----------------------|------------------|--------|
| 4KB | ~80ns | ~300ns | Single read + copy |
| 16KB | ~320ns | ~300ns | Break-even |
| 32KB | ~640ns | ~300ns | Zero-copy |
| 1MB | ~20µs | ~300ns | Zero-copy (66x better) |

**Threshold:** ~16-32KB depending on batching efficiency. Below: copy. Above: two-phase zero-copy.

**Implementation strategy:**
1. First read into small buffer (4-16KB, covers most commands)
2. Parse header to determine value size
3. If large value: allocate value buffer, copy partial bytes, submit exact read for remainder
4. Small copy overhead (~80ns for 4KB) is negligible

#### Ring and Buffer Sizing

**Ring size:** Number of in-flight operations (SQ entries + pending CQ entries)
- ~64 bytes per entry
- Oversize freely—memory cost trivial

**Buffer pool size:** Must match max concurrent connections with active I/O
- Undersized pool → reject connections or stall operations (dangerous)
- Oversized pool → waste memory (safe)

**Sizing rule:** Buffer count ≥ max connections. Ring size ≥ 2× expected concurrent ops (headroom for batching).

#### SQPOLL Analysis

SQPOLL eliminates `io_uring_enter` syscall by having kernel thread poll submission queue.

**Cost:** Dedicates one CPU core per io_uring instance.

**Math at 200K QPS:**
- Without SQPOLL: ~500ns syscall × 200K = 100ms/sec (10% of core)
- With SQPOLL: 0 syscall overhead, but lose 50% of cores (if 1:1 ring:worker)

**Conclusion:** SQPOLL not worth it for v2. Aggressive batching achieves similar syscall reduction without core overhead. Consider shared SQPOLL thread across rings (Linux 5.11+) if revisiting.

#### Batching Deep Dive

**Natural batching pattern:**
```rust
loop {
    while let Some(cqe) = ring.completion().next() {
        handle_and_queue(cqe);
    }
    ring.submit_and_wait(1)?;
}
```

Process all available completions, then one syscall for all queued submissions.

**Batching trade-offs:**

| Effect | Impact on p50 |
|--------|--------------|
| Queuing delay (batch wait) | Hurts p50 (+50-125µs at high batch size) |
| Reduced syscall overhead → lower utilization → shorter queues | Helps p50 |

**Crossover point:** At low load, batching hurts p50. At high load (near saturation), reduced syscall overhead wins.

**Adaptive batching:** Check `ring.completion().len()` (cheap: 2 atomic loads, ~10-20ns). Batch aggressively when queue depth high, submit immediately when low.

**Network RTT context:**

| Scenario | Network RTT | Batching delay as % of total |
|----------|-------------|------------------------------|
| Same rack | 30µs | 77% (dominates) |
| Same AZ | 100µs | 50% (significant) |
| Cross-AZ | 1ms | 9% (noise) |

For same-AZ deployments, batching delay is noticeable. Adaptive batching matters.

#### Latency Formulas

**Little's Law (general):** L = λ × W
- L = average items in system
- λ = arrival rate
- W = average time in system

**M/M/1 queue depth:** L = ρ / (1 - ρ)
- ρ = utilization = λ/μ
- Hyperbolic blowup near saturation

**M/D/1 (deterministic service):** L ≈ ρ²/(2(1-ρ)) + ρ
- ~Half the queue depth of M/M/1 at same utilization
- Better model for cache workloads (consistent service times)

**Batch size formula (for p50 ≤ 2× ideal):**
```
max_batch = 2 × userspace_drain_rate × network_rtt
```
Example: 0.33 ops/µs drain rate × 100µs RTT → max_batch ≈ 66

### Decisions Made

1. **Build custom io_uring runtime** — Direct control over buffer lifecycle, zero-copy paths, batching
2. **Use registered buffers** — Not provided buffer rings, to enable zero-copy for large values
3. **Fixed batch size initially** — Adaptive batching as future TODO
4. **Skip SQPOLL** — Aggressive batching provides similar benefit without core overhead
5. **Thread-per-core model** — Each worker owns: listener (SO_REUSEPORT), io_uring instance, local buffers
6. **Zero-copy threshold ~32KB** — Below: copy (memcpy faster than extra submission)
7. **macOS fallback to mio/kqueue** — No io_uring on macOS; accept lower performance

### Open Items

1. **Adaptive batch sizing** — Based on completion queue depth and network RTT estimate
2. **Storage integration** — Sharded storage with thread-per-core; may need channels for cross-shard
3. **Graceful shutdown** — Drain in-flight operations, close connections cleanly
4. **Metrics/observability** — tracing integration, per-worker stats
5. **Benchmark against Tokio baseline** — Validate io_uring actually wins for this workload

---

## Session: 2026-01-06 - Test Protocols and Context Management

### Context
Continuing v2 implementation after the native runtime (kqueue/io_uring) Phase 1 was completed and committed. This session added test protocols and explored tooling improvements for long-running AI-assisted development sessions.

### Key Points Explored

#### Test Protocol Design

User requested two protocols for testing purposes:
- **Ping**: Health checks and latency measurement
- **Echo**: Throughput testing with variable payload sizes

**Ping Protocol rationale:**
- Load balancers need simple health checks without touching cache state
- Measures pure network + runtime overhead (no storage operations)
- Establishes baseline latency for comparing Tokio vs native runtime
- Format: `PING\r\n` → `PONG\r\n` (or with optional message echo)

**Echo Protocol rationale:**
- Tests server's ability to move bytes without protocol parsing overhead
- Variable payload sizes expose buffer handling issues, memory allocation patterns
- Validates data integrity (echoed data should match sent data)
- Stress testing for connection/memory limits
- Format: Length-prefixed binary (`<length>\r\n<data>` → same format back)

**Design decision:** Echo uses length-prefixed binary rather than delimiter-based to allow testing with arbitrary binary payloads and predictable framing.

#### Implementation Pattern

Both protocols follow the established vertical slice pattern:
- `src/protocols/{name}/mod.rs` — Module with use case documentation
- `src/protocols/{name}/handler.rs` — Async Tokio handler
- `src/runtime/protocol.rs` — Synchronous `process_{name}` for native runtime

Protocol dispatch added to:
- `src/server.rs` — Tokio runtime dispatch
- `src/runtime/kqueue/event_loop.rs` — kqueue dispatch
- `src/runtime/mod.rs` — Config-to-runtime protocol mapping

#### Context Compaction and Discussion Preservation

Identified a gap in the development workflow: context compaction happened mid-session without the `milestone-documentation` skill being invoked, losing detailed discussion from prior sessions.

**Problem:** The model has no visibility into context usage. Cannot proactively archive discussions before compaction occurs.

**Explored solutions:**
1. **MCP server with ContextInfo tool** — Could query context usage, but `/context` is internal to Claude Code, not accessible externally
2. **Tie documentation to commits** — Works for code changes, misses valuable discussions that don't result in commits
3. **Periodic checkpoints** — Requires manual discipline or external automation

**Outcome:** Filed feature request on Claude Code GitHub (issue #16510) requesting:
- System reminder injection at context thresholds (60%, 75%, 90%)
- Or built-in `ContextInfo` tool the model can query
- Or MCP-accessible endpoint for custom solutions

### Decisions Made

1. **Ping/Echo as test protocols** — Not production protocols; categorized separately in `src/protocols/mod.rs`
2. **Length-prefixed Echo** — Binary-safe, predictable framing
3. **Document in module docstrings** — Use cases captured in `src/protocols/{ping,echo}/mod.rs`
4. **Feature request for context visibility** — Issue #16510 opened

### Files Changed

**New files:**
- `src/protocols/ping/mod.rs` — Module with use case documentation
- `src/protocols/ping/handler.rs` — Async handler (PING/PONG/QUIT)
- `src/protocols/echo/mod.rs` — Module with protocol format documentation
- `src/protocols/echo/handler.rs` — Async handler (length-prefixed echo)

**Modified files:**
- `src/config.rs` — Added `Ping`, `Echo` to `ProtocolType` enum
- `src/protocols/mod.rs` — Added module exports, categorized test vs production
- `src/server.rs` — Added Tokio dispatch for new protocols
- `src/runtime/protocol.rs` — Added `Protocol::Ping/Echo`, `process_ping`, `process_echo`
- `src/runtime/mod.rs` — Added protocol type mapping
- `src/runtime/kqueue/event_loop.rs` — Added kqueue dispatch

### Open Items

1. **Test the protocols manually** — Verify Ping/Echo work with both Tokio and native runtimes
2. **Add integration tests** — Automated tests for new protocols
3. **Context visibility** — Awaiting response on issue #16510; meanwhile, be more diligent about invoking milestone-documentation

---

## Session: 2026-01-07 - Runtime Module Refactoring

### Context
Exploring shared abstractions for the runtime module to reduce code duplication between io_uring and mio backends. The main entities are: workers/threads, connections, and buffers—currently siloed between backends.

### Key Points Explored

#### Scope: Which Runtimes?
**Question:** Should Tokio be included in the shared abstraction?

**Decision:** Leave Tokio out. Tokio's async/await model is fundamentally different from the synchronous event loops in io_uring and mio. Forcing them into the same abstraction would create awkward impedance mismatches.

#### Trait Overhead
**Question:** Is trait overhead acceptable for runtime abstractions?

**Analysis:** The overhead is vtable indirection—one extra memory lookup per virtual method call (~1-5ns). For operations that already involve syscalls (100s of ns) and network I/O (µs-ms), this is negligible.

**Decision:** Traits are acceptable. Cleaner code architecture wins over micro-optimization at this level.

#### Control Plane vs Data Plane Separation

User introduced the networking concept of separating control plane from data plane:

- **Control plane**: Connection lifecycle management (accept, TLS handshake, close)
- **Data plane**: Request processing on established connections (read, process, write)

This maps naturally to a two-level state machine:

```rust
/// Control plane: connection lifecycle phases
enum ConnPhase {
    Accepting,
    Handshaking,  // future TLS support
    Established(DataState),
    Closing,
}

/// Data plane: request processing state
enum DataState {
    Reading { filled: usize },
    Writing { buf_idx: usize, written: usize, total: usize },
}
```

**Benefits:**
1. Type system enforces data operations only on established connections
2. Clear handoff point for future worker specialization (accept workers vs request workers)
3. Natural fit for TLS handshake phase

#### Worker Model
Each worker is a thread with:
- Own listener socket (SO_REUSEPORT for kernel load balancing)
- Own event loop
- Own resources (buffers, connection registry)

Kernel distributes incoming connections across workers. Workers don't share connections.

#### Module Naming Confusion
`protocol.rs` was confusing because `protocols/` directory contains the actual protocol implementations (parsers, handlers).

**Options considered:**
1. `protocol_dispatch.rs` — verbose
2. `dispatch.rs` — unclear what's being dispatched
3. `request.rs` — matches the actual content (request processing)

**Decision:** Rename to `request.rs`. The file contains synchronous request processing adapters for the native runtime.

### Implementation

Files changed:
- `src/runtime/protocol.rs` → `src/runtime/request.rs`
- `src/runtime/connection.rs` — Refactored with `ConnPhase`/`DataState`, made shared (removed `#[cfg(target_os = "linux")]`)
- `src/runtime/token.rs` → `src/runtime/uring/token.rs` (io_uring-specific)
- `src/runtime/mod.rs` — Updated exports
- `src/runtime/uring/event_loop.rs` — Updated for new state types
- `src/runtime/mio/event_loop.rs` — Uses shared `DataState`

New module structure:
```
runtime/
├── mod.rs           # Shared entry point
├── buffer.rs        # Shared BufferPool
├── connection.rs    # Shared Connection, ConnPhase, DataState, ConnectionRegistry
├── request.rs       # Shared request dispatch
├── mio/             # mio backend (epoll/kqueue)
│   ├── mod.rs
│   └── event_loop.rs
└── uring/           # io_uring backend (Linux)
    ├── mod.rs
    ├── buf_ring.rs
    ├── event_loop.rs
    └── token.rs
```

### Functional Tests

Created `tests/functional.sh` to test all protocol × runtime combinations:
- 4 protocols: Ping, Echo, Memcached, RESP
- 2 runtimes: native (io_uring), mio (epoll)
- 18 test cases total

All tests pass.

### Decisions Made

1. **Exclude Tokio** from shared abstraction (different async model)
2. **Use traits** for runtime abstraction (overhead negligible)
3. **Separate control/data plane** with `ConnPhase`/`DataState` enums
4. **Rename `protocol.rs`** → `request.rs`
5. **Move `token.rs`** into `uring/` (io_uring-specific)
6. **Share `connection.rs`** between io_uring and mio

### Open Items

1. **Worker trait** — Not yet implemented; each backend still has its own event loop entry point
2. **Shared buffer abstraction** — `BufferPool` exists but backends use it differently
3. **TLS support** — `Handshaking` phase ready but not implemented

---
