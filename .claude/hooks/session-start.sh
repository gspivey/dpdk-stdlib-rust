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

# Build workspace so subsequent cargo test/build are incremental
echo "Building workspace (incremental)..."
cargo build 2>&1

echo "=== Session startup complete ==="
