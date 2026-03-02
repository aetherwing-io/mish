.PHONY: test test-lib test-integration clean-zombies build release

# Kill orphaned test runners from previous aborted runs.
# These hold PTY file descriptors and cargo locks, blocking new test runs.
clean-zombies:
	@pkill -f 'target/debug/deps/mish-' 2>/dev/null || true
	@pkill -f 'target/release/deps/mish-' 2>/dev/null || true

# Run all tests (cleans zombies first)
test: clean-zombies
	cargo test

# Run only lib + grammar tests (fast, no integration)
test-lib: clean-zombies
	cargo test --lib

# Run integration tests only
test-integration: clean-zombies
	cargo test --test grammar_tests --test fixture_pipeline_tests

# Build debug
build:
	cargo build

# Build release + update symlink
release:
	cargo build --release
	ln -sf $(CURDIR)/target/release/mish /opt/homebrew/bin/mish
