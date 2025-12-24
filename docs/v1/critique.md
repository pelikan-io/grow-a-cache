# v1 Critique

## Requirements Assessed

Add RESP 2/3 protocol support alongside memcached, with minimal command set (GET, SET, DEL, PING) for client compatibility.

## Design Review

### Limitations

**No protocol auto-detection.** Clients must connect to the correct port/instance. If a memcached client connects to a RESP server, they'll get parse errors rather than a helpful message. This is by design (simplicity), but worth noting.

**Single protocol per server instance.** Running both protocols requires two processes. Shared storage across instances isn't supported—each process has its own in-memory data. This limits operational flexibility.

**RESP3 support is minimal.** The HELLO command negotiates version, but no RESP3-specific frame types (maps, attributes, pushes) are implemented. A RESP3 client won't get any benefit beyond what RESP2 provides.

### Assumptions

- **Clients send well-formed requests.** Malformed RESP causes connection-level errors rather than graceful per-command errors in some paths.
- **Keys are valid UTF-8.** The RESP handler converts Bytes to &str for storage keys; non-UTF-8 keys are rejected.
- **EX/PX values are reasonable.** No validation against overflow when converting PX (milliseconds) to seconds.

## Implementation Review

### Gaps

**No integration tests with real clients.** Only unit tests for parsing. Redis client libraries may have expectations about response format, command behavior, or error messages that aren't tested.

**COMMAND returns empty array.** Real Redis returns command metadata. Some clients use this for capability detection. Empty response may confuse them.

**No QUIT command.** Redis has QUIT; current implementation doesn't. Connection just stays open until client closes it.

**SET flags handling incomplete.** EXAT, PXAT, KEEPTTL, GET, IFEQ, IFGT not implemented. SET NX + XX is explicitly rejected but the error message differs from Redis's behavior.

**Error message formatting.** Redis uses specific error prefixes (ERR, WRONGTYPE, NOPROTO). Current implementation uses generic "ERR" for everything.

## Hypothetical Scenarios

### "What if clients use pipelining?"

RESP supports sending multiple commands without waiting for responses. Current implementation processes commands one at a time from the buffer. Pipelining should work, but hasn't been tested. Bulk operations may have performance characteristics different from Redis.

### "What if keys contain binary data?"

Storage API uses `&str` keys. RESP allows arbitrary binary keys. A client sending binary keys (common in some use cases) will get errors. This is a protocol mismatch with real Redis.

### "What if we need pub/sub?"

Pub/sub requires connection state (subscribed channels), push messages, and different command semantics. Current architecture (stateless command execution) would need significant changes. The vertical slice approach helps—pub/sub could be its own module—but storage layer has no pub/sub primitives.

### "What if we add clustering?"

RESP clustering uses specific commands (CLUSTER, MOVED/ASK redirects). Current design has no concept of slots, nodes, or redirects. Would require new storage abstraction and protocol extensions.

### "What if clients expect Lua scripting?"

Redis EVAL/EVALSHA execute Lua. This would require embedding a Lua runtime. Current architecture doesn't preclude it, but it's a significant undertaking.

## Recommendations

1. **Add integration test with redis-cli or redis-py.** Validates actual client compatibility rather than just protocol correctness.

2. **Implement QUIT command.** Trivial and expected by clients.

3. **Consider binary key support.** Either accept arbitrary bytes in storage or document UTF-8 limitation clearly.

4. **Add RESP3 frame types.** If advertising RESP3 support via HELLO, should deliver on it. Maps and sets are straightforward additions.

5. **Test pipelining explicitly.** Important for performance-sensitive clients.
