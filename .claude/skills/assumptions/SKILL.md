---
name: assumptions
description: Use when planning, reviewing, or critiquing project design and implementation and explicitly state requirements assumptions. Main aspects include beliefs on workloads, environments, constraints, and optimization goals.
---

# Assumptions Documentation

## Overview

Design decisions are only as good as the assumptions they're built on. This skill formalizes the process of identifying, discussing, and documenting assumptions.

## When to Use

- Ask questions to clarify assumptions as part of the planning process
- Introspect any implicit assumptions for existing design and make sure they are documented
- When critiquing or evaluting an existing design an dimplementation

## Document Location

```
docs/ASSUMPTIONS.md
```

Single living document, versioned with the codebase. Assumptions evolve—old ones get invalidated, new ones emerge.

## Document Structure

Use the following template. Each section should use a numbered list for itemization.

```markdown
# Assumptions

Last updated: {date}

## Workload

Covering arrival pattern, size distribution of both keys and values, skew, concurrency, and other key aspects of the request response pattern.

## Environment

Operating systems supported, network, security boundary and protocols, privacy consideration, deploy mechanism, and other runtime considerations that are outside of workload characteristics.

## Operations

Desirable throughput and latency target, robustness, scalability, availability, configurability, and observability considerations.

## Others

Anything that doesn't fit the categories above but with design impact.

## Open Questions

Assumptions that need clarification or validation before finalizing design decisions.
```
