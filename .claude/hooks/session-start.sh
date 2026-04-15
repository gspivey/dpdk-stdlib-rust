#!/bin/bash
set -euo pipefail

# Claude Code session-start hook for dpdk-stdlib-rust
# Ensures Rust toolchain and project dependencies are ready before the session begins.

# Only run in remote (Claude Code on the web) environments
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

cd "$CLAUDE_PROJECT_DIR"

echo "=== dpdk-stdlib-rust session startup ==="

# Ensure Rust toolchain is available
if ! command -v cargo &>/dev/null; then
  echo "Installing Rust toolchain..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
  echo "export PATH=\"\$HOME/.cargo/bin:\$PATH\"" >> "$CLAUDE_ENV_FILE"
fi

# GitHub API access note (agents running in Claude Code):
#   Use the GitHub MCP server tools (mcp__github__*) for ALL GitHub interactions —
#   PRs, issues, CI runs, comments, file reads. The MCP server is authenticated by
#   the Claude harness; agents do not need and do not have a usable GH_TOKEN.
#   The `gh` CLI is intentionally NOT installed here: in scheduled/remote sessions
#   the token plumbing differs from interactive web sessions and `gh` calls
#   typically 401 against api.github.com even when git operations (via the local
#   proxy at 127.0.0.1/git/...) work fine.
#
# Git operations (clone, fetch, push, pull) use the pre-configured local git
# proxy and work without any extra setup — just use `git` directly.

# Build workspace so subsequent cargo test/build are incremental
echo "Building workspace (incremental)..."
cargo build 2>&1

echo "=== Session startup complete ==="
