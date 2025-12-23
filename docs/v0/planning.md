# v0 Planning: Memcached-Compatible Cache Server

## Goals

Build a memcached-compatible cache server in Rust implementing the text protocol.

## Requirements

From original prompt:
> A memcached-compatible cache server written in Rust, implementing the text protocol.

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Language | Rust | Memory safety, performance, modern tooling |
| Protocol | Memcached text protocol | Compatibility with existing clients, simpler than binary |
| Async runtime | Tokio | De-facto standard for async Rust |
| Storage | In-memory HashMap with RwLock | Simple starting point |
| Eviction | LRU with TTL | Standard cache semantics |
| Memory limit | Configurable with LRU eviction | Prevent unbounded growth |

## Architecture

Single-process server with:
- TCP listener accepting connections
- Per-connection async task handling
- Shared storage via `Arc<Storage>` with `RwLock<HashMap>`
- Background task for expired item cleanup

## Scope

**Included:**
- Core memcached text protocol commands: get, gets, set, add, replace, append, prepend, cas, delete, incr, decr, flush_all, stats, version, quit
- TTL-based expiration
- LRU eviction when memory limit reached
- CAS (compare-and-swap) support
- Configuration via CLI and TOML file

**Explicitly deferred:**
- Binary protocol
- Multi-threading optimizations (sharding, lock-free structures)
- Tiered storage (SSD/PMEM)
- Clustering/replication
- Alternative protocols (RESP, etc.)
