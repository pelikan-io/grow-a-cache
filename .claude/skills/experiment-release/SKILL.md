---
name: experiment-release
description: Use when releasing a milestone (v0, v1, v2...) after merging to main - creates pre-release with release notes
---

# Experiment Release

## Overview

This project is an experiment in AI-assisted development. All releases are pre-releases with structured release notes documenting what changed.

## When to Use

- After milestone branch is merged to main
- When user requests to tag/release a version

## Prerequisites

Before releasing, verify:
1. Feature branch is merged to main
2. You are on the main branch
3. All tests pass
4. Milestone documentation exists in `docs/v{N}/`

## Workflow

1. **Verify on main branch**
   ```bash
   git checkout main
   git pull origin main
   ```

2. **Determine version number**
   ```bash
   git tag --list 'v*' --sort=-v:refname | head -1
   ```
   Increment from latest (v0 → v1 → v2...)

3. **Create tag on main**
   ```bash
   git tag v{N}
   ```

4. **Create GitHub release with notes**
   ```bash
   gh release create v{N} --prerelease --title "grow-a-cache v{N}" --notes "$(cat <<'EOF'
   ## {Feature Title}

   {One paragraph summary of what this milestone adds.}

   ### Features
   - {Bullet list of features}

   ### Architecture
   - {Key architectural changes, if any}

   ### Documentation
   - Milestone docs in `docs/v{N}/` (planning, discussion, critique)

   **Full Changelog**: https://github.com/pelikan-io/grow-a-cache/compare/v{N-1}...v{N}
   EOF
   )"
   ```

5. **Push tag**
   ```bash
   git push origin refs/tags/v{N}
   ```

## Release Notes Template

```markdown
## {Feature Title}

{One paragraph summary.}

### Features
- Feature 1
- Feature 2

### Architecture
- Architectural change 1 (if applicable)

### Documentation
- Milestone docs in `docs/v{N}/` (planning, discussion, critique)

**Full Changelog**: https://github.com/pelikan-io/grow-a-cache/compare/v{N-1}...v{N}
```

## Rules

- **Always pre-release** — This is an experiment, not production software
- **Always from main** — Never tag feature branches directly
- **Always with notes** — Every release documents what changed
- **Use explicit refs** — Push `refs/tags/v{N}` to avoid branch/tag ambiguity
