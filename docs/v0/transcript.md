# Cache Architecture Discussion

## Session Overview

This document captures a technical discussion about cache server threading, storage architectures, and benchmarking tools for building a high-performance KV cache system.

---

## [Yao] How well does the current server work with multiple threads?

### Response

The server uses **Tokio's async runtime** with cooperative multitasking rather than traditional OS-level threading. Here's what was found:

#### How It Works

- **Runtime**: Tokio multi-threaded runtime (one worker thread per CPU core by default)
- **Connection handling**: Each connection spawns as an independent async task via `tokio::spawn()`
- **Connection limit**: Semaphore-based, max 10,000 concurrent connections
- **Shared state**: `Arc<Storage>` with `RwLock<HashMap>` for cache data and atomic counters

#### Key Issues Identified

| Issue | Severity | Description |
|-------|----------|-------------|
| **Unused worker config** | Medium | The `workers` config field is parsed but never applied to Tokio runtime |
| **RwLock contention** | Medium | Two separate RwLocks (`data` and `access_order`) create potential bottlenecks under high load |
| **Expensive atomics** | Low | All atomic operations use `SeqCst` (strongest/slowest ordering) when `Relaxed` would suffice for counters |
| **TOCTOU races** | Medium | Memory limit checks happen before acquiring exclusive locks, allowing limit to be exceeded |
| **Blocking in async** | Medium | Uses `std::sync::RwLock` instead of `tokio::sync::RwLock`, which blocks the thread |
| **Lock cycling** | Medium | Operations like `cas()` and `append()` drop and re-acquire locks, creating race windows |

#### What Works Well

- **I/O-bound workloads**: Excellent for typical cache operations
- **Memory safety**: Rust's type system guarantees thread safety
- **Connection management**: Semaphore-based limiting prevents overload
- **Work stealing**: Tokio's scheduler efficiently distributes tasks across cores

#### What Could Be Improved

1. Replace `std::sync::RwLock` with `tokio::sync::RwLock` for proper async-aware locking
2. Actually use the `workers` config to customize thread count
3. Consolidate `data` and `access_order` into a single lock to reduce contention
4. Use atomic compare-and-swap for memory accounting to prevent TOCTOU races
5. Reduce atomic ordering from `SeqCst` to `Relaxed` where possible

#### Bottom Line

The server works reasonably well under typical loads, but will show performance degradation under sustained high concurrency due to lock contention and synchronization overhead. The architecture is correct for safety but not optimized for maximum throughput.

---

## [Yao] How would you design the architecture if we need to scale to 64 or even 128 threads?

### Response

Scaling to 64-128 threads requires fundamentally rethinking the architecture to minimize contention. Here's the proposed design:

#### 1. Sharded Storage (Most Critical Change)

The single `RwLock<HashMap>` is the primary bottleneck. Replace with sharded design:

```rust
pub struct ShardedStorage {
    shards: Box<[Shard]>,          // Fixed array of shards
    shard_mask: usize,             // For fast modulo (power of 2)
    global_memory: AtomicU64,      // Approximate global tracking
    max_memory: usize,
}

pub struct Shard {
    data: RwLock<HashMap<String, CacheItem>>,
    access_order: RwLock<BTreeMap<u64, String>>,  // Ordered for LRU
    memory_used: AtomicU64,
    access_counter: AtomicU64,
}
```

**Why this helps**: With 256 shards and 128 threads, probability of two threads hitting the same shard is ~50% per operation, vs 100% with single lock.

#### 2. Shard Count Recommendations

| Threads | Recommended Shards | Contention Probability |
|---------|-------------------|----------------------|
| 64 | 256-512 | ~12-25% |
| 128 | 512-1024 | ~12-25% |

Rule of thumb: **4-8x the thread count**

#### 3. Lock-Free Read Path

For read-heavy workloads, consider using `DashMap` for fine-grained locking per bucket.

#### 4. Approximate Memory Accounting

Use eventual consistency instead of strict atomic accounting to remove global serialization points.

#### 5. Per-Shard LRU Instead of Global LRU

Global LRU requires cross-shard coordination. Use per-shard LRU with round-robin eviction.

#### 6. Thread-per-Core with Connection Affinity

Configure Tokio with explicit thread count and pin connections to specific workers for cache locality.

#### 7. Relaxed Atomic Ordering

Replace `SeqCst` with `Relaxed` for counters that don't need cross-thread visibility guarantees.

#### 8. Batch Operations for Multi-Key Commands

Group keys by shard to reduce lock acquisitions for `get_multi`.

#### Summary of Changes by Impact

| Change | Implementation Effort | Performance Impact |
|--------|----------------------|-------------------|
| Sharded storage | Medium | **Very High** |
| Per-shard LRU | Low | High |
| Relaxed atomics | Low | Medium |
| DashMap/lock-free reads | Low | High |
| Approximate memory accounting | Medium | Medium |
| Connection affinity | Medium | Medium |
| Batch key grouping | Low | Medium |
| Custom thread count | Low | Low |

---

## [Yao] Survey the existing storage design in open source KV cache projects, and assess how well they will work for our requirement. In addition to concurrency, we also want to handle a wide range of object sizes, from several bytes to megabytes

### Response

A comprehensive research document was created at `/home/user/grow-a-cache/RESEARCH_KV_CACHE_ARCHITECTURES.md` covering six major systems:

#### Assessment Matrix

| System | Thread Scalability (64-128) | Small Objects (bytes) | Large Objects (MB) | Overall Fit |
|--------|---------------------------|----------------------|-------------------|-------------|
| **Dragonfly** | ⭐⭐⭐⭐⭐ Excellent | ⭐⭐⭐ Good | ⭐⭐⭐⭐ Good | **Best for concurrency** |
| **Pelikan Segcache** | ⭐⭐⭐⭐ Very Good | ⭐⭐⭐⭐⭐ Excellent | ⭐⭐ Poor | **Best for small objects** |
| **Garnet** | ⭐⭐⭐⭐ Very Good | ⭐⭐⭐ Good | ⭐⭐⭐⭐⭐ Excellent | **Best for mixed sizes** |
| **Memcached** | ⭐⭐ Fair | ⭐⭐ Fair | ⭐⭐⭐ Good | Legacy choice |
| **KeyDB** | ⭐⭐ Fair | ⭐⭐⭐ Good | ⭐⭐⭐ Good | Limited to ~4 threads |
| **Redis** | ⭐ Poor | ⭐⭐⭐ Good | ⭐⭐⭐ Good | Single-threaded |

#### Key Findings

**Threading Models - Three Generations:**
1. **First Generation (Redis):** Pure single-threaded - simplicity over parallelism
2. **Second Generation (Memcached, KeyDB):** Worker thread pools with locks
3. **Third Generation (Dragonfly, Pelikan, Garnet):** Advanced concurrency with shared-nothing or epoch-based designs

**Memory Allocation Strategies:**
- **Memcached**: Slab allocator with 56 bytes metadata per object
- **Pelikan Segcache**: 5 bytes metadata per object (91% reduction!)
- **Garnet**: Dual-store architecture for strings vs complex objects

#### Recommended Hybrid Architecture

Given requirements for both high thread scaling and variable object sizes:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Hybrid Storage Architecture                   │
├─────────────────────────────────────────────────────────────────┤
│  Shared-Nothing Thread Model (from Dragonfly)                   │
│  - 64-128 shards, one per thread                                │
│  - Message passing for cross-shard operations                   │
│                                                                 │
│  Small Object Store (from Pelikan)                              │
│  - Segment-based, 5-byte metadata                               │
│  - TTL grouping, Objects < 4KB                                  │
│                                                                 │
│  Large Object Store (from Garnet)                               │
│  - Per-shard arena, Size-class allocation                       │
│  - Pointer for ≥4KB objects                                     │
└─────────────────────────────────────────────────────────────────┘
```

---

## [Yao] Segcache allows segment sizes to be increased, and has a datapool abstraction to allow SSD as the KV storage medium, does that change your conclusion?

### Response

Yes, this significantly changes the assessment.

#### What the Datapool Abstraction Provides

| Backend | Description | Use Case |
|---------|-------------|----------|
| **SHM** | Shared memory (anonymous) | RAM-based caching |
| **PMEM** | Persistent/file-backed memory | SSD or NVMe storage |

#### How Configurable Segment Sizes Help

With larger segment sizes, the economics change:

```
Default segment: 1MB
  - 10,000 x 100-byte objects → 5 bytes overhead each
  - 1 x 1MB object → 5 bytes overhead (negligible)

Larger segment: 4MB
  - Can store multiple MB-sized objects per segment
  - TTL grouping still works
```

#### Revised Viability

| Requirement | Original Assessment | Revised Assessment |
|-------------|--------------------|--------------------|
| **Small objects** | ⭐⭐⭐⭐⭐ Excellent | ⭐⭐⭐⭐⭐ Excellent |
| **Large objects** | ⭐⭐ Poor | ⭐⭐⭐⭐ Good with larger segments |
| **SSD support** | Not considered | ⭐⭐⭐⭐ Good via PMEM datapool |

**Updated Recommendation**: Segcache with configurable segments + PMEM datapool is now the top recommendation over the hybrid architecture.

---

## [Yao] How about the storage design in Garnet from Microsoft? It supports tiered storage

### Response

Deep dive into Garnet's storage design revealed the **HybridLog** architecture from FASTER:

#### HybridLog Design

```
TAIL (Hot)              HEAD (Warm)           STORAGE (Cold)
┌──────────────┐       ┌──────────────┐       ┌──────────────┐
│   Mutable    │       │  Read-Only   │       │    On-Disk   │
│    Region    │       │    Region    │       │    Region    │
│  In-place    │       │ "2nd chance" │       │  Read-copy-  │
│   updates    │       │  for hot     │       │   update     │
│  (RAM)       │       │  (RAM)       │       │  (SSD/Cloud) │
└──────────────┘       └──────────────┘       └──────────────┘
```

#### Garnet's Dual Store Architecture

1. **Main Store**: Optimized for raw string operations, avoids GC
2. **Object Store**: Handles complex objects (Sorted Sets, Lists, Hashes)

#### Tiered Storage Configuration

```bash
garnet-server \
  --storage-tier true \
  --memory 128m \
  --logdir /ssd/garnet \
  --segment 1g
```

#### Comparison: Garnet vs Segcache

| Aspect | Garnet/Tsavorite | Pelikan Segcache |
|--------|------------------|------------------|
| **Tiered Storage** | ⭐⭐⭐⭐⭐ Native, automatic | ⭐⭐⭐⭐ Via PMEM datapool |
| **Hot/Cold Separation** | ⭐⭐⭐⭐⭐ Automatic via HybridLog | ⭐⭐⭐ Manual (TTL-based) |
| **Small Object Overhead** | ⭐⭐⭐ 24+ bytes/record | ⭐⭐⭐⭐⭐ 5 bytes/object |
| **Update Strategy** | ⭐⭐⭐⭐⭐ In-place (hot), RCU (cold) | ⭐⭐⭐ Append-only log |

---

## [Yao] How does Garnet handle tiering automatically?

### Response

Garnet uses the **HybridLog** from FASTER which maintains three logical regions separated by address pointers:

#### The Three Regions

```
    HeadAddress      ReadOnlyAddress              TailAddress
        │                  │                           │
        ▼                  ▼                           ▼
    ════╪══════════════════╪═══════════════════════════╪════
        │   STABLE         │   READ-ONLY    │  MUTABLE │
        │   (On Disk)      │   (RAM)        │  (RAM)   │
        │  Read-Copy-      │  "2nd Chance"  │ In-Place │
        │  Update (RCU)    │  before evict  │ Updates  │
```

#### How Records Automatically Migrate

1. **Record Creation**: New record at TailAddress in MUTABLE region
2. **Natural Aging**: As more records added, old records cross into READ-ONLY
3. **Eviction to Disk**: Memory fills up, records cross HeadAddress to STABLE
4. **Re-heating**: Cold record accessed → copied back to TailAddress

#### The MutableFraction Setting

```
MutableFraction = 0.9 (default)

Total Memory: 1GB
├── Mutable Region:   900MB (90%) - in-place updates
└── Read-Only Region: 100MB (10%) - "second chance" buffer
```

#### Why This Is "Automatic"

| Aspect | How It's Automatic |
|--------|-------------------|
| **No manual tiering rules** | Address advancement happens naturally as log grows |
| **No explicit hot/cold marking** | Position in log = temperature |
| **Self-optimizing** | Hot records copied back to tail when accessed |
| **Workload-adaptive** | Zipf distributions naturally cluster hot data |

---

## [Yao] How does FASTER compare to S3-FIFO, whose main FIFO queue can be put on SSD/cloud?

### Response

S3-FIFO is a compelling alternative approach using **three FIFO queues** with a filtering mechanism:

#### S3-FIFO Architecture

```
S3-FIFO uses three FIFO queues:
- Small Queue (S) - 10% - Filter for one-hit wonders
- Main Queue (M) - 90% - Proven valuable objects
- Ghost Queue (G) - Metadata only - Tracks evicted IDs
```

#### Key Insight: Quick Demotion

> "Most objects in Zipf workloads will only be accessed once in a short window, so it is critical to evict them early."

#### FASTER vs S3-FIFO Comparison

| Aspect | FASTER HybridLog | S3-FIFO |
|--------|------------------|---------|
| **Core structure** | Single log with 3 regions | 3 separate FIFO queues |
| **Update strategy** | In-place (mutable region) | Append-only (FIFO) |
| **Flash writes** | Mixed (in-place + append) | Sequential only (FIFO) |
| **Write amplification** | Higher (RCU for cold data) | **WAF ≈ 1** |
| **Scalability** | Good (epoch protection) | **6× better than LRU** |

#### Flash/SSD Friendliness

**S3-FIFO on SSD**:
- Write Pattern: Pure Sequential
- All queues: FIFO (append-only)
- Eviction order = insertion order
- **Write amplification ≈ 1**

From Solidigm's implementation:
> "The KV store's primary queue can operate at **10 GB/s bandwidth**."

#### Recommendations

| Use Case | Better Choice | Why |
|----------|---------------|-----|
| **Update-heavy, hot working set** | FASTER | In-place updates faster |
| **Flash/SSD as primary storage** | **S3-FIFO** | WAF=1, sequential writes |
| **Maximum scalability** | **S3-FIFO** | Lock-free FIFO queues |

---

## [Yao] Make a list of cache benchmarking projects in the open.

### Response

#### Load Generators / Stress Testing

| Project | Description | Protocols |
|---------|-------------|-----------|
| **memtier_benchmark** | High-throughput benchmarking tool from Redis | Redis, Memcached |
| **rpc-perf** (IOP Systems) | Twitter's RPC benchmarking tool (Rust) | Redis, Memcached, Kafka, gRPC |
| **YCSB** | Yahoo Cloud Serving Benchmark | Redis, Memcached, many DBs |
| **twemperf/mcperf** | Twitter's memcached performance tool | Memcached ASCII |
| **mutilate** | High-fidelity memcached load generator | Memcached |
| **valkey-benchmark** | Built-in Valkey/Redis benchmarking | Valkey/Redis |
| **cache-benchmarks** | Comparative benchmarks | Redis, Memcached, Valkey, Dragonfly, Garnet |

#### Cache Simulators / Algorithm Benchmarks

| Project | Description | Algorithms |
|---------|-------------|------------|
| **libCacheSim** | High-performance simulator (20M req/sec) | LRU, LFU, ARC, FIFO, S3-FIFO, SIEVE, TinyLFU |
| **CacheBench** | Meta's cache benchmark for CacheLib | CacheLib algorithms |
| **Caffeine Simulator** | Eviction policy simulator | W-TinyLFU, LRU, LFU |

#### Trace Datasets

| Dataset | Source | Description |
|---------|--------|-------------|
| **Twitter Cache Traces** | Twitter (2020) | 54 production clusters, 1-week traces |
| **CMU Cache Datasets** | CMU PDL | Collection from multiple sources |
| **S3-FIFO Traces** | CMU (SOSP'23) | 6594 traces from 14 datasets |

---

## [Yao] What about the fork of rpc-perf maintained by IOP Systems?

### Response

The [IOP Systems fork of rpc-perf](https://github.com/iopsystems/rpc-perf) is the actively maintained version with significant enhancements:

#### IOP Systems rpc-perf

| Aspect | Details |
|--------|---------|
| **Maintainer** | IOP Systems (Brian Martin, formerly Twitter) |
| **Language** | Rust |
| **License** | Apache-2.0 / MIT |

#### Supported Protocols

| Protocol | Use Case |
|----------|----------|
| **Memcached** | Cache benchmarking |
| **Redis** | Cache/data store benchmarking |
| **Kafka** | PubSub benchmarking |
| **gRPC** | General RPC benchmarking |
| **HTTP/1.1, HTTP/2** | Web service benchmarking |
| **Momento** | Momento cache service |

#### Key Features

- **Waterfall visualization** - Latency distribution over time
- **High-resolution histograms** - Detailed percentile tracking
- **OpenTelemetry integration** - Export metrics to observability platforms

#### vs Original Twitter Version

| Feature | Twitter rpc-perf | IOP Systems rpc-perf |
|---------|------------------|----------------------|
| Active development | Minimal | ✓ Active (v5.23.0+) |
| Momento support | ✗ | ✓ |
| Kafka support | ✗ | ✓ |
| OpenTelemetry | ✗ | ✓ |

---

## [Yao] Examine the source code of these projects, and list the ones that are potentially susceptible to coordinated omission and under-reporting of tail latencies

### Response

#### Coordinated Omission Susceptibility Assessment

| Tool | CO Susceptible? | Mitigation Available? | Risk Level |
|------|-----------------|----------------------|------------|
| **valkey-benchmark / redis-benchmark** | ✅ Yes | ❌ No | 🔴 **HIGH** |
| **twemperf / mcperf** | ✅ Yes | ❌ No | 🔴 **HIGH** |
| **memtier_benchmark** | ✅ Yes (default) | ⚠️ Partial (`--rate-limiting`) | 🟡 **MEDIUM** |
| **rpc-perf (IOP Systems)** | ⚠️ Unclear | ⚠️ Has rate limiting | 🟡 **MEDIUM** |
| **YCSB** | ✅ Yes (default) | ✅ Yes (`measurement.interval=intended`) | 🟢 **LOW** (if configured) |
| **mutilate** | ✅ Yes (default) | ✅ Yes (`--measure_depth`) | 🟢 **LOW** (if configured) |
| **wrk2** | ❌ No | ✅ Native | 🟢 **REFERENCE** |
| **libCacheSim** | N/A | N/A | ⬜ **N/A** (simulator) |

#### The Coordinated Omission Problem

```
CLOSED-LOOP (susceptible):
T=50ms:  Send R50 ────────[STALL 100ms]───────────► Response (100ms)
T=150ms: Send R51 ────────────────────────────────► Response (1ms)
         ▲
         └── Requests R51-R150 NEVER SENT during stall

Recorded: 49 × 1ms, 1 × 100ms
Reported p99: ~1ms  ← WRONG!

OPEN-LOOP (correct):
Includes queue wait time for all requests
Reported p99: ~100ms  ← CORRECT!
```

#### Real-World Impact (YCSB)

- `[READ] 99th`: 647μs (uncorrected)
- `[Intended-READ] 99th`: **665,087μs** (corrected) — **1000x difference!**

#### Key Takeaway

> **Any benchmark reporting p99 < 10ms while the server experienced multi-second stalls is lying to you.**

---

## [Yao] examine rpc-perf source code to get a sense of if it's closed-loop load generation

### Response

Based on source code examination:

#### Key Findings

**1. Load Generation Model: Open-Loop (Fire-and-Forget)**

From `src/workload/mod.rs`:
```rust
// Workload threads run continuously, generating requests
// Does NOT wait for responses before sending next request
```

**2. Rate Limiting: Token Bucket**

```rust
if let Some(ref ratelimiter) = self.ratelimiter {
    loop {
        if ratelimiter.try_wait().is_ok() {
            break;
        }
    }
}
// Send request immediately after rate limit permits
```

**3. Missing: Intended Send Time Tracking**

No evidence found of:
- Calculating `intended_send_time` based on target rate
- Recording `latency = response_time - intended_send_time`
- Coordinated omission correction

#### Verdict: **Partial Susceptibility**

| Aspect | rpc-perf | wrk2 (reference) |
|--------|----------|------------------|
| Load generation | ✅ Open-loop | ✅ Open-loop |
| Waits for response | ✅ No | ✅ No |
| Records intended time | ❌ No evidence | ✅ Yes |
| CO correction | ❌ No evidence | ✅ Yes |

**Risk Level: 🟡 MEDIUM**

rpc-perf is significantly better than closed-loop tools, but for accurate tail latency measurement under stress, prefer wrk2, YCSB with `measurement.interval=intended`, or mutilate with `--measure_depth`.

---

## [Yao] Looks at the source code of pelikan-io/pelikan on Github, and draw a architectural diagram

### Response

```
┌─────────────────────────────────────────────────────────────────────────────────────┐
│                              PELIKAN CACHE FRAMEWORK                                │
│                     "Runtime separation of control and data plane"                  │
└─────────────────────────────────────────────────────────────────────────────────────┘

                                  NETWORK LAYER
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │                              TCP Listener                                    │
  │                         (Server Thread - Data Plane)                         │
  └───────────────────────────────────┬─────────────────────────────────────────┘
                                      │
                    ┌─────────────────┼─────────────────┐
                    │   Connection    │   Lockless      │
                    │   Hand-off      │   Ring Array    │
                    ▼                 ▼                 ▼
  ┌──────────────────────────────────────────────────────────────────────────────┐
  │                         WORKER THREADS (Data Plane)                          │
  │  ┌────────────┐ ┌────────────┐ ┌────────────┐ ┌────────────┐ ┌────────────┐  │
  │  │  Worker 0  │ │  Worker 1  │ │  Worker 2  │ │  Worker 3  │ │  Worker 4  │  │
  │  │ • I/O      │ │ • I/O      │ │ • I/O      │ │ • I/O      │ │ • I/O      │  │
  │  │ • Parsing  │ │ • Parsing  │ │ • Parsing  │ │ • Parsing  │ │ • Parsing  │  │
  │  │ • Session  │ │ • Session  │ │ • Session  │ │ • Session  │ │ • Session  │  │
  │  └─────┬──────┘ └─────┬──────┘ └─────┬──────┘ └─────┬──────┘ └─────┬──────┘  │
  │        └──────────────┴──────────────┼──────────────┴──────────────┘         │
  │                          Non-blocking, Lockless                              │
  └──────────────────────────────────────┼───────────────────────────────────────┘
                                         │
                              PROTOCOL LAYER
  ┌──────────────────────────────────────┼──────────────────────────────────────┐
  │  ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐ ┌───────────┐      │
  │  │ Memcache  │ │   RESP    │ │   HTTP    │ │   Ping    │ │   Admin   │      │
  │  │ Protocol  │ │  (Redis)  │ │ Protocol  │ │ Protocol  │ │ Protocol  │      │
  │  └───────────┘ └───────────┘ └───────────┘ └───────────┘ └───────────┘      │
  └──────────────────────────────────────┬──────────────────────────────────────┘
                                         │
                              STORAGE THREAD
                    (Serialized Access - Eliminates Data Races)
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │                            SEGCACHE ENGINE                                   │
  │  ┌─────────────────────────────────────────────────────────────────────┐    │
  │  │                      HASH TABLE                                      │    │
  │  │              key → (segment_id, offset)                              │    │
  │  └─────────────────────────────────────────────────────────────────────┘    │
  │                              │                                               │
  │  ┌─────────────────────────────────────────────────────────────────────┐    │
  │  │                    TTL BUCKETS                                       │    │
  │  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐             │    │
  │  │  │ 0-60s    │  │ 60s-5min │  │ 5min-1hr │  │  1hr+    │             │    │
  │  │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘             │    │
  │  │       ▼             ▼             ▼             ▼                   │    │
  │  │  ┌─────────────────────────────────────────────────────┐            │    │
  │  │  │              SEGMENT CHAINS                         │            │    │
  │  │  │  ┌─────┐   ┌─────┐   ┌─────┐   ┌─────┐             │            │    │
  │  │  │  │Seg 0│──▶│Seg 1│──▶│Seg 2│──▶│Seg 3│──▶ ...      │            │    │
  │  │  │  │1-2MB│   │1-2MB│   │1-2MB│   │1-2MB│             │            │    │
  │  │  │  └─────┘   └─────┘   └─────┘   └─────┘             │            │    │
  │  │  │  Per-object: 5 bytes only! (vs 56 bytes Memcached) │            │    │
  │  │  └─────────────────────────────────────────────────────┘            │    │
  │  └─────────────────────────────────────────────────────────────────────┘    │
  │  ┌───────────────┐  ┌───────────────┐                                       │
  │  │  BLOOM FILTER │  │   DATATIER    │                                       │
  │  │  (optional)   │  │  (SSD/PMEM)   │                                       │
  │  └───────────────┘  └───────────────┘                                       │
  └─────────────────────────────────────────────────────────────────────────────┘

                           CONTROL PLANE (Separate Thread)
  ┌─────────────────────────────────────────────────────────────────────────────┐
  │                              ADMIN THREAD                                    │
  │  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  ┌─────────────┐   │
  │  │ Stats         │  │ Stats         │  │ Log           │  │ Config      │   │
  │  │ Aggregation   │  │ Exporting     │  │ Dump          │  │ Reload      │   │
  │  │ • Atomic      │  │ • Prometheus  │  │ • Ring Buffer │  │ • Hot       │   │
  │  │   counters    │  │ • JSON        │  │ • Waitless    │  │   reload    │   │
  │  └───────────────┘  └───────────────┘  └───────────────┘  └─────────────┘   │
  └─────────────────────────────────────────────────────────────────────────────┘
```

#### Key Design Principles

| Principle | Implementation |
|-----------|----------------|
| **Data/Control Plane Separation** | Worker threads (data) vs Admin thread (control) |
| **Lockless Data Structures** | Ring buffers, atomic counters, serialized storage access |
| **Worker Never Blocks** | Non-blocking I/O, async operations |
| **Serialized Storage Access** | Single storage thread eliminates data races |
| **Memory Efficiency** | Segcache: 5 bytes/object metadata, TTL-based grouping |

---

## Summary

This discussion covered:

1. **Current server threading analysis** - Identified RwLock contention and TOCTOU issues
2. **Scaling to 64-128 threads** - Proposed sharded architecture with per-shard LRU
3. **Open source KV cache survey** - Compared Dragonfly, Pelikan, Garnet, Memcached, KeyDB, Redis
4. **Segcache with PMEM datapool** - Revised recommendation for mixed object sizes
5. **Garnet's HybridLog tiering** - Automatic hot/cold separation via log position
6. **S3-FIFO comparison** - Flash-friendly design with WAF=1
7. **Benchmarking tools survey** - Listed load generators, simulators, and trace datasets
8. **Coordinated omission analysis** - Identified which tools underreport tail latencies
9. **rpc-perf source examination** - Found open-loop but no intended-time tracking
10. **Pelikan architecture** - Diagrammed data/control plane separation and module structure
