# grow-a-cache

## Preamble
This whole project is part of my experiment to build a production-grade cache backend without writing a single line of source code myself. I do not yet know how far this could get me, but that's why it'll be fun to find out.

My high level plan is to start with "vibe-coding", i.e. a very terse description without many qualifiers or full requirements, and iterate toward something suitable for production, which means scalability, robustness, debuggability, configurability, and many more properties we demand in a real service.

I choose cache as my target because I'm an expert in caching, so I can judge things pretty well. And I've built several projects over the years, so I also have some thoughts on how to guide a new project in the right direction. Let's see if such guidance works on LLMs as well, and what things that are easy/hard for humans end up being hard/easy for LLM.

Share your thoughts by filing an issue.

## V0 prompt

A memcached-compatible cache server written in Rust, implementing the text protocol.

## Features

- **Memcached Text Protocol Support**: Compatible with existing memcached clients
  - `get` / `gets` - Retrieve items (with CAS support)
  - `set` / `add` / `replace` - Store items
  - `delete` - Remove items
  - `cas` - Compare-and-swap atomic updates
  - `append` / `prepend` - Modify existing values
  - `incr` / `decr` - Atomic numeric operations
  - `flush_all` - Clear all items
  - `stats` / `version` - Server information

- **Key Expiration**: Items can be set with TTL (time-to-live)
- **Memory Limits**: Configurable maximum memory with LRU eviction
- **Configuration**: Via command-line arguments or TOML config file

## Building

```bash
cargo build --release
```

## Usage

### Basic Usage

```bash
# Run with default settings (127.0.0.1:11211, 64MB memory)
./target/release/grow-a-cache

# Specify listen address and memory limit
./target/release/grow-a-cache -l 0.0.0.0:11211 -m 134217728

# Use a configuration file
./target/release/grow-a-cache -c config.toml
```

### Command-Line Options

```
Options:
  -c, --config <CONFIG>        Path to TOML configuration file
  -l, --listen <LISTEN>        Address to bind to (e.g., 127.0.0.1:11211)
  -m, --max-memory <BYTES>     Maximum memory usage in bytes
  -t, --default-ttl <SECONDS>  Default TTL for items (0 = no expiration)
  -w, --workers <COUNT>        Number of worker threads
      --log-level <LEVEL>      Log level (trace, debug, info, warn, error)
  -h, --help                   Print help
  -V, --version                Print version
```

### Configuration File

Create a `config.toml` file:

```toml
[server]
listen = "127.0.0.1:11211"
# workers = 4  # Defaults to number of CPU cores

[storage]
max_memory = 67108864  # 64 MB
default_ttl = 0        # No default expiration
cleanup_interval = 60  # Cleanup expired items every 60 seconds

[logging]
level = "info"
```

## Testing with Telnet

```bash
# Connect to the server
telnet localhost 11211

# Set a value
set mykey 0 3600 5
hello
STORED

# Get the value
get mykey
VALUE mykey 0 5
hello
END

# Delete the key
delete mykey
DELETED

# Compare-and-swap
gets mykey
VALUE mykey 0 5 1
hello
END

cas mykey 0 3600 5 1
world
STORED
```

## Testing with a Memcached Client

Any memcached client library should work. Example with Python:

```python
import socket

def memcached_set(key, value, flags=0, ttl=0):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 11211))

    cmd = f"set {key} {flags} {ttl} {len(value)}\r\n{value}\r\n"
    sock.send(cmd.encode())
    response = sock.recv(1024).decode()
    sock.close()
    return response

def memcached_get(key):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 11211))

    cmd = f"get {key}\r\n"
    sock.send(cmd.encode())
    response = sock.recv(4096).decode()
    sock.close()
    return response

# Usage
print(memcached_set("hello", "world"))
print(memcached_get("hello"))
```

## Architecture

```
src/
├── main.rs      # Entry point, logging setup
├── config.rs    # CLI and TOML configuration
├── protocol.rs  # Memcached text protocol parser
├── server.rs    # TCP server and connection handling
└── storage.rs   # In-memory storage with LRU eviction
```

## Memory Management

- Items are stored in a HashMap with LRU (Least Recently Used) tracking
- When memory limit is reached, least recently accessed items are evicted
- Expired items are cleaned up periodically (configurable interval)
- Items are also lazily evicted on access if expired

## License

Apache-2.0
