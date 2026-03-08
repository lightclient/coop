# Prompt: Airtight Workspace Isolation

Implement airtight per-principal workspace isolation across Coop. Treat workspace directories as **real security boundaries**, not just conventions or prompt hints.

## Goal

Make file/path access safe across **all Coop tools and all Coop-controlled file loading paths**, even when `sandbox.enabled = false`.

Today this is not true because:

- `ToolContext.workspace` is the global workspace
- only `bash` is sandboxed
- `read_file` / `write_file` / `edit_file` are not sandboxed
- image auto-injection loads host files from message text
- Signal attachment handling uses a shared attachments directory
- some helpers do ad-hoc filesystem checks outside the main file tools

Fix that. The directory boundary must be enforced centrally in Rust, with sandbox as defense-in-depth only.

## Desired security model

### 1) Workspace principals

Use the root workspace as an admin/shared root, with these principal workspaces:

- `users/<sanitized-user>/`
- `groups/<sanitized-group>/`

Create helpers for stable, sanitized directory names. Do **not** use raw group ids directly as directory names if they contain unsafe characters; sanitize or hash as needed.

### 2) Scope rules

#### Non-group sessions

For DM / main / terminal / isolated / cron sessions:

- `Owner` and `Full` trust:
  - tool/file scope root = global workspace root
  - they can see `users/*` and `groups/*`
- `Inner`, `Familiar`, `Public`:
  - tool/file scope root = `users/<current-user>/`
  - they must **not** be able to read/write any sibling user workspace or any group workspace
- If a low-trust turn has no mapped `user_name`, **fail closed** for path-based access

#### Group sessions

For `SessionKind::Group(_)`:

- tool/file scope root = `groups/<current-group>/`
- this applies **regardless of sender trust**
- a group turn must **not** be able to read any `users/*`
- a group turn must **not** be able to read any other `groups/*`

This is intentional: groups are their own principal and should not inherit private user workspace access.

### 3) Enforcement must not depend on sandbox

Even with sandbox disabled:

- `read_file`
- `write_file`
- `edit_file`
- `signal_send_image`
- attachment saving/loading
- image auto-injection
- any helper that checks file existence or loads file bytes

must all honor the current scoped workspace.

Sandbox should then be updated so `bash` is mounted/rooted to the same scoped workspace, but sandbox is **not** the primary enforcement mechanism.

## Implementation approach

### A. Add a central scope/path policy

Create a small, lightweight central helper for workspace scoping and path authorization. Keep it dependency-light.

Suggested shape:

- `WorkspaceScope` or `PathScope`
- knows:
  - session kind
  - effective trust
  - user name
  - root workspace
  - scoped root for this turn
- exposes:
  - `scope_root() -> &Path`
  - `resolve_user_path(...)`
  - `resolve_host_path_for_read(...)`
  - `resolve_host_path_for_write(...)`
  - `contains_host_path(...)`

Important:
- Canonicalize and reject escapes
- Reject symlink traversal outside the scope root
- Prefer one shared resolver used everywhere rather than ad-hoc path logic in each tool

### B. Change `ToolContext`

Extend `ToolContext` so tools get the scoped root, not just the global workspace.

Current code uses the global workspace in `Gateway::tool_context()`. Change that so tool-facing path access uses the resolved scope root.

Keep this small and clean. If you need both:
- global workspace root
- scoped tool workspace root

add both explicitly so the distinction is obvious.

### C. Update native file tools

Update:

- `crates/coop-core/src/tools/read_file.rs`
- `crates/coop-core/src/tools/write_file.rs`
- `crates/coop-core/src/tools/edit_file.rs`

to use the central scope resolver.

Behavior:
- relative paths resolve within the scoped root
- path traversal is denied
- absolute paths are not allowed in tool-facing APIs
- errors should clearly say access is outside the current workspace scope

### D. Update bash sandbox scope

Update sandbox plumbing so the sandbox root/mount is the scoped root, not the global workspace.

Relevant files:
- `crates/coop-gateway/src/sandbox_executor.rs`
- `crates/coop-sandbox/src/policy.rs`
- `crates/coop-sandbox/src/linux.rs`
- `crates/coop-sandbox/src/apple.rs`

Requirements:
- `bash` in a low-trust user turn sees only `users/<that-user>/`
- `bash` in a group turn sees only `groups/<that-group>/`
- owner/full non-group turns still get the global workspace root
- keep sandbox behavior aligned with the Rust-side path policy

### E. Update attachment storage

Stop using one shared `workspace/attachments`.

Instead:

- DM/user-scoped attachments go under:
  - `users/<user>/attachments/`
- group attachments go under:
  - `groups/<group>/attachments/`

Use the session/group principal, not a global shared directory.

For Coop-generated file references in chat history, do **not** emit raw host absolute paths. Prefer scope-relative paths such as:

- `./attachments/photo.jpg`

This is important so later image injection can resolve relative to the current scoped root.

Relevant files:
- `crates/coop-gateway/src/main.rs`
- `crates/coop-channels/src/signal.rs`

### F. Update image auto-injection

Current image injection reads host paths from message text. Make it scope-aware.

Update:
- `crates/coop-core/src/images.rs`
- call sites in `crates/coop-gateway/src/gateway.rs`

Requirements:
- only load images that resolve inside the current scoped workspace
- reject out-of-scope paths
- reject path escapes like `../other-user/...`
- for low-trust/group scopes, do not load arbitrary absolute host paths
- pass scope info into image injection instead of using host-global path resolution

If needed, change `inject_images_for_provider(...)` to accept scope/root context.

### G. Update Signal send-image tool

`signal_send_image` currently reads whatever path is passed. Make it use the same scoped path policy as file tools.

Relevant file:
- `crates/coop-channels/src/signal_tools.rs`

Requirements:
- relative paths resolve inside current scope
- absolute host paths are rejected
- out-of-scope paths are rejected

### H. Audit all remaining ad-hoc file access

Audit other tool-facing or user-influenced file reads/checks and route them through the same scope helper.

At minimum inspect:
- `crates/coop-gateway/src/memory_tools.rs` (`file_exists_in_workspace`)
- any helper that does `exists()`, `read()`, `read_to_string()`, `write()` on a user-supplied or message-derived path

### I. Prompt/runtime clarity

Update runtime context text so the model sees the actual effective working area.

Relevant files:
- `crates/coop-core/src/prompt.rs`
- possibly `crates/coop-gateway/src/gateway.rs`

Suggested behavior:
- for low-trust user sessions, indicate the effective home/workspace is the user workspace
- for group sessions, indicate the effective home/workspace is the group workspace

Do not rely on this text for security; it is only clarity for the model.

## Acceptance criteria

### User workspace isolation

- Alice (`full`) in a non-group session can read `users/bob/...`
- Bob (`inner`) in a non-group session can read/write only `users/bob/...`
- Bob cannot read `users/alice/...`
- Bob cannot read `groups/<any>/...`

### Group isolation

- A group turn can read/write only `groups/<current-group>/...`
- A group turn cannot read any `users/*`
- A group turn cannot read any other `groups/*`
- This remains true even if the sender is `full` or `owner`

### Tool coverage

- `read_file`, `write_file`, `edit_file` enforce scope
- `bash` sees only the scoped root when sandbox is enabled
- `signal_send_image` enforces scope
- image auto-injection enforces scope
- attachment save/load uses scoped locations

### Path handling

- relative paths work
- `../` escapes are denied
- symlink escapes are denied
- absolute host paths are rejected for tool-facing path APIs

### Attachments/images

- DM attachment for Alice is saved under `users/alice/attachments/...`
- group attachment is saved under `groups/<group>/attachments/...`
- message content uses scope-relative refs like `./attachments/...`
- image injection loads those refs only within the current scope

## Tests

Add focused tests for:

### Scope resolution

- session kind + trust + user name -> expected scoped root
- sanitized group dir naming is stable and safe

### File tools

- low-trust user cannot escape own workspace
- owner/full non-group can access other users
- group session cannot access any user workspace
- symlink escape is blocked

### Image injection

- in-scope `./attachments/x.png` loads
- out-of-scope path is skipped
- absolute host path is skipped/rejected for low-trust/group scopes

### Signal attachment handling

- DM attachment lands in the user workspace
- group attachment lands in the group workspace
- rewritten message content uses scoped relative paths

### Sandbox plumbing

- sandbox policy/workdir/mount uses the scoped root, not global workspace

### Any audited helpers

- e.g. `memory_tools` file existence checks do not leak out-of-scope file existence

Use placeholder names only: Alice, Bob, Carol, etc.

## Tracing and verification

Add tracing around scope resolution and denied accesses.

At minimum log:
- scoped root
- session kind
- effective trust
- user/group principal
- denied path attempts

Verify tracing by actually running with `COOP_TRACE_FILE=traces.jsonl` and checking the expected fields appear.

Because this touches signal handling, tools, and agent turns, end-to-end verification is mandatory:
- run targeted crate tests
- run `cargo fmt`
- run `cargo clippy --all-targets --all-features -- -D warnings`
- use the `signal-e2e-test` skill to verify the behavior over real Signal; unit tests alone are not sufficient
- include at least DM and group scenarios that confirm workspace boundaries, attachment scoping, and image/path isolation end-to-end

## Constraints

- Keep files small; extract a focused module if needed
- Use `anyhow::Result`
- Do not add heavy dependencies
- Do not add config knobs unless truly necessary; derive from existing session kind / user / trust
- Keep the design simple: one central scope policy used everywhere
- Prefer fail-closed behavior over permissive fallback

## Relevant files

Start here:

- `crates/coop-gateway/src/gateway.rs`
- `crates/coop-core/src/traits.rs`
- `crates/coop-core/src/tools/read_file.rs`
- `crates/coop-core/src/tools/write_file.rs`
- `crates/coop-core/src/tools/edit_file.rs`
- `crates/coop-gateway/src/sandbox_executor.rs`
- `crates/coop-sandbox/src/policy.rs`
- `crates/coop-sandbox/src/linux.rs`
- `crates/coop-sandbox/src/apple.rs`
- `crates/coop-core/src/images.rs`
- `crates/coop-channels/src/signal.rs`
- `crates/coop-channels/src/signal_tools.rs`
- `crates/coop-gateway/src/memory_tools.rs`
- `crates/coop-core/src/prompt.rs`

Deliver the code, tests, and a short summary of the final scope model and any edge cases you chose for cron/isolated sessions.
