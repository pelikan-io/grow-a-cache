# Assumptions

Last updated: 2026-01-08

## Workload

1. Read-heavy workload (>80% reads) — typical cache usage pattern
2. Key sizes < 250 bytes — Memcached spec limit
3. Value sizes typically < 1MB, hard limit configurable up to 8MB default — larger values are rejected early at protocol parse time to bound memory and latency; 8MB chosen to avoid confusion with memcached's 1MB slab limit
4. Uniform key distribution — hot keys could invalidate sharding assumptions
5. Single-key operations dominate — MGET/MSET less common
6. Multi-key operations bounded — configurable limit on keys per MGET/MSET for predictable latency
7. Large values (> buffer_size) may arrive across multiple TCP packets — protocol handlers must accumulate partial reads

## Environment

1. Linux and macOS production targets — Linux for high-scale deployments, macOS for smaller workloads and development
2. 4-64 CPU cores typical — cloud VM sizing
3. Memory: 1GB - 128GB per instance
4. Dedicated cache servers — not shared pods with noisy neighbors
5. TCP only — no Unix sockets, no TLS termination at server
6. Clients use connection pooling — low connection churn expected

## Operations

1. Target: 10K-100K concurrent connections
2. Target: 200K QPS per core — scales linearly with core count
3. Key cardinality: 1M-100M keys — current O(n) LRU scan won't work at scale
4. p99.9 latency < 2ms — tail latency target, affects lock vs channel trade-off
5. No persistence required — cache, not database
6. No replication/clustering — single-node design for v1/v2 scope
7. Eviction is a core capability — cache replaces old/stale data under memory pressure, not a failure mode
8. Memory usage bounded by buffer pool size — io_uring provided buffers and write buffers come from fixed pools; no dynamic allocation on hot path
9. Large value handling adds per-connection state — io_uring requires accumulation buffer per connection for partial reads; mio can read directly into connection buffer

## Others

1. Memcached text protocol and RESP are equally supported — no preferred protocol
2. Pipelining supported — both protocols allow it, responses in request order (no out-of-order execution)
3. No authentication/authorization — trusted network environment
4. No rate limiting — well-behaved clients assumed

---

## Open Questions

1. Hot key distribution in real workloads?
2. What should the default limit be for multi-key operations?
3. Should streaming responses (for large GET values) write directly from storage or copy to buffer first? — v3 uses BufferChain copy; zero-copy streaming deferred to v4
4. What's the optimal buffer pool partitioning between connection I/O and value accumulation? — current suggestion is 75% chains / 25% connections
