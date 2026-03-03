#!/usr/bin/env bash
# mcp-dev.sh — Build mish and ensure it's registered as a Claude Code MCP server.
#
# Usage:
#   ./scripts/mcp-dev.sh          # debug build (fast)
#   ./scripts/mcp-dev.sh release  # release build (optimized)
#   ./scripts/mcp-dev.sh test     # build + run smoke test
set -euo pipefail

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

MODE="${1:-debug}"
BINARY="$(pwd)/target/debug/mish"

if [[ "$MODE" == "release" ]]; then
    echo "→ cargo build --release"
    cargo build --release 2>&1
    BINARY="$(pwd)/target/release/mish"
elif [[ "$MODE" == "test" ]]; then
    echo "→ cargo build --release"
    cargo build --release 2>&1
    BINARY="$(pwd)/target/release/mish"
    echo "→ smoke test"
    python3 tests/smoke_mcp.py "$BINARY"
    exit $?
else
    echo "→ cargo build"
    cargo build 2>&1
    BINARY="$(pwd)/target/debug/mish"
fi

echo "→ binary: $BINARY"
"$BINARY" --version

# Register/update in Claude Code (user scope so it works in all projects)
echo "→ registering mish MCP server"
claude mcp remove mish 2>/dev/null || true
claude mcp add --transport stdio --scope user mish -- "$BINARY" serve

echo "→ done. Restart Claude Code to pick up the new binary."
echo "  Run '/mcp' inside Claude Code to verify."
