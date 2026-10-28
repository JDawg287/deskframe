#!/usr/bin/env bash
# PostToolUse hook: after an Edit/Write to a .rs file, format and lint.
# Exit 2 sends the clippy output back to Claude so it fixes the warnings.
input=$(cat)
printf '%s' "$input" | grep -qE '"file_path"[[:space:]]*:[[:space:]]*"[^"]*\.rs"' || exit 0
cd "${CLAUDE_PROJECT_DIR:-$(dirname "$0")/../..}" || exit 0
cargo fmt 2>&1 >/dev/null
if ! out=$(cargo clippy --quiet --all-targets -- -D warnings 2>&1); then
  printf 'cargo clippy failed:\n%s\n' "$out" | tail -60 >&2
  exit 2
fi
exit 0
