#!/bin/bash
# Pre-commit hook: Reminds Claude to run pre-commit checks before git commit
#
# Receives JSON via stdin: {"tool_name": "Bash", "tool_input": {"command": "..."}}
# Exit 0 = allow, Exit 2 = block (stderr shown to Claude)

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.tool_input.command // ""')

# Only trigger on git commit commands
if echo "$COMMAND" | grep -q "^git commit"; then
  # Output to stderr - this is what Claude sees
  echo "REMINDER: Before committing, verify pre-commit checks pass:" >&2
  echo "  1. cargo fmt --check" >&2
  echo "  2. cargo clippy --all-targets" >&2
  echo "  3. cargo test" >&2
  echo "" >&2
  echo "Use the pre-commit-checks skill for the full workflow." >&2
fi

# Always allow - this is just a reminder
exit 0
