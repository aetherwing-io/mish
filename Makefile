.PHONY: test test-claude test-all clean-zombies build release

# Kill orphaned test runners from previous aborted runs.
# These hold PTY file descriptors and cargo locks, blocking new test runs.
clean-zombies:
	@pkill -f 'target/debug/deps/mish-' 2>/dev/null || true
	@pkill -f 'target/release/deps/mish-' 2>/dev/null || true

# Inner loop: lib + grammar + MCP + CLI integration (~20s)
test: clean-zombies
	MISH_NO_DAEMON=1 cargo test --lib --test grammar_tests --test fixture_pipeline_tests \
	  --test mcp_integration --test cli_integration

# Pre-release: Claude Code compat + stdout contamination (~35s)
test-claude: clean-zombies
	cargo test --test claude_code_compat --test stdout_contamination

# Everything
test-all: clean-zombies
	cargo test

# Build debug
build:
	cargo build

# Build release + update symlink
release:
	cargo build --release
	ln -sf $(CURDIR)/target/release/mish /opt/homebrew/bin/mish
	git tag v$(shell cargo metadata --no-deps --format-version=1 | jq -r '.packages[0].version')
	git push origin v$(shell cargo metadata --no-deps --format-version=1 | jq -r '.packages[0].version')
