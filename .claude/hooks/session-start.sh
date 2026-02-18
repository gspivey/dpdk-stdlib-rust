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

# Install gh CLI so agents can query GitHub Actions results without guessing.
# This is required for private repos (WebFetch can't access them unauthenticated).
# See CLAUDE.md "Querying CI Results" for the commands to use once installed.
if ! command -v gh &>/dev/null; then
  echo "Installing gh CLI..."
  # Detect latest version; fall back to a known-good version if network fails
  GH_VER=$(curl -sI https://github.com/cli/cli/releases/latest 2>/dev/null \
    | grep -i '^location:' \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' \
    | head -1 \
    || echo "")
  GH_VER="${GH_VER:-2.65.0}"
  GH_ARCHIVE="gh_${GH_VER}_linux_amd64"
  curl -sL "https://github.com/cli/cli/releases/download/v${GH_VER}/${GH_ARCHIVE}.tar.gz" \
    | tar xz -C /tmp 2>/dev/null \
    && mkdir -p ~/.local/bin \
    && cp "/tmp/${GH_ARCHIVE}/bin/gh" ~/.local/bin/gh \
    && echo "export PATH=\"\$HOME/.local/bin:\$PATH\"" >> "$CLAUDE_ENV_FILE" \
    && export PATH="$HOME/.local/bin:$PATH" \
    && echo "gh ${GH_VER} installed to ~/.local/bin/gh" \
    || echo "Warning: gh installation failed — CI querying will require manual setup"
fi

# Authenticate gh with GITHUB_TOKEN so it can access private repos.
# GITHUB_TOKEN is injected by Claude Code remote sessions when the user has
# connected their GitHub account.
if command -v gh &>/dev/null && [[ -n "${GITHUB_TOKEN:-}" ]]; then
  echo "$GITHUB_TOKEN" | gh auth login --with-token 2>/dev/null \
    && echo "gh authenticated with GITHUB_TOKEN" \
    || echo "Warning: gh auth failed (token may be expired or missing repo scope)"
fi

# Build workspace so subsequent cargo test/build are incremental
echo "Building workspace (incremental)..."
cargo build 2>&1

echo "=== Session startup complete ==="
