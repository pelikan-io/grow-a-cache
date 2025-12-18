# Storage Architecture Research: Major Open-Source KV Cache Systems

## Executive Summary

This research examines the storage architecture designs of six major open-source key-value cache systems: Memcached, Redis, Dragonfly, Pelikan, KeyDB, and Garnet. The analysis focuses on threading models, memory allocation strategies, object size handling, and lock contention approaches.

---

## 1. Memcached

### Threading/Concurrency Model
- **Multi-threaded architecture** with a single listener thread accepting connections on port 11211
- Listener thread dispatches connections to a pool of worker threads
- Each worker thread independently handles assigned connections without coordination
- **Queue-based asynchronous I/O** system: I/O operations are queued in thread-specific queues
- Thread subsystem initialized via `memcached_thread_init()` during startup

### Memory Allocation Strategy
**Slab Allocator Design:**
- Memory partitioned into 1MB **pages** at startup
- Each page assigned to a **slab class** (or remains free)
- Pages divided into fixed-size **chunks** based on slab class
- Cannot mix different chunk sizes within the same page

**Slab Class Progression:**
- Smallest chunk: 80 bytes
- Growth factor: 1.25 (configurable with `-f` flag)
- Example progression: 80 → 104 → 128 → 160... → 1MB
- Total of ~40 slab classes by default (can configure ~170 with `-n 5 -f 1.05`)

### Small vs Large Objects
**Small Objects:**
- Designed for larger objects; small objects suffer overhead
- 50-byte object in 80-byte chunk = 30 bytes wasted (37.5% overhead)
- Can optimize with `-n 5 -f 1.05` for 8-byte chunk increments
- Problematic when workload predominantly uses small objects

**Large Objects (>1MB):**
- Items larger than chunk size comprise multiple chunks of largest size
- With 16KB max chunk size, a 16KB+1 byte item uses 32KB
- Can waste significant memory when classes are far apart
- Configurable max item size with `--max-item-size`

### Lock Contention Strategy
- **Global slab lock** (`pthread_mutex_t slabs_lock`) protects allocator access
- Public interface `slabs_alloc()` is thread-safe wrapper
- **Contention issues:**
  - Hash table expansion contention moved to dedicated thread
  - Per-thread stat collection reduces contention
  - Slab rebalancer runs in background thread
- **Known deadlock scenarios** between dispatcher and slab rebalancer threads
- **No fine-grained locking**: All threads blocked during item access

### Key Design Trade-offs
- **Eliminates external fragmentation** but accepts internal fragmentation
- Fragments "converged and locked in fixated memory locations"
- Memory penalty is predictable: unused chunk space cannot be reclaimed
- **Slab calcification problem**: Once memory distributed to size classes, difficult to rebalance

---

## 2. Redis

### Threading/Concurrency Model
**Traditional (pre-6.0):**
- **Pure single-threaded** for all command execution
- Event-driven, non-blocking I/O based on Reactor pattern
- Avoids race conditions, deadlocks, and context switching overhead
- Simplifies codebase for optimization

**Redis 6.0+ I/O Threading:**
- Offloads socket read/write to background threads
- **Single designated thread** still handles main Redis dictionary
- Configurable with `io-threads N` (disabled by default)
- Optional read threading: `io-threads-do-reads yes`
- Can achieve **2x performance** with I/O threading enabled
- Returns processing logic to main thread (unlike Memcached's true thread isolation)

### Memory Allocation Strategy
**Default Allocator: jemalloc**
- jemalloc default on Linux; libc malloc otherwise
- Chosen for **fewer fragmentation problems** than libc malloc
- Modified jemalloc enables active defragmentation
- `--with-lg-quantum 3` (instead of 4) provides more size classes for Redis structures

**Fragmentation Comparison:**
- tcmalloc: 1.01 fragmentation ratio
- jemalloc: 1.02 fragmentation ratio
- libc: 1.31 fragmentation ratio

**Memory Efficiency Features:**
- Dynamic encoding selection based on data size/type
- SDS (Simple Dynamic Strings) for strings
- Ziplist for compact small lists/hashes
- Intset for small integer sets
- Quicklist (hybrid linked list + ziplist) for large lists

### Small vs Large Objects
**Small Objects:**
- jemalloc classes: Tiny (8), Quantum-spaced (16, 32, 48...)
- **Problem:** 24 bytes not an allocation class, but critical Redis structures are 24 bytes
- tcmalloc better here with 8-byte spacing for all small sizes
- Memory footprint much better with jemalloc vs glibc for many small objects

**Large Objects:**
- jemalloc divides memory by powers of two: 8B, 16B, 32B, 2KB, 4KB...
- Chooses closest size and allocates, leading to some waste
- Active defragmentation can reclaim fragmented memory

### Lock Contention Strategy
- **No locks in single-threaded model** (traditional Redis)
- I/O threading (Redis 6.0+) uses threads only for socket I/O
- Main thread serializes all data structure access
- Predictable, atomic command execution
- Trade-off: Limited to one CPU core for command processing

---

## 3. Dragonfly

### Threading/Concurrency Model
**Shared-Nothing Architecture:**
- Multiple threads, each running independent event loop
- Keyspace **partitioned into shards**, one per thread
- Each CPU core responsible for own subset of keys
- **No global lock, no central coordinator**
- Built from ground up for multi-core hardware

**Fiber-based Asynchronous I/O:**
- Each thread manages responsibilities via **stackful fibers** (like goroutines)
- Client connection bound to single thread for entire lifetime
- 100% non-blocking design
- Custom Boost.Fibers scheduler integrated with I/O polling loop
- Based on **helio I/O library** and **io_uring** API

### Memory Allocation Strategy
- Leverages modern **io_uring** Linux API for I/O
- Each shard independently manages its memory
- No shared data structures requiring locks

### Small vs Large Objects
- Shard-level management handles all object sizes
- No special slab classes - uses standard allocators
- Performance optimizations at I/O and threading level, not memory allocator level

### Lock Contention Strategy
**Zero Mutexes/Spinlocks:**
- **Message passing** instead of locks for inter-thread communication
- VLL (Very Lightweight Locking) for transactions
- **Intent locks** declare future intention to acquire control
- **Out-of-order execution** for transactions not blocking on locks

**VLL Transaction Framework:**
- Based on "VLL: a lock manager redesign for main memory database systems" paper
- Locks keys within partition to prevent concurrent access
- No deadlock within partition guaranteed
- Compact intent lock table in every thread
- Multi-key operations achieve atomicity without global lock

**Performance:**
- 6.43 million ops/sec on c7gn.16xlarge (64-core)
- Near-linear scalability with core count
- 25x faster than Redis (marketing claim)

---

## 4. Pelikan (Segcache)

### Threading/Concurrency Model
- **Near-linear scalability** tested up to 24 threads
- Achieves **8x higher throughput** than Memcached at 24 threads
- Similar per-thread throughput as Pelikan's slab storage
- Up to 40% higher than Memcached for Twitter workloads

### Memory Allocation Strategy
**Segment-Structured Design:**
- Object store space divided into fixed-size **segments**
- Each segment is a **small log** storing objects of similar TTLs
- **Key difference from slabs:** Segments store variable-sized objects with similar TTLs, not similar sizes
- Segments in each TTL bucket sorted by creation time

**Three Main Components:**
1. **Object store**: Space allocated for key-values, divided into segments
2. **Hash table**: Maps keys to segment locations
3. **TTL buckets**: Organize segments by expiration time

### Small vs Large Objects
**Small Object Optimization (Primary Use Case):**
- Designed specifically for **small objects** (10s to 1000s of bytes)
- Memcached: **56 bytes metadata per object**
- Segcache: **5 bytes metadata per object** (91% reduction!)
- For 100-byte objects, Memcached wastes 36% on metadata
- Segcache reduces memory footprint by **up to 60%** for small objects

**Metadata Sharing:**
- Objects in same segment share: creation time, approximate TTL, expiration time, reference counter, next segment pointer
- Metadata amortized over 1,000s to 10,000s of objects
- Objects in same hash bucket share: approximate last-access timestamp, CAS value, bucket-level spinlock

**Impact on Miss Ratio:**
- For 45-byte objects: 20-38% reduction in miss ratio
- For typical small objects: 6-8% reduction in miss ratio

### Slab/Segment-Based Design
**Proactive TTL Expiration:**
- Expired objects removed within 1 second
- Segments sorted by expiration time
- Scan first segment of each TTL bucket; if expired, recycle entire segment
- Never scan unexpired segments - minimal wasted computation

**Merge-Based Eviction:**
- Evicts entire segments rather than individual objects
- Merges consecutive segments (more homogeneous)
- Preserves frequently accessed objects
- Almost no memory fragmentation from variable object sizes

### Lock Contention Strategy
**Segment-Level Locking:**
- Locks only when segment created or moved
- Example: 10,000 objects per segment, 10% write ratio
- Locks roughly once every 100,000 requests
- **10,000x reduction** vs. designs locking per request

**Macro Management:**
- Operates on segments, not objects
- Batch operations over contiguous memory
- Immutable segments during read
- Opportunistic eviction

---

## 5. KeyDB

### Threading/Concurrency Model
**Multi-Threaded Event Loop:**
- Runs normal Redis event loop on **multiple threads**
- Network I/O and query parsing done concurrently
- Each connection assigned to thread on `accept()`
- Recommended: **4 threads** (related to network queue count, not core count)
- Default: 2 threads

**Why Higher Thread Count Not Always Better:**
- Uses **spinlocks** to reduce latency
- Too many threads reduces performance
- Should match network hardware queue count

### Memory Allocation Strategy
- Compatible with Redis memory allocators (jemalloc, tcmalloc, libc)
- MVCC architecture for non-blocking queries (KEYS, SCAN)
- Maintains Redis data structure layout for compatibility

### Small vs Large Objects
- Inherits Redis encoding strategies (SDS, Ziplist, Intset, Quicklist)
- Same allocator trade-offs as Redis

### Lock Contention Strategy
**Core Hash Table Spinlock:**
- Access to core hash table guarded by **spinlock**
- Low contention due to extremely fast hash table access
- Transactions hold lock for duration of EXEC command
- Network I/O and parsing avoid lock entirely

**GIL (Global Interpreter Lock) for Modules:**
- GIL acquired only when all server threads paused
- Maintains atomicity guarantees modules expect
- Module compatibility with Redis modules

**Performance Characteristics:**
- Starting at 4 cores, outperforms Redis in all metrics
- 66% more operations per second than Redis
- Replaces complex redis-cluster setup with single process
- No client-side sharding required

---

## 6. Garnet (Microsoft)

### Threading/Concurrency Model
**Thread-Scalable Shared Memory:**
- **Thread-scalable within single node** using Tsavorite
- Network layer based on **shared memory design**
- TLS processing and storage on network I/O completion thread
- Avoids thread switching overhead
- **CPU cache coherence** brings data to processing logic (vs. shuffle-based designs)
- Can use all CPU/memory of server with single instance (no intra-node cluster)

**Inspired by ShadowFax Research:**
- Network layer inherits shared memory design from prior research
- Orders-of-magnitude performance difference for GET/SET operations

### Memory Allocation Strategy
**Two-Tiered Tsavorite Storage:**

**Main Store (String Operations):**
- Optimized for raw string operations (GET, SET, MGET, MSET, etc.)
- Manages memory carefully to **avoid garbage collection**
- Efficient use of memory, reduced GC latency
- Written in modern .NET C#

**Object Store (Complex Types):**
- Optional store for Sorted Set, Set, Hash, List, Geo
- Stored on heap in memory (efficient updates)
- Serialized form on disk
- Leverages .NET library ecosystem
- Can disable with `--no-obj` if using only strings

**Tsavorite Storage Engine (forked from FASTER):**
- Epoch protection for thread safety
- **Space reuse for memory tier** to prevent fragmentation
- Hybrid log-structured design with in-place updates in memory
- Non-blocking checkpointing
- Operation logging for durability

### Small vs Large Objects
**Main Store:**
- Each record typically small, optimized for string operations
- Memory tier holds actual keys and values

**Object Store:**
- Hybrid log holds **references** to objects, not actual objects
- `ObjectStoreLogMemorySize`: controls max records in memory (24 bytes/record)
- `ObjectStoreHeapMemorySize`: controls total heap size for objects
- Setting 1GB log memory = up to 44M records (1GB / 24 bytes)

**Larger-Than-Memory Datasets:**
- Supports spilling to local and cloud storage
- EnableStorageTier flag enables tiered storage

### Lock Contention Strategy
**Epoch Protection:**
- Threads protected via epoch mechanism (from FASTER research)
- **UnsafeContext**: Manual epoch management (BeginUnsafe/EndUnsafe)
- **LockableContext**: Manual locks (BeginLockable/EndLockable)
- **LockableUnsafeContext**: Both manual epoch and manual locking

**Two-Phase Locking for Transactions:**
- Multi-key transactions use 2PL
- Narrow-waist Tsavorite storage API abstracts locking details
- Storage API: read, upsert, delete, atomic RMW
- Clean separation of parsing/query processing from storage concerns

**Deadlock Prevention:**
- Lock spinning limited to prevent deadlocks
- Example: BasicContext spinning on exclusive lock while holding epoch
- BumpCurrentEpoch coordination prevents deadlock scenarios

---

## Comparative Analysis

### Threading Models Summary

| System | Model | Threads | Lock Strategy |
|--------|-------|---------|---------------|
| **Memcached** | Multi-threaded | Worker pool | Global slab lock, per-thread I/O queues |
| **Redis** | Single-threaded (6.0: I/O threads) | 1 main + I/O threads | No locks (single thread), I/O thread coordination |
| **Dragonfly** | Shared-nothing, fiber-based | N shards (per-core) | Message passing, VLL, no mutexes |
| **Pelikan** | Multi-threaded | Up to 24+ tested | Segment-level locks (very rare) |
| **KeyDB** | Multi-threaded event loop | 2-4 recommended | Core hash table spinlock, GIL for modules |
| **Garnet** | Thread-scalable shared memory | Scalable | Epoch protection, 2PL transactions |

### Memory Allocation Strategies

| System | Primary Strategy | Fragmentation Approach |
|--------|------------------|------------------------|
| **Memcached** | Slab allocator (fixed-size chunks) | Accepts internal fragmentation, eliminates external |
| **Redis** | jemalloc (power-of-2 size classes) | Active defragmentation, optimized size classes |
| **Dragonfly** | Per-shard allocation | Shared-nothing reduces contention |
| **Pelikan** | Segment-structured (variable sizes) | Merge-based eviction, almost no fragmentation |
| **KeyDB** | Inherits Redis (jemalloc) | Same as Redis |
| **Garnet** | Tsavorite hybrid log, space reuse | Prevents fragmentation in memory tier |

### Small Object Handling

| System | Metadata Overhead | Best Approach |
|--------|-------------------|---------------|
| **Memcached** | High (56 bytes/object) | Optimize with `-n 5 -f 1.05` for smaller chunks |
| **Redis** | Moderate (encoding-dependent) | Ziplist, Intset for small collections |
| **Dragonfly** | Standard allocator overhead | Sharding reduces per-object management |
| **Pelikan** | **Minimal (5 bytes/object)** | **Best-in-class for small objects** |
| **KeyDB** | Same as Redis | Inherits Redis optimizations |
| **Garnet** | 24 bytes/record (log) | Separate main/object stores |

### Large Object Handling

| System | Approach | Trade-offs |
|--------|----------|------------|
| **Memcached** | Multiple chunks | Can waste significant memory with size mismatches |
| **Redis** | jemalloc power-of-2 | Some waste, active defrag helps |
| **Dragonfly** | Shard-level management | No special handling needed |
| **Pelikan** | Not optimized | Designed for small objects with TTL |
| **KeyDB** | Same as Redis | Inherits Redis behavior |
| **Garnet** | Tiered storage | Can spill to SSD/cloud |

### Scalability Characteristics

| System | Cores | Peak Performance | Scaling Characteristics |
|--------|-------|------------------|-------------------------|
| **Memcached** | Multi-core | High | Contention on slab lock, hash expansion moved to thread |
| **Redis** | 1 (traditional) | ~100K-200K ops/sec | Single-threaded bottleneck |
| **Redis 6.0** | 1+N I/O | ~2x traditional | I/O threading helps, main thread still bottleneck |
| **Dragonfly** | N (all cores) | 6.43M ops/sec (64-core) | Near-linear scaling, 25x+ vs Redis |
| **Pelikan** | 24+ | 8x Memcached (24 threads) | Near-linear to 24 threads |
| **KeyDB** | 4 recommended | 1.66x Redis (66% more) | Spinlock contention at high thread counts |
| **Garnet** | N (all cores) | High | Thread-scalable shared memory |

---

## Key Architectural Insights

### 1. Lock Contention Solutions
- **Memcached:** Global locks, background threads for contention points
- **Redis:** No locks (single-threaded), simplicity over parallelism
- **Dragonfly:** Message passing + VLL, zero mutexes
- **Pelikan:** Segment-level batching (lock once per 10,000s of ops)
- **KeyDB:** Spinlocks on fast paths (hash table)
- **Garnet:** Epoch protection + 2PL

### 2. Memory Efficiency Innovations
- **Pelikan's 5-byte metadata** is revolutionary for small objects (91% reduction)
- **Segment-based TTL grouping** enables bulk operations and metadata sharing
- **Garnet's dual stores** optimize for different use cases (strings vs. objects)
- **jemalloc/tcmalloc** consistently better than libc (1.01-1.02 vs. 1.31 fragmentation)

### 3. Threading Model Evolution
1. **First Generation:** Single-threaded (Redis) - simplicity, no locks
2. **Second Generation:** Worker threads (Memcached, KeyDB) - some parallelism, lock contention
3. **Third Generation:** Shared-nothing (Dragonfly), Segment-based (Pelikan), Epoch-protected (Garnet) - high parallelism, minimal contention

### 4. Design Philosophy Trade-offs

**Simplicity vs. Performance:**
- Redis chooses simplicity (single-threaded) - easier to reason about, limited scalability
- Dragonfly/Garnet choose complexity for performance - harder to implement, near-linear scaling

**Memory Efficiency vs. CPU Efficiency:**
- Memcached's slab allocator trades memory (internal fragmentation) for CPU (no external fragmentation, fast allocation)
- Pelikan's segments trade some CPU (segment management) for memory (5-byte overhead, 60% footprint reduction)

**Generality vs. Specialization:**
- Memcached/Redis are general-purpose (any object sizes, various use cases)
- Pelikan Segcache is specialized (small objects with TTL, incredible efficiency in this niche)
- Garnet splits the difference (dual stores for different use cases)

---

## Recommendations for grow-a-cache Project

Based on this research, here are architectural considerations:

### Threading Model
- **Consider shared-nothing architecture** (Dragonfly approach) for multi-core scalability
- **Message passing over locks** to avoid contention
- **Fiber/coroutine-based I/O** for efficient async operations within threads

### Memory Allocation
- **Segment-based design** (Pelikan) if target workload is small objects with TTL
- **Dual-store approach** (Garnet) if supporting both simple strings and complex objects
- **Minimize per-object metadata** - Pelikan's 5-byte overhead is the gold standard

### Small Object Optimization
- If workload is small object-heavy, Pelikan's segment approach is proven (60% memory reduction)
- Batch metadata at segment/chunk level rather than per-object
- Group by TTL to enable bulk expiration/eviction

### Lock Contention
- **Segment/batch-level locking** (Pelikan) for orders-of-magnitude reduction in lock frequency
- **VLL-style transactions** (Dragonfly) for multi-key operations
- **Epoch protection** (Garnet/FASTER) for safe concurrent access without traditional locks

### Performance Targets
- **Single-threaded baseline:** ~100-200K ops/sec (Redis)
- **Multi-threaded (worker pool):** ~2-4x with 4-8 threads (KeyDB, Memcached)
- **Shared-nothing:** 25-50x with full core utilization (Dragonfly)
- **Segment-optimized:** 8x at 24 threads (Pelikan)

---

## References and Sources

### Memcached
- [Memcached Architecture - Medium](https://medium.com/@akashsdas_dev/memcached-architecture-4c0aa8790dd0)
- [Threading Model - DeepWiki](https://deepwiki.com/memcached/memcached/3.2-threading-model)
- [Understanding Memcached Source Code - Slab I](https://medium.com/source-code/understanding-the-memcached-source-code-slab-i-9199de613762)
- [Memcache Internals](https://adayinthelifeof.nl/2011/02/06/memcache-internals/)
- [Memcached for Small Objects](https://dom.as/2008/12/25/memcached-for-small-objects/)
- [ReleaseNotes1429 - GitHub Wiki](https://github.com/memcached/memcached/wiki/ReleaseNotes1429)

### Redis
- [Redis: Single-Threaded and Still Fast - Medium](https://medium.com/@yashpaliwal42/redis-single-threaded-and-still-fast-89625094048b)
- [Redis Architecture: A Detailed Exploration](https://datasturdy.com/redis-architecture-a-detailed-exploration/)
- [Memory in Redis and How to Push It](https://firstprinciplesdesign.substack.com/p/memory-in-redis-and-how-to-push-it)
- [Discussion on Redis Allocators](https://topic.alibabacloud.com/a/discussion-on-redis-using-different-memory-allocator-tcmalloc-and-jemalloc_redis_1_47_20092246.html)
- [Diving Into Redis 6.0](https://redis.io/blog/diving-into-redis-6/)
- [Redis 6 Multithreading - InfoWorld](https://www.infoworld.com/article/2257643/redis-6-arrives-with-multithreading-for-faster-io.html)

### Dragonfly
- [Dragonfly Share-Nothing Architecture](https://github.com/dragonflydb/dragonfly/blob/main/docs/df-share-nothing.md)
- [GitHub - dragonflydb/dragonfly](https://github.com/dragonflydb/dragonfly)
- [Threading Models Matter](https://www.dragonflydb.io/blog/why-threading-models-matter-dragonfly-vs-valkey)
- [Ensuring Atomicity: Dragonfly Transactions](https://www.dragonflydb.io/blog/transactions-in-dragonfly)
- [DragonflyDB vs Redis - Medium](https://medium.com/@mohitdehuliya/dragonflydb-vs-redis-a-deep-dive-towards-the-next-gen-caching-infrastructure-23186397b3d3)

### Pelikan (Segcache)
- [Segcache: Memory-Efficient Cache](https://pelikan.io/2021/segcache.html)
- [NSDI'21 Paper - USENIX](https://www.usenix.org/system/files/nsdi21-yang.pdf)
- [Segcache NSDI'21 Presentation](https://www.usenix.org/conference/nsdi21/presentation/yang-juncheng)
- [GitHub - twitter/pelikan](https://github.com/twitter/pelikan)
- [Segcache: Segment-Structured Cache](https://blog.jasony.me/system/cache/2021/04/01/segcache)

### KeyDB
- [A Multithreaded Fork of Redis 5X Faster](https://docs.keydb.dev/blog/2019/10/07/blog-post/)
- [GitHub - Snapchat/KeyDB](https://github.com/Snapchat/KeyDB)
- [Comparing Redis6 vs KeyDB - KeyDB Blog](https://docs.keydb.dev/blog/2020/04/15/blog-post/)
- [Redis Analysis - Threading Model](https://www.dragonflydb.io/blog/redis-analysis-part-1-threading-model)

### Garnet
- [Introducing Garnet - Microsoft Research](https://www.microsoft.com/en-us/research/blog/introducing-garnet-an-open-source-next-generation-faster-cache-store-for-accelerating-applications-and-services/)
- [GitHub - microsoft/garnet](https://github.com/microsoft/garnet)
- [Welcome to Garnet](https://microsoft.github.io/garnet/docs)
- [Garnet Locking Documentation](https://microsoft.github.io/garnet/docs/dev/tsavorite/locking)
- [Managing Memory Usage - Garnet Docs](https://microsoft.github.io/garnet/docs/getting-started/memory)

### Academic Papers
- [VLL: A Lock Manager Redesign for Main Memory Database Systems](https://www.cs.umd.edu/~abadi/papers/vldbj-vll.pdf)
- [Lightweight Locking for Main Memory Database Systems](https://www.cs.umd.edu/~abadi/papers/vll-vldb13.pdf)

---

*Research compiled: 2025-12-11*
