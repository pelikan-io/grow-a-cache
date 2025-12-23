# v0 Discussion

## Highlights

The v0 discussion was primarily research-oriented—exploring the design space before committing to implementation choices for future versions.

### Storage Architecture Trade-offs

Explored three generations of KV cache threading models:
1. **Single-threaded** (Redis): Simple but doesn't scale
2. **Worker pools with locks** (Memcached, KeyDB): Limited to ~4 effective threads
3. **Shared-nothing / epoch-based** (Dragonfly, Pelikan, Garnet): Scales to 64-128 threads

Key insight: The single `RwLock<HashMap>` in v0 is a known bottleneck. Scaling requires sharding (4-8× thread count) or a fundamentally different architecture.

### Segcache vs Garnet's HybridLog

Two compelling approaches for mixed object sizes:

**Segcache (Pelikan):**
- 5 bytes metadata per object (vs 56 bytes in memcached)
- TTL-based segment grouping
- PMEM datapool for SSD backing
- Configurable segment sizes for large objects

**HybridLog (Garnet/FASTER):**
- Automatic hot/cold tiering via log position
- In-place updates for hot data, RCU for cold
- Three regions: Mutable → Read-Only → Stable (disk)

Decision deferred to future version.

### S3-FIFO for Flash Storage

S3-FIFO emerged as a compelling alternative for SSD-backed caching:
- Three FIFO queues with "quick demotion" for one-hit wonders
- Write amplification ≈ 1 (pure sequential writes)
- 6× better scalability than LRU
- Solidigm reports 10 GB/s bandwidth

### Benchmarking and Coordinated Omission

Surveyed benchmarking tools and identified coordinated omission risks:

| Tool | CO Risk | Notes |
|------|---------|-------|
| redis-benchmark | HIGH | Closed-loop, no correction |
| memtier_benchmark | MEDIUM | Has rate limiting option |
| rpc-perf (IOP) | MEDIUM | Open-loop but no intended-time tracking |
| wrk2 | LOW | Reference implementation with CO correction |
| YCSB | LOW | Has `measurement.interval=intended` option |

Key finding: YCSB showed 1000× difference between uncorrected and corrected p99 latency.

### Pelikan Architecture

Examined Pelikan's architecture as reference for future scaling:
- Data/control plane separation
- Lockless ring buffers between threads
- Serialized storage access (single storage thread)
- Worker threads never block

## Questions Raised

1. Should we adopt Segcache-style segment storage or HybridLog-style tiering?
2. Is S3-FIFO worth implementing for flash-backed deployments?
3. How do we want to handle benchmarking—build CO-aware tooling or use existing (wrk2/YCSB)?

---

## Full Transcript

<details>
<summary>Complete conversation log</summary>

See [transcript.md](transcript.md) for the complete v0 research discussion.

</details>
