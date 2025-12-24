# v1 Planning: RESP Protocol Support

## Goals

Add Redis RESP2/3 protocol support as a second protocol alongside memcached, enabling Redis client libraries to talk to this cache.

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Command set | GET, SET, DEL, PING, HELLO, COMMAND | Minimal viable set for basic cache operations |
| Protocol selection | Config-based, single port | Simple and predictable, no auto-detection complexity |
| RESP version | RESP2 default, RESP3 via HELLO | Maximum compatibility with upgrade path |
| Parser | Hand-rolled | Matches existing style, zero dependencies, RESP is simple |
| Code organization | Vertical slices per protocol | Each protocol self-contained, no shared abstractions |
| SET options | EX, PX, NX, XX | Core Redis SET functionality, maps cleanly to storage API |

## Architecture

```
src/
  protocols/
    memcached/
      mod.rs         # re-exports
      parser.rs      # parsing (moved from protocol.rs)
      handler.rs     # command execution (extracted from server.rs)
    resp/
      mod.rs         # re-exports
      parser.rs      # RESP2/3 frame parsing
      handler.rs     # command dispatch
  server.rs          # protocol-agnostic, dispatches to handlers
  config.rs          # ProtocolType enum for selection
```

Storage API is the only contract between protocols. No shared Protocol trait, no unified command enum.

## Scope

**Included:**
- RESP frame types: Simple string, Error, Integer, Bulk string, Array
- Commands: PING, GET, SET (with EX/PX/NX/XX), DEL, HELLO, COMMAND
- Protocol selection via `--protocol resp` CLI flag or TOML config
- Comprehensive parser unit tests

**Deferred:**
- RESP3-specific frame types (maps, sets, booleans)
- Additional commands (INCR, EXPIRE, TTL, etc.)
- Integration tests with actual Redis clients
- Cross-protocol storage verification tests
