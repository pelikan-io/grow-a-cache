# RESP 2/3 Protocol Support Design

## Overview

Add Redis RESP2/3 protocol support as a self-contained vertical slice alongside the existing memcached implementation. Protocol selection via configuration. Minimal initial command set (GET, SET, DEL, PING), expandable without touching other protocols.

## Motivation

Client compatibility—enable existing Redis client libraries to talk to this cache without implementing memcached protocol adapters.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Command set | GET, SET, DEL, PING, HELLO | Start minimal, expand as needed |
| Protocol selection | Config-based, single port | Simple and predictable, no auto-detection complexity |
| RESP version | RESP2 default, RESP3 via HELLO | Maximum compatibility with upgrade path |
| Parser | Hand-rolled | Matches existing style, zero dependencies, RESP is simple |
| Code organization | Vertical slices per protocol | Explicit, isolated, no shared abstractions to maintain |

## Architecture

```
src/
  protocols/
    memcached/
      mod.rs         # re-exports
      parser.rs      # existing parsing logic (moved from protocol.rs)
      handler.rs     # command execution (extracted from server.rs)
    resp/
      mod.rs         # re-exports
      parser.rs      # RESP2/3 frame parsing, hand-rolled
      handler.rs     # command dispatch, direct storage calls
  storage.rs         # unchanged
  server.rs          # protocol-agnostic connection handling
  config.rs          # adds protocol selection field
```

Each protocol directory is fully self-contained. No shared `Protocol` trait, no unified command enum. Storage API is the only contract between protocols.

## Configuration

Add protocol selection to `Config`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolType {
    Memcached,
    Resp,
}

pub struct Config {
    pub listen: String,
    pub max_memory: usize,
    pub default_ttl: u64,
    pub cleanup_interval: u64,
    pub workers: Option<usize>,
    pub log_level: String,
    pub protocol: ProtocolType,  // new field, defaults to Memcached
}
```

- CLI flag: `--protocol memcached|resp`
- TOML: `protocol = "resp"`

Single port, single protocol per server instance. If you need both, run two instances.

## RESP Parser

Hand-rolled parser for RESP2/3 frames. RESP is length-prefixed, simpler than memcached's text protocol.

```rust
/// RESP data types
pub enum Frame {
    Simple(String),            // +OK\r\n
    Error(String),             // -ERR message\r\n
    Integer(i64),              // :1000\r\n
    Bulk(Option<Bytes>),       // $5\r\nhello\r\n or $-1\r\n (null)
    Array(Option<Vec<Frame>>), // *2\r\n... or *-1\r\n (null)
}

/// Parse result
pub enum ParseResult {
    Complete(Frame, usize),  // frame + bytes consumed
    Incomplete,
    Error(String),
}

pub fn parse(buffer: &[u8]) -> ParseResult {
    // First byte determines type: +, -, :, $, *
    // Length-prefixed, so no ambiguity
}
```

RESP3 additions (maps, sets, booleans) added later if clients send `HELLO 3`.

## RESP Commands

Initial command set:

```rust
pub enum RespCommand {
    Ping { message: Option<Bytes> },
    Get { key: Bytes },
    Set { key: Bytes, value: Bytes, ex: Option<u64>, nx: bool, xx: bool },
    Del { keys: Vec<Bytes> },
    Hello { version: u8 },  // RESP3 negotiation
}

pub struct RespHandler {
    storage: Arc<Storage>,
    resp_version: u8,  // 2 or 3, per-connection state
}

impl RespHandler {
    pub fn execute(&mut self, frame: Frame) -> Frame {
        let cmd = match self.parse_command(frame) {
            Ok(cmd) => cmd,
            Err(msg) => return Frame::Error(msg),
        };

        match cmd {
            RespCommand::Ping { message } => {
                Frame::Simple(message.unwrap_or("PONG".into()))
            }
            RespCommand::Get { key } => {
                match self.storage.get(&key) {
                    Some(item) => Frame::Bulk(Some(item.value.into())),
                    None => Frame::Bulk(None),  // null bulk
                }
            }
            RespCommand::Set { key, value, ex, nx, xx } => {
                // Map nx/xx to add/replace/set
                // Map ex to exptime
                // Return OK or null depending on NX/XX outcome
            }
            RespCommand::Del { keys } => {
                let count = keys.iter()
                    .filter(|k| self.storage.delete(k).is_deleted())
                    .count();
                Frame::Integer(count as i64)
            }
            RespCommand::Hello { version } => {
                self.resp_version = version.min(3).max(2);
                // Return server info map (RESP3) or array (RESP2)
            }
        }
    }
}
```

No flags passed to storage—RESP doesn't have them. TTL mapped from `EX`/`PX` options.

## Server Changes

The server becomes protocol-agnostic. Protocol handler selected at startup:

```rust
impl Server {
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(&self.config.listen).await?;

        loop {
            let (stream, addr) = listener.accept().await?;
            let storage = Arc::clone(&self.storage);
            let protocol = self.config.protocol.clone();

            tokio::spawn(async move {
                let result = match protocol {
                    ProtocolType::Memcached => {
                        memcached::handler::handle_connection(stream, storage).await
                    }
                    ProtocolType::Resp => {
                        resp::handler::handle_connection(stream, storage).await
                    }
                };

                if let Err(e) = result {
                    debug!(error = %e, "Connection error");
                }
            });
        }
    }
}
```

Each protocol's `handle_connection` owns its full read/parse/execute/respond loop.

## Migration of Existing Code

Existing `protocol.rs` and handler logic in `server.rs` move to:

- `protocols/memcached/parser.rs` — current `Parser`, `Command`, `Response` structs
- `protocols/memcached/handler.rs` — current `handle_connection`, `execute_command`, `execute_storage_command`

Mechanical move, no logic changes. Existing tests move with their code.

## Testing Strategy

Each protocol tested independently.

**RESP unit tests** (`protocols/resp/parser.rs`):
- Parse each frame type: simple string, error, integer, bulk, array
- Parse null bulk (`$-1\r\n`) and null array (`*-1\r\n`)
- Incomplete buffer returns `Incomplete`
- Malformed frames return `Error`

**RESP integration tests** (`tests/resp_integration.rs`):
- Connect, send `PING`, expect `+PONG\r\n`
- `SET foo bar` → `+OK\r\n`, `GET foo` → `$3\r\nbar\r\n`
- `GET nonexistent` → `$-1\r\n`
- `DEL foo bar baz` → `:<count>\r\n`
- `HELLO 3` upgrades connection to RESP3

**Cross-protocol storage test**:
- Run two server instances (memcached + RESP) against same storage
- Verify data written by one is readable by the other
- Confirms storage layer is truly protocol-agnostic

## Future Expansion

**Adding commands to RESP:**
1. Add variant to `RespCommand`
2. Add match arm in `execute()`
3. Add tests

**Adding a new protocol (e.g., memcached binary):**
1. Create `protocols/memcached_binary/` directory
2. Implement `parser.rs` and `handler.rs`
3. Add `ProtocolType::MemcachedBinary` to config
4. Add match arm in `server.rs`

No changes to existing protocols or storage.
