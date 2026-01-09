# v3 Planning: Large Value Support

## Goals

Support values larger than the buffer size (64KB default) while maintaining:
- Bounded memory usage (no OOM possible)
- High connection capacity (10,000+ connections)
- No per-request heap allocation on hot path

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Buffer strategy | Chained buffers from pool | Memory bounded, no fragmentation, O(1) alloc |
| Memory limit | Pool size is the limit | Natural backpressure, no tracking overhead |
| Large value assembly | Assemble to Vec for storage | Storage API unchanged, single copy acceptable |
| I/O model | Scatter-gather (writev/readv) | Zero-copy writes from chain |
| Parsing strategy | Header in first chunk | Headers small (<1KB), only values span chunks |
| Max value config | `max_value_size` setting | Early rejection, clear contract with clients |

## Architecture

### Buffer Chain

```
┌─────────────────────────────────────────────────────────┐
│                    BufferChain                          │
├─────────────────────────────────────────────────────────┤
│ buffers: Vec<usize>   [0, 5, 12, 8]  // pool indices   │
│ len: usize            = 250000       // total bytes     │
│ buffer_size: usize    = 65536        // per chunk       │
└─────────────────────────────────────────────────────────┘
        │
        ▼
┌────────┬────────┬────────┬────────┐
│ buf[0] │ buf[5] │ buf[12]│ buf[8] │  ← from BufferPool
│ 64KB   │ 64KB   │ 64KB   │ 56KB   │  ← last partially filled
└────────┴────────┴────────┴────────┘
```

### Memory Model

```
Buffer Pool (Per-Worker): 8192 × 64KB = 512MB
├── Connection I/O buffers: ~2000 (2 per connection)
└── Chain buffers: ~6192 available
    └── Max concurrent 10MB values: ~39 per worker
```

### Data Flow

**SET with large value:**
```
1. Read header into first buffer (find length)
2. Allocate chain buffers based on length
3. Read remaining data into chain
4. Assemble chain → Vec<u8>
5. Store in storage
6. Release chain buffers
```

**GET with large value:**
```
1. Look up key in storage
2. Write header to first buffer
3. Copy value into chain (or stream from storage)
4. writev() all chunks to socket
5. Release chain buffers
```

## Scope

### Completed in v3
- `BufferChain` abstraction with append, assemble, iterate, release
- `max_value_size` configuration option (CLI: `--max-value-size`, default 8MB)
- Early rejection of oversized requests
- Partial read accumulation in both event loops
- `NeedChain` and `LargeResponse` variants in `ProcessResult`
- io_uring read accumulation buffer (fixed partial read bug)
- Functional tests for max_value_size rejection

### Remaining (v3)
- Full chained buffer read path for values > buffer_size
- Functional tests for 1MB+ payloads
- Multi-buffer write support for large responses in io_uring

### Deferred to v4
- Zero-copy streaming from storage (requires storage API changes)
- Memory-mapped large values
- Configurable buffer pool partitioning (chains vs connections)
- Per-connection value budgets
- Idle timeout for slow accumulation
