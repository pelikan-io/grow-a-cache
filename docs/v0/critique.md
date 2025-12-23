# v0 Critique

## Requirements Assessed

> A memcached-compatible cache server written in Rust, implementing the text protocol.

Implicit requirements inferred from discussion:
- Should eventually scale to 64-128 threads
- Should handle mixed object sizes (bytes to megabytes)
- Should support tiered storage (RAM + SSD)

## Design Review

### Limitations

1. **Single lock bottleneck.** `RwLock<HashMap>` serializes all storage access. Under high concurrency, this will dominate latency.

2. **Blocking locks in async context.** Uses `std::sync::RwLock` instead of `tokio::sync::RwLock`. A long write operation blocks the Tokio worker thread.

3. **TOCTOU races in memory accounting.** Memory limit checks happen before acquiring exclusive locks, allowing the limit to be exceeded.

4. **Dual-lock overhead.** Separate `RwLock` for `data` and `access_order` doubles locking overhead and creates potential for inconsistency.

5. **Expensive atomics.** All atomic operations use `SeqCst` when `Relaxed` would suffice for counters.

6. **No connection affinity.** Connections bounce between Tokio workers, losing cache locality.

### Assumptions

1. **Workload is I/O-bound.** The design assumes network I/O dominates, not storage contention. Breaks under high request rates.

2. **Objects are small.** No special handling for multi-megabyte values. Large values hold locks longer.

3. **Single-node deployment.** No consideration for clustering, replication, or distributed operation.

4. **RAM-only.** No path to SSD/PMEM backing for capacity beyond RAM.

## Implementation Review

### Gaps

1. **Workers config unused.** The `workers` field is parsed but never applied to Tokio runtime.

2. **No graceful shutdown.** Server runs forever in `loop {}`. No signal handling for clean shutdown.

3. **No metrics export.** Stats command exists but no Prometheus/OpenTelemetry integration.

4. **No connection timeout.** Idle connections persist indefinitely.

5. **Limited stats.** Only basic counters; missing hit/miss rates, latency histograms.

6. **No integration tests.** Unit tests exist but no tests with actual TCP connections.

## Hypothetical Scenarios

### "What if we need to handle 100K requests/second?"

The single RwLock becomes the bottleneck. Based on discussion research, expect degradation beyond ~10K RPS with mixed read/write workloads. Mitigation requires sharding.

### "What if we need to store 100GB of data with 16GB RAM?"

No path to tiered storage. Would require implementing Segcache-style PMEM datapool or HybridLog-style disk tiering. Significant architectural change.

### "What if objects range from 100 bytes to 10MB?"

Current design handles this but without optimization. Large objects hold locks longer, increasing contention. No size-class allocation or separate handling for large values.

### "What if we need sub-millisecond p99 latency?"

Blocking std::sync::RwLock in async context is incompatible with this goal. A single slow operation blocks an entire Tokio worker. Need lock-free reads or sharded architecture.

### "What if we need to add authentication?"

No hooks for auth. Would require protocol extension (memcached SASL or custom) and per-connection state for auth status.

## Recommendations

**Priority 1 (Blocking for production):**
1. Replace `std::sync::RwLock` with `tokio::sync::RwLock` or move to sharded architecture
2. Fix TOCTOU race in memory accounting
3. Add graceful shutdown with signal handling

**Priority 2 (Important):**
4. Consolidate `data` and `access_order` into single lock
5. Actually use `workers` config for Tokio runtime
6. Add connection idle timeout

**Priority 3 (Nice to have):**
7. Relax atomic ordering where possible
8. Add metrics export (Prometheus)
9. Integration tests with real TCP connections
