# Compile Time Optimizations

Coop is primarily developed through agentic coding loops: an AI agent edits code, builds, tests, reads errors, and iterates. Every second of compile time is a second the agent (and the human) waits before getting feedback. Fast incremental builds are critical to productive agent sessions.

This document explains what we do to keep builds fast, why each optimization matters, and how to preserve these properties as the codebase grows.

## Current numbers

| Scenario | Time |
|----------|------|
| Full clean build | ~14s |
| Incremental (leaf crate touch, e.g. `coop-gateway`) | ~0.6s |
| Incremental (root crate touch, e.g. `coop-core`) | ~1.0s |

The goal is to keep **incremental gateway builds under 1 second** for the foreseeable future.

## Toolchain configuration

### Linker: mold

We use [mold](https://github.com/rui314/mold), the fastest ELF linker available. Traditional linkers (`ld`, `gold`) are largely single-threaded; mold aggressively parallelizes all linking phases. On incremental builds where codegen is fast, linking is a significant fraction of total time.

Configuration in `.cargo/config.toml`:

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

Install: `apt install mold`

### sccache

[sccache](https://github.com/mozilla/sccache) wraps `rustc` and caches compilation artifacts. This eliminates redundant work when switching branches, after `cargo clean`, or when CI re-runs builds that haven't changed.

```toml
[build]
rustc-wrapper = "sccache"
```

Install: `cargo install sccache`

## Cargo profile

In the workspace `Cargo.toml`:

```toml
[profile.dev]
debug = "line-tables-only"

[profile.dev.package."*"]
opt-level = 1
```

**`debug = "line-tables-only"`** — Generates minimal debug info (file/line for backtraces) instead of full DWARF. This speeds up both compilation and linking by producing smaller object files.

**`opt-level = 1` for dependencies** — Counterintuitive but effective: slightly optimizing third-party crates reduces monomorphization bloat, producing smaller object files that link faster. The one-time cost on clean builds is repaid on every incremental build. Dev runtime is also faster since hot paths in `tokio`, `serde`, etc. aren't fully unoptimized.

## Crate architecture for compile speed

### Dependency graph shape

The ideal shape for compile speed is a **wide, shallow** dependency graph. Crates that don't depend on each other compile in parallel. Our graph:

```
coop-core (root — everything depends on this)
├── coop-agent
├── coop-channels
├── coop-ipc
├── coop-tui
└── coop-gateway (leaf — depends on all of the above)
```

Changes to `coop-core` trigger recompilation of all crates. Changes to `coop-agent` only trigger `coop-gateway`. This is why it's critical to keep `coop-core` small and stable.

### File separation in leaf crates

Large files in leaf crates (especially `coop-gateway`) hurt incremental builds because any change to the file recompiles the entire crate. We split `main.rs` into focused modules:

```
crates/coop-gateway/src/
├── main.rs          # entry point, command dispatch, event loops
├── cli.rs           # CLI argument parsing (clap structs)
├── config.rs        # config loading
├── gateway.rs       # gateway logic
├── router.rs        # message routing
├── tracing_setup.rs # tracing subscriber init
├── trust.rs         # trust level logic
└── tui_helpers.rs   # TUI layout, chat rendering, welcome banner
```

Touching `tui_helpers.rs` recompiles `coop-gateway` but doesn't require re-analyzing `cli.rs` or `router.rs` — Rust's incremental compilation tracks per-function dependencies within a crate.

### Minimizing per-crate dependencies

Each crate only pulls in the tokio features it actually uses:

| Crate | Tokio features |
|-------|---------------|
| `coop-core` | `process`, `fs`, `time`, `sync`, `macros`, `rt` |
| `coop-agent` | `time`, `sync`, `macros`, `rt` |
| `coop-ipc` | `net`, `io-util`, `sync`, `macros`, `rt` |
| `coop-channels` | `sync`, `time`, `macros`, `rt`, `rt-multi-thread` |
| `coop-tui` | *(none — does not depend on tokio)* |
| `coop-gateway` | `full` (binary entry point) |

Only the final binary (`coop-gateway`) uses `tokio = { features = ["full"] }`. Library crates use minimal feature sets.

### Feature-gated heavy dependencies

`tiktoken-rs` (tokenizer) is behind an optional `tokenizer` feature in `coop-core`. When the feature is disabled (e.g. for fast dev iteration on unrelated code), token counting falls back to a `chars/4` approximation. The feature is enabled by default so production builds get accurate counts.

```toml
[features]
default = ["tokenizer"]
tokenizer = ["dep:tiktoken-rs"]
```

## What NOT to do

- **Don't add `tokio = { features = ["full"] }` to library crates.** Use the minimum features needed.
- **Don't put everything in one file.** Large files defeat incremental compilation.
- **Don't add heavy dependencies to `coop-core`.** Everything depends on it, so its compile time multiplies.
- **Don't use `#[derive(...)]` macros from slow proc-macro crates unless necessary.** Each proc-macro crate adds to the critical path.
- **Don't add `reqwest` or other HTTP crates to more places.** Keep network-heavy deps in `coop-agent`.

## Measuring

### Quick check

```bash
# Incremental (leaf change)
touch crates/coop-gateway/src/main.rs && time cargo build

# Incremental (root change)  
touch crates/coop-core/src/lib.rs && time cargo build

# Clean build
cargo clean && time cargo build
```

### Detailed timing

```bash
cargo build --timings
# Opens target/cargo-timings/cargo-timing.html
```

This shows per-crate compile times and parallelism. Look for:
- Crates on the critical path (longest sequential chain)
- Crates that could compile in parallel but are blocked by unnecessary deps
- Individual crates that take disproportionately long
