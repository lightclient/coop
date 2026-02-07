# Development commands for coop

# Run all checks (what CI will run)
check: fmt-check toml-check lint deny test

# Format all code
fmt:
    cargo fmt --all
    taplo fmt

# Check formatting without modifying
fmt-check:
    cargo fmt --all -- --check

# Check TOML formatting
toml-check:
    taplo check
    taplo fmt --check

# Run clippy with workspace lints
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Check dependencies (licenses, advisories, bans)
deny:
    cargo deny check

# Run all tests
test:
    cargo test --all

# Find unused dependencies
machete:
    cargo machete

# Build in release mode
build:
    cargo build --release

# Run the TUI
run:
    cargo run --bin coop

# Fix all auto-fixable issues
fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged
    taplo fmt

# Install git hooks from .githooks/
hooks:
    git config core.hooksPath .githooks
    @echo "✅ git hooks installed from .githooks/"

# Run TUI with JSONL tracing to traces.jsonl
trace:
    COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- chat

# Run gateway daemon with JSONL tracing
trace-gateway:
    COOP_TRACE_FILE=traces.jsonl cargo run --bin coop -- start

# Tail recent trace events
trace-tail n="50":
    tail -n {{n}} traces.jsonl

# Show errors from traces
trace-errors:
    grep '"level":"ERROR"' traces.jsonl | tail -20

# Show warnings from traces
trace-warnings:
    grep '"level":"WARN"' traces.jsonl | tail -20

# Show tool execution spans
trace-tools:
    grep 'tool_execute' traces.jsonl | tail -20

# Show API request spans
trace-api:
    grep 'anthropic_request' traces.jsonl | tail -20

# Show agent turn spans
trace-turns:
    grep 'agent_turn' traces.jsonl | tail -20

# Clear trace file
trace-clear:
    rm -f traces.jsonl
