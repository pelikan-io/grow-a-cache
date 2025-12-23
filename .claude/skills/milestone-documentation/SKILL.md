---
name: milestone-documentation
description: Use when completing a milestone (v0, v1, ...) to document design decisions, conversation summary, and retrospective for experiment tracking
---

# Milestone Documentation

## Overview

This project tracks an experiment in AI-assisted development. Each milestone (v0, v1, ...) requires structured documentation to support a final writeup analyzing the process.

## When to Use

- After completing implementation for a milestone
- Before tagging a release (v0, v1, ...)
- When user requests milestone documentation

## Directory Structure

```
docs/
  v0/
    planning.md      # Design decisions made during brainstorming
    discussion.md    # Summary of the conversation/collaboration
    critique.md      # Retrospective: what worked, what didn't, future work
  v1/
    planning.md
    discussion.md
    critique.md
  ...
```

## Document Templates

### planning.md

Design decisions and rationale. Move or consolidate from `docs/plans/` if a planning doc already exists for this milestone.

```markdown
# v{N} Planning: {Feature Name}

## Goals
What we set out to build.

## Key Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| ... | ... | ... |

## Architecture
How it fits into the system.

## Scope
What's included, what's explicitly deferred.
```

### discussion.md

Full transcript plus curated highlights. Highlights focus on decisions that required exploration—avoid duplicating what's already in planning.md.

```markdown
# v{N} Discussion

## Highlights

Decisions that required back-and-forth to converge on. Skip straightforward choices already captured in planning.md.

### {Topic that required exploration}
What made this non-obvious. Options considered, trade-offs discussed, how we landed on the final choice.

### {Another exploratory topic}
...

## Questions Raised
Open questions that emerged but weren't resolved.

---

## Full Transcript

<details>
<summary>Complete conversation log</summary>

[Paste full conversation here]

</details>
```

### critique.md

External review simulation. Pretend a fresh team of experienced engineers is reviewing the design and implementation against stated requirements. Point out limitations, flaws, and gaps—both intrinsic issues and hypothetical scaling/evolution concerns.

```markdown
# v{N} Critique

## Requirements Assessed
The prompt/requirements this version was built against. (These evolve across versions.)

## Design Review

### Limitations
Intrinsic issues—things that are objectively problematic or suboptimal given current requirements.

### Assumptions
Implicit assumptions that could break under different conditions.

## Implementation Review

### Gaps
Missing functionality, incomplete error paths, untested scenarios.

## Hypothetical Scenarios

Test the design against plausible requirement changes or environmental shifts.

### "What if requirements change to...?"
e.g., need authentication, need encryption, need multi-tenancy

### "What if the operating environment changes?"
e.g., runs in constrained memory, deployed across regions, mixed client versions

### "What if usage patterns differ from assumptions?"
e.g., write-heavy instead of read-heavy, large values, hot keys

(Tailor hypotheticals to the feature area—not just perf/scale.)

## Recommendations
Prioritized list of what to fix or reconsider.
```

## Workflow

1. **Planning doc** — Created during brainstorming (may already exist in `docs/plans/`)
2. **Implementation** — Code written, tests passing
3. **Discussion doc** — Summarize the conversation that led to this milestone
4. **Critique doc** — Write retrospective after implementation complete
5. **Consolidate** — Move planning doc to `docs/v{N}/planning.md` if needed
6. **Tag** — `git tag v{N}` and push

## Notes

- Discussion summary should be useful to someone who wasn't in the conversation
- Critique should be honest—failures are valuable data for the experiment
- Keep docs concise; aim for signal over completeness
