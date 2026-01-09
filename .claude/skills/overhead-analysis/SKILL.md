---
name: overhead-analysis
description: Estimate per-request overhead costs (syscalls, queue ops, allocations, memcpy) for the current design. Use when planning, reviewing performance, or evaluating design trade-offs.
---

# Overhead Analysis

## Overview

High-performance systems must minimize per-request overhead. This skill analyzes the current design to estimate costs amortized by throughput, identifying optimization opportunities.

## When to Use

- During planning to evaluate design trade-offs
- When reviewing or critiquing performance characteristics
- After implementation changes that affect the hot path
- When comparing alternative approaches
- User explicitly requests overhead analysis

## Analysis Categories

Analyze these four categories of overhead per request:

### 1. Syscalls
System calls are expensive (~100-1000ns each). Count:
- `read`/`write` or `recv`/`send`
- `epoll_wait`/`io_uring_enter`
- `accept`
- Any other syscalls on hot path

### 2. Queue Operations
Enqueue/dequeue operations, explicit or implicit:
- io_uring submission/completion queues
- Channel sends/receives
- Ring buffer operations
- Any FIFO/LIFO structures

### 3. Memory Allocations
Heap allocations are costly (~50-200ns each):
- `Vec::new()`, `Vec::push()` that grows
- `String` allocations
- `Box::new()`
- Any `clone()` that allocates

### 4. Memory Copies
`memcpy`/`memmove` operations:
- Buffer copies between layers
- Data cloning from storage
- Protocol response building
- Any `copy_from_slice`, `extend_from_slice`, `clone()`

## Output Format

Produce a markdown table summarizing overhead per request-response cycle:

```markdown
## Overhead Analysis: [Component/Flow Name]

Analysis date: YYYY-MM-DD

### Summary Table

| Category | Count/Request | Notes |
|----------|---------------|-------|
| Syscalls | N | details |
| Queue Ops | N | details |
| Allocations | N | details |
| Memcpy | N × size | details |

### Breakdown by Operation

#### [Operation 1: e.g., GET small value]
| Category | Count | Details |
|----------|-------|---------|
| ... | ... | ... |

#### [Operation 2: e.g., SET with noreply]
| Category | Count | Details |
|----------|-------|---------|
| ... | ... | ... |

### Optimization Opportunities
- [ ] Opportunity 1
- [ ] Opportunity 2

### Comparison with Ideal
| Category | Current | Ideal | Gap |
|----------|---------|-------|-----|
| ... | ... | ... | ... |
```

## Analysis Process

1. **Identify the hot path**: Trace request from socket read to response write
2. **Count each category**: Walk through code, noting each operation
3. **Distinguish by operation type**: GET vs SET vs DELETE may have different profiles
4. **Note amortization**: Some costs are amortized (e.g., batch syscalls)
5. **Compare runtimes**: io_uring vs mio may differ
6. **Document assumptions**: Buffer sizes, value sizes, batch sizes

## Document Location

```
docs/overhead-analysis.md
```

Update this document when design changes affect overhead characteristics.

## Example Analysis

For a cache server handling GET requests:

| Category | io_uring | mio | Notes |
|----------|----------|-----|-------|
| Syscalls | 1 (amortized) | 2 | io_uring batches; mio needs epoll_wait + read |
| Queue Ops | 2 | 0 | SQ push + CQ pop |
| Allocations | 2 | 2 | Response Vec + value clone |
| Memcpy | 3 × value_size | 3 × value_size | storage→Vec→buffer→kernel |

This reveals: allocation overhead is the same, but syscall overhead differs significantly.
