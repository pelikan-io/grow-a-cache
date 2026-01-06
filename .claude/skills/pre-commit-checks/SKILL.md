---
name: pre-commit-checks
description: Automatically triggered before git commit to fix all outstanding style and functional warnings (project)
---

# Pre-Commit Checks

## Overview

Before committing code, ensure there are no compiler warnings, clippy lints, or formatting issues. This skill runs automatically before any git commit operation.

## When to Use

- **Automatically before any git commit** — Claude must run this before executing `git commit`
- When user asks to "fix warnings" or "clean up code"
- After significant code changes before finalizing

## Checklist

Before committing, run these checks in order:

### 1. Format Check
```bash
cargo fmt --check
```
If formatting issues exist, fix them:
```bash
cargo fmt
```

### 2. Clippy Lints
```bash
cargo clippy --all-targets --all-features 2>&1
```
Review and fix any warnings. Common fixes:
- `unused variable` → prefix with `_` or remove
- `unused import` → remove the import
- `unused mut` → remove `mut` keyword
- `dead_code` → add `#[allow(dead_code)]` if intentionally unused for now, or remove

### 3. Compiler Warnings
```bash
cargo check --all-targets 2>&1
```
Fix any remaining warnings not caught by clippy.

### 4. Tests Pass
```bash
cargo test 2>&1
```
Ensure all tests still pass after fixes.

## Common Warning Patterns

| Warning | Fix |
|---------|-----|
| `unused variable: x` | Rename to `_x` or remove |
| `unused import` | Remove the import line |
| `unused mut` | Remove `mut` keyword |
| `dead_code` on function | Add `#[allow(dead_code)]` or remove function |
| `dead_code` on field | Add `#[allow(dead_code)]` above field |
| `variable does not need to be mutable` | Remove `mut` |
| `this `if` has identical blocks` | Consolidate the conditions |

## Workflow

1. Run `cargo fmt` to auto-fix formatting
2. Run `cargo clippy` and review output
3. Fix each warning methodically
4. Run `cargo test` to verify nothing broke
5. Proceed with git commit only when clean

## Rules

- **Never commit with warnings** — All code must compile cleanly
- **Prefer fixes over suppressions** — Only use `#[allow(...)]` when the code is intentionally structured that way (e.g., dead code that will be wired in later)
- **Document suppressions** — Add a comment explaining why when using `#[allow(...)]`
- **Run tests after fixes** — Warnings fixes can accidentally break functionality
