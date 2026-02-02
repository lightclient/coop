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
