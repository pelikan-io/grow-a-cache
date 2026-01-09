# Overhead Analysis

Analysis date: 2026-01-08

## Assumptions

- Buffer size: 64KB
- Small value: < 1KB (fits in single response)
- Large value: > buffer size
- Single-key operations (GET key, SET key value)

---

## Summary Table (Per Request-Response)

| Category | io_uring | mio | Notes |
|----------|----------|-----|-------|
| **Syscalls** | 1 (amortized) | 2-3 | io_uring batches via `submit_and_wait`; mio needs `poll` + `read` + `write` |
| **Queue Ops** | 4 | 0 | SQ push (read) + CQ pop + SQ push (write) + CQ pop |
| **Allocations** | 3-4 | 3-4 | input_copy Vec + response Vec + value clone + (keys Vec for GET) |
| **Memcpy** | 4 × data | 4 × data | provided→accum + accum→input_copy + storage→response + response→write_buf |

---

## Breakdown by Operation

### GET (Small Value, Cache Hit)

#### io_uring

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 1 amortized | `io_uring_enter` batches multiple completions |
| Queue Ops | 4 | SQ push (read), CQ pop, SQ push (write), CQ pop |
| Allocations | 4 | `input_copy.to_vec()`, `keys_ref Vec`, `response Vec::new()`, `item.clone()` |
| Memcpy | 4 | provided_buf→accum_buf, accum→input_copy, value→response, response→write_buf |

#### mio

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 3 | `epoll_wait`, `read`, `write` |
| Queue Ops | 0 | No explicit queues |
| Allocations | 4 | `input_copy.to_vec()`, `keys_ref Vec`, `response Vec::new()`, `item.clone()` |
| Memcpy | 3 | (direct read into buffer), input→input_copy, value→response, response→write_buf |

### GET (Cache Miss)

#### io_uring

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 1 amortized | Same |
| Queue Ops | 4 | Same |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` (just "END\r\n") |
| Memcpy | 3 | provided→accum, accum→input_copy, response→write_buf |

#### mio

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 3 | Same |
| Queue Ops | 0 | Same |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` |
| Memcpy | 2 | input→input_copy, response→write_buf |

### SET (with noreply)

#### io_uring

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 0.5 amortized | Only read, no write needed |
| Queue Ops | 2 | SQ push (read), CQ pop |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` (empty) |
| Memcpy | 2 | provided→accum, accum→input_copy |

#### mio

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 2 | `epoll_wait`, `read` |
| Queue Ops | 0 | None |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` |
| Memcpy | 1 | input→input_copy |

### SET (with reply)

#### io_uring

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 1 amortized | read + write batched |
| Queue Ops | 4 | Same as GET |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` ("STORED\r\n") |
| Memcpy | 3 | provided→accum, accum→input_copy, response→write_buf |

#### mio

| Category | Count | Details |
|----------|-------|---------|
| Syscalls | 3 | `epoll_wait`, `read`, `write` |
| Queue Ops | 0 | None |
| Allocations | 2 | `input_copy.to_vec()`, `response Vec::new()` |
| Memcpy | 2 | input→input_copy, response→write_buf |

---

## Optimization Opportunities

### High Priority (Eliminates allocations on hot path)

- [ ] **Eliminate `input_copy.to_vec()`**: Restructure borrow checker constraints by using separate read/write buffer pools, or process in-place before allocating write buffer
- [ ] **Write responses directly to output buffer**: Change `execute_command() -> Vec<u8>` to `execute_command_into(&mut [u8]) -> usize` for fixed-size responses (STORED, DELETED, END, etc.)
- [ ] **Avoid `keys_ref` Vec allocation**: Use fixed-size array for common case (single key), only allocate for MGET

### Medium Priority (Reduces copies)

- [ ] **io_uring: Eliminate accum→input_copy**: Use two separate pools (read vs write) to avoid borrow conflict
- [ ] **mio: Process directly from read buffer**: Already possible but blocked by borrow checker workaround
- [ ] **Storage: Support `get_into()` API**: Copy value directly into response buffer instead of cloning to intermediate Vec

### Lower Priority (Syscall optimization)

- [ ] **mio: Use `writev` for scatter-gather**: Combine header + value + trailer in single syscall
- [ ] **io_uring: Link read→process→write**: Use SQE linking for dependent operations (complex)

---

## Comparison with Ideal

| Category | Current (io_uring) | Current (mio) | Ideal | Gap |
|----------|--------------------|--------------:|-------|-----|
| Syscalls | 1 amortized | 3 | 1 amortized | mio: 2 extra |
| Queue Ops | 4 | 0 | 2 | io_uring: 2 extra (could link) |
| Allocations | 3-4 | 3-4 | 0-1 | 2-4 extra (response Vec, input_copy, keys_ref) |
| Memcpy | 4 × value | 3 × value | 2 × value | 1-2 extra copies |

### Ideal Path (GET, cache hit)

1. Syscall: kernel→user (read) + user→kernel (write) = 1 batched syscall
2. Queue: submit read + submit write = 2 ops
3. Allocations: 0 (if value fits in pre-allocated buffer) or 1 (value clone from storage)
4. Memcpy: kernel→read_buf + storage→write_buf = 2 copies

### Current Bottleneck

The biggest gap is **allocations per request**. At 200K QPS:
- Current: 600K-800K allocations/sec per core
- Ideal: 0-200K allocations/sec per core

This is 3-4× more allocator pressure than necessary.

---

## Runtime Comparison Summary

| Aspect | io_uring | mio | Winner |
|--------|----------|-----|--------|
| Syscall count | 1 (batched) | 3 | io_uring |
| Syscall latency | Lower (no context switch per op) | Higher | io_uring |
| Queue overhead | 4 ops | 0 ops | mio |
| Memory model | Provided buffers (copy required) | Direct buffers | mio |
| Allocation count | Same | Same | Tie |
| Total copies | 4 | 3 | mio |
| Scalability | Better at high concurrency | Good | io_uring |

**Conclusion**: io_uring wins on syscall efficiency but loses on memory copies due to provided buffer model. For small values where copies are cheap, io_uring's syscall batching dominates. For large values, the extra copy hurts.
