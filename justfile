# Development commands for coop

# Defaults — override via env or `just features=signal run`
features   := env_var_or_default("COOP_FEATURES", "")
trace_file := env_var_or_default("COOP_TRACE_FILE", "traces.jsonl")
config     := env_var_or_default("COOP_CONFIG", "")

# Internal: build cargo flag strings from the above
_feat := if features != "" { "--features " + features } else { "" }
_conf := if config != "" { "--config " + config } else { "" }

# Run all checks (what CI will run)
check: fmt-check toml-check lint deny typos dead-code test

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

# Run all tests (nextest for speed, fallback to cargo test for doctests)
test:
    cargo nextest run --workspace
    cargo test --doc --workspace

# Find unused dependencies
machete:
    cargo machete

# Build in release mode
build:
    cargo build {{_feat}} --release

# Install the release binary to ~/.cargo/bin
install:
    cargo install {{_feat}} --path crates/coop-gateway

# Run the TUI
run:
    cargo run {{_feat}} --bin coop -- {{_conf}} chat

# Start the gateway daemon
start:
    cargo run {{_feat}} --bin coop -- {{_conf}} start

# Fix all auto-fixable issues
fix:
    cargo fmt --all
    cargo clippy --fix --allow-dirty --allow-staged
    taplo fmt
    typos -w

# Install git hooks from .githooks/
hooks:
    git config core.hooksPath .githooks
    @echo "✅ git hooks installed from .githooks/"

# Link a Signal account to coop
signal-link:
    cargo run --features signal --bin coop -- {{_conf}} signal link

# Run TUI with JSONL tracing
trace:
    COOP_TRACE_FILE={{trace_file}} cargo run {{_feat}} --bin coop -- {{_conf}} chat

# Run gateway daemon with JSONL tracing
trace-gateway:
    COOP_TRACE_FILE={{trace_file}} cargo run {{_feat}} --bin coop -- {{_conf}} start

# Tail recent trace events (current file)
trace-tail n="50":
    tail -n {{n}} {{trace_file}}

# Follow traces with friendly colors (live tail)
trace-follow:
    touch {{trace_file}}
    tail -f {{trace_file}} | jq -r --unbuffered -f scripts/trace-colorize.jq

# Show errors from traces
trace-errors:
    grep '"level":"ERROR"' {{trace_file}} | tail -20

# Show warnings from traces
trace-warnings:
    grep '"level":"WARN"' {{trace_file}} | tail -20

# Show tool execution spans
trace-tools:
    grep 'tool_execute' {{trace_file}} | tail -20

# Show API request spans
trace-api:
    grep 'anthropic_request' {{trace_file}} | tail -20

# Show agent turn spans
trace-turns:
    grep 'agent_turn' {{trace_file}} | tail -20

# Clear current trace file and all rotated archives
trace-clear:
    #!/bin/bash
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    rm -f "{{trace_file}}"
    archives=$(find . -maxdepth 1 -name "${prefix}.*.jsonl" 2>/dev/null)
    if [ -n "$archives" ]; then
        echo "$archives" | xargs rm -v
    fi

# List current trace file and all rotated archives
trace-list:
    #!/bin/bash
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    { [ -f "{{trace_file}}" ] && echo "{{trace_file}} (current)"; \
      find . -maxdepth 1 -name "${prefix}.*.jsonl" | sort; \
    } | grep . || echo "No trace files found"

# Search all trace files (current + archives) for a pattern
trace-grep pattern:
    #!/bin/bash
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    files=$(find . -maxdepth 1 \( -name "{{trace_file}}" -o -name "${prefix}.*.jsonl" \) | sort)
    if [ -n "$files" ]; then
        grep '{{pattern}}' $files | tail -40
    else
        echo "No trace files found"
    fi

# Clean old rotated archives (keep last N, default 7)
trace-clean n="7":
    #!/bin/bash
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    archives=$(find . -maxdepth 1 -name "${prefix}.*.jsonl" | sort -r)
    to_remove=$(echo "$archives" | tail -n +$(({{n}}+1)))
    if [ -n "$to_remove" ]; then
        echo "$to_remove" | xargs rm -v
    else
        echo "Nothing to clean ({{n}} or fewer archives)"
    fi

# Show total size of all trace files (current + archives)
trace-size:
    #!/bin/bash
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    files=$(find . -maxdepth 1 \( -name "{{trace_file}}" -o -name "${prefix}.*.jsonl" \) 2>/dev/null)
    if [ -n "$files" ]; then
        echo "$files" | xargs du -ch | tail -1
    else
        echo "0	total"
    fi

# Archive rotated trace files older than N days
trace-archive days="7" dir="archive":
    #!/bin/bash
    mkdir -p {{dir}}
    prefix=$(echo "{{trace_file}}" | sed 's/\.jsonl$//')
    files=$(find . -maxdepth 1 -name "${prefix}.*.jsonl" -mtime +{{days}})
    if [ -n "$files" ]; then
        echo "Archiving files older than {{days}} days:"
        echo "$files" | xargs -I {} mv -v {} {{dir}}/
    else
        echo "No archives older than {{days}} days"
    fi

# ---------------------------------------------------------------------------
# Code quality & analysis
# ---------------------------------------------------------------------------

# Spell-check code, docs, and comments
typos:
    typos

# Find dead code: orphan .rs files + unused dependencies
dead-code:
    @./scripts/find-orphan-files.sh
    cargo machete

# Code coverage report (text summary)
coverage:
    cargo llvm-cov --workspace --text

# Code coverage report (HTML, opens in browser)
coverage-html:
    cargo llvm-cov --workspace --html
    @echo "Report: target/llvm-cov/html/index.html"

# Mutation testing (slow — run on specific crates)
mutants crate="coop-core":
    cargo mutants -p {{crate}}

# Show which generics produce the most LLVM IR (compile-time hotspots)
llvm-lines crate="coop-gateway":
    cargo llvm-lines -p {{crate}} | head -30

# Module dependency tree for a crate
modules crate="coop-gateway":
    cargo modules generate tree --with-types -p {{crate}}

# Workspace crate dependency graph (DOT format)
depgraph:
    cargo depgraph --workspace-only | dot -Tsvg -o deps.svg
    @echo "Written: deps.svg"

# Unsafe usage audit across all dependencies
geiger:
    cargo geiger

# Supply chain audit (cargo-vet)
vet:
    cargo vet

# Show outdated dependencies
outdated:
    cargo outdated --workspace --root-deps-only

# Binary size analysis
bloat:
    cargo bloat --release -p coop-gateway -n 20

# Full analysis suite (not for CI — slow, informational)
analyze: coverage outdated machete geiger bloat
