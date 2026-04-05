# Prompt: Full Subagent System

Implement a first-class subagent system in Coop.

The key architectural decision is:

**specialized models should be selected at the subagent/session level, not via image-specific top-level config like `agent.image_model` or `agent.image_generation_model`.**

That means:
- main turns stay on the main/session model
- model switching is always explicit via subagent spawn or explicit tool arguments
- image/vision/editing/generation tasks are just examples of work a specialized subagent can do
- do not keep expanding the architecture around dedicated image-model settings

This is a substantial feature. Read the design material first, then implement in phases. Keep compile times in mind and split code into focused modules under ~500 lines.

## Read first

- `AGENTS.md`
- `docs/compile-times.md`
- `docs/design.md`
- `docs/hermes-ideas-for-coop.md`
- `crates/coop-core/src/traits.rs`
- `crates/coop-core/src/types.rs`
- `crates/coop-core/src/prompt.rs`
- `crates/coop-core/src/images.rs`
- `crates/coop-core/src/workspace_scope.rs`
- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/config_watcher.rs`
- `crates/coop-gateway/src/gateway.rs`
- `crates/coop-gateway/src/router.rs`
- `crates/coop-gateway/src/commands.rs`
- `crates/coop-gateway/src/main.rs`
- `crates/coop-gateway/src/provider_registry.rs`
- `crates/coop-gateway/src/provider_factory.rs`
- `crates/coop-gateway/src/session_search.rs`
- `crates/coop-gateway/src/user_model_store.rs`
- `crates/coop-gateway/src/vision_tools.rs`
- `crates/coop-gateway/src/image_generation_tools.rs`
- `workspaces/default/TOOLS.md`
- `README.md`

## Goal

Add a real subagent runtime to Coop with:

- explicit child-session spawning
- fresh isolated child context
- child model/profile selection
- bounded tool scoping
- blocking and background modes
- persisted child run metadata
- cancellation propagation
- slash-command inspection/control
- path and attachment handoff for media tasks
- strong tracing

At the same time, move Coop away from image-specific model routing/config.

Final architecture should make this true:

- the main agent has one primary model
- if the agent wants a different model for a subtask, it spawns a subagent
- image/media tasks are handled by subagents plus tools/capabilities, not by `agent.image_model` / `agent.image_generation_model`

## Non-goals

- Do not add hidden automatic model switching based on message content.
- Do not route ordinary main-session turns to a different model just because a message mentions or contains an image.
- Do not introduce a large generic nested-orchestration system before the basics are solid.
- Do not add heavy dependencies to `coop-core`.

## Design principles

### 1. Subagents are the specialization mechanism

Subagents are the standard way to do:
- cheaper/faster side work
- model-specific work
- multimodal work
- long-running background work
- bounded isolated experimentation

### 2. Main turns stay simple

Normal conversation turns must continue to use the current main/session model. A different model is only used when the agent explicitly spawns a child.

### 3. Fresh child context by default

Follow Hermes here:
- child gets a fresh conversation
- child gets delegated `task` and `context`
- child does **not** inherit the full parent transcript
- child does **not** inherit memory by default
- child prompt should be minimal by default

### 4. Runtime lifecycle should be full-featured

Follow OpenClaw here:
- child session identity
- parent/child lineage
- background execution
- registry/state tracking
- list/inspect/kill control surface
- cancellation propagation

### 5. Media tasks should fit naturally into the subagent model

Do not build the architecture around separate image-specific model settings.

Instead:
- a subagent may use a multimodal model directly
- a subagent may also call dedicated tools such as `vision_analyze`, `image_generate`, and future `image_edit`
- those tools should use the current session/subagent model or an explicit tool arg, not dedicated `agent.image_*` config

## Final public surface

## A. `subagent_spawn` tool

Add a native tool something like:

```json
{
  "name": "subagent_spawn",
  "description": "Spawn a child agent in a fresh isolated session to complete a delegated task.",
  "parameters": {
    "type": "object",
    "properties": {
      "task": { "type": "string" },
      "context": { "type": "string" },
      "profile": { "type": "string" },
      "model": { "type": "string" },
      "tools": {
        "type": "array",
        "items": { "type": "string" }
      },
      "paths": {
        "type": "array",
        "items": { "type": "string" }
      },
      "mode": {
        "type": "string",
        "enum": ["wait", "background"]
      },
      "max_turns": { "type": "integer" },
      "timeout_seconds": { "type": "integer" }
    },
    "required": ["task"]
  }
}
```

Semantics:
- `task`: required, self-contained goal for the child
- `context`: optional extra background
- `profile`: named child profile from config
- `model`: explicit override
- `tools`: optional further narrowing of child tools
- `paths`: workspace-relative files to give the child
- `mode = wait`: block until completion and return structured result
- `mode = background`: return immediately with a run id and child session info

## B. `subagents` control tool

Add a second native tool for orchestration/control:
- `list`
- `inspect`
- `kill`

You can start with:

```json
{
  "name": "subagents",
  "description": "List, inspect, or stop active/recent subagent runs.",
  "parameters": {
    "type": "object",
    "properties": {
      "action": {
        "type": "string",
        "enum": ["list", "inspect", "kill"]
      },
      "run_id": { "type": "string" }
    },
    "required": ["action"]
  }
}
```

Do not start with `steer` unless the basics are solid.

## C. Slash commands

Add:
- `/subagents`
- `/subagents list`
- `/subagents inspect <id>`
- `/subagents kill <id>`

Update `/help` accordingly.

## D. Models/help/status UX

Move the UX away from image-model-specific controls.

Final UX should prefer:
- `/model <id>` for the primary session model
- `/subagents ...` for subagent control
- docs/help text that tells the assistant/operator to use subagent profiles for specialized models

`/image-model` and `/image-generation-model` should not remain first-class runtime controls.

## Config design

Add a dedicated subagent config section.

Suggested shape:

```toml
[agent]
id = "coop"
model = "gpt-5-codex"
workspace = "./workspaces/default"

[agent.subagents]
enabled = true
model = "gpt-4.1-mini"
max_spawn_depth = 2
max_active_children = 5
max_concurrent = 4
default_timeout_seconds = 900
default_max_turns = 25
prompt_mode = "minimal"
inherit_memory = false

[agent.subagents.profiles.code]
model = "gpt-5-codex"
tools = ["bash", "read", "edit", "write"]
prompt_mode = "minimal"
default_timeout_seconds = 900
allow_spawn = false

[agent.subagents.profiles.research]
model = "gpt-4.1-mini"
tools = ["read", "web_fetch", "session_search"]
prompt_mode = "minimal"
allow_spawn = false

[agent.subagents.profiles.media]
model = "gpt-4o"
tools = ["read", "write", "vision_analyze", "image_generate"]
prompt_mode = "minimal"
default_timeout_seconds = 600
allow_spawn = false
```

Suggested types:
- `SubagentsConfig`
- `SubagentProfileConfig`
- `SubagentPromptMode`

Model resolution order for a child should be:
1. explicit `subagent_spawn.model`
2. `profile.model`
3. `agent.subagents.model`
4. parent session model

## Required cleanup from current state

There is already image-model-specific runtime logic in the repo. This feature should move Coop away from that design.

Current files to clean up or replace include at least:
- `crates/coop-gateway/src/config.rs`
- `crates/coop-gateway/src/config_check.rs`
- `crates/coop-gateway/src/config_watcher.rs`
- `crates/coop-gateway/src/gateway.rs`
- `crates/coop-gateway/src/commands.rs`
- `crates/coop-gateway/src/router.rs`
- `crates/coop-gateway/src/vision_tools.rs`
- `crates/coop-gateway/src/image_generation_tools.rs`
- `workspaces/default/TOOLS.md`
- `README.md`

Specifically:
- stop designing around `agent.image_model`
- stop designing around `agent.image_generation_model`
- stop designing around `/image-model`
- stop designing around `/image-generation-model`
- remove or deprecate the per-user image-model override stores and related gateway helpers
- stop tagging `/models` output with image-specific model state
- stop describing image-specific model settings as the main customization story

Preferred end state:
- `vision_analyze`, `image_generate`, and future `image_edit` use the current session/subagent model unless an explicit tool argument says otherwise
- specialized models are chosen by the child session/profile, not a special image config knob

If you need a short compatibility window for config parsing, keep it read-only and loudly deprecated via `coop check`, but do not keep expanding the old runtime control surface.

## Session/runtime architecture

## 1. Add a real subagent session kind

In `crates/coop-core/src/types.rs`, add:

```rust
SessionKind::Subagent(Uuid)
```

Do not overload `Isolated` for this. A real session kind makes lineage, tracing, control, and policy much clearer.

Update display formatting and any parsing/tests that assume the current set of session kinds.

## 2. Add a subagent run registry

Create focused modules under `crates/coop-gateway/src/subagents/`, for example:
- `mod.rs`
- `spawn.rs`
- `registry.rs`
- `policy.rs`
- `prompt.rs`
- `runtime.rs`
- `announce.rs`
- `attachments.rs`
- `commands.rs`

Keep file sizes small.

Suggested persisted run record fields:
- `run_id`
- `child_session_key`
- `parent_session_key`
- `parent_run_id` optional
- `requesting_user`
- `task`
- `profile`
- `model`
- `status`
- `depth`
- `created_at`
- `started_at`
- `ended_at`
- `timeout_seconds`
- `artifact_paths`
- `summary`
- `error`

Statuses:
- `queued`
- `running`
- `completed`
- `failed`
- `cancelled`
- `timed_out`

Persist this in workspace state under `coop-gateway`, not `coop-core`.

## 3. Dedicated queue lane

Run subagents on a dedicated bounded lane/semaphore so they do not starve normal chat turns.

Use config:
- `max_concurrent`
- `max_active_children`
- `max_spawn_depth`

## Prompt and context model

Default child prompt behavior should follow the Hermes approach.

## Minimal child context

By default, child gets:
- built-in subagent instructions
- delegated `task`
- delegated `context`
- explicit file/path list
- `AGENTS.md`
- `TOOLS.md`

By default, child does **not** get:
- full parent transcript
- parent hidden reasoning
- memory prompt injection
- the full normal prompt stack

Add a `prompt_mode` setting such as:
- `minimal`
- `full`

Default should be `minimal`.

## Parent/child transcript boundary

Parent should receive a structured result/summary, not the full raw child transcript.

The full child transcript should remain available through the child session history and control surface.

## Tool policy and safety

A child must never gain more power than the parent.

Compute child tool availability as:

`parent_visible_tools`
∩ `profile.tools` if present
∩ `request.tools` if present
− default child denylist
− profile denies
− request denies

Default denylist for children should include at least:
- `subagent_spawn`
- `subagents`
- channel-send tools by default
- config mutation tools
- cron/scheduler mutation tools
- any other sensitive orchestration tools that would let children escalate

If `max_spawn_depth > 1`, only explicitly allowed orchestrator profiles should get spawn capability.

## Execution modes

Support both from the start.

## `mode = "wait"`

Blocking delegation.

Return structured JSON like:

```json
{
  "success": true,
  "run_id": "...",
  "child_session": "...",
  "status": "completed",
  "summary": "...",
  "artifact_paths": ["./generated/result.png"]
}
```

Use this for:
- focused coding subtasks
- research subtasks
- bounded media work

## `mode = "background"`

Non-blocking delegation.

Return immediately:

```json
{
  "success": true,
  "status": "accepted",
  "run_id": "...",
  "child_session": "..."
}
```

Then announce completion back later.

## Completion delivery

Start simple.

When a background child completes, inject a structured synthetic internal message into the parent session with:
- child id
- child session key
- status
- summary
- artifact paths

That gives the parent enough information to continue naturally.

Do not begin with a huge event-bus redesign unless necessary.

## Attachments, paths, and multimodal child input

This is how the media/image use case should fit into the subagent system.

## 1. Start with explicit `paths`

Support `paths: [...]` on `subagent_spawn`.

These should be workspace-relative or otherwise resolved through existing workspace-scope rules. Do not scan arbitrary session history for image paths.

## 2. Child prompt should include file context explicitly

The child should be told exactly which files were supplied.

## 3. If the child model/provider supports images, pass images explicitly

For image paths supplied to the child:
- if the resolved child model/provider can accept image content, build the child input explicitly with `Content::Image` blocks
- use explicit spawn metadata to do this, not transcript scanning
- reuse safe image-loading helpers from `coop-core/src/images.rs`
- preserve workspace-scope enforcement

If the child model/provider does not support images:
- still provide the file paths in text context
- let the child use tools such as `read`, `vision_analyze`, or future media tools

This is important:
- no global automatic image routing
- no dedicated image model knob
- explicit file handoff to the child instead

## 4. Future attachment materialization

After `paths` works, add richer attachment support:
- materialize inbound channel attachments into child-accessible files
- record provenance in the run registry
- keep transcript/logging safe

## Media tool alignment

Do not make image/media tools rely on top-level image-model config.

Update `vision_analyze`, `image_generate`, and future `image_edit` so they resolve their model from:
1. explicit tool arg `model` if provided
2. current session/subagent model otherwise

That means:
- no `agent.image_model`
- no `agent.image_generation_model`
- no special per-user image-model override store

A media-capable subagent profile should be the preferred way to get a different backend/model for those tasks.

## Gateway integration

Integrate subagent support into the gateway in a way that preserves existing turn/session behavior.

Expected pieces:
- spawn request parsing and validation
- parent/child lineage tracking
- child session creation
- child tool filtering
- child model resolution
- child prompt building
- completion handling
- cancellation propagation from parent `/stop`

Parent cancellation must stop active child runs and cascade to nested children if nesting is enabled.

## Tracing requirements

This feature must be heavily traced.

Add spans/events such as:
- `subagent_spawn`
- `subagent_queue`
- `subagent_run`
- `subagent_completion`
- `subagent_cancel`

Important fields:
- `run_id`
- `parent_session`
- `child_session`
- `profile`
- `model`
- `mode`
- `depth`
- `status`
- `timeout_seconds`
- `artifact_paths`
- `tool_count`

For explicit file/media handoff, include enough metadata to debug behavior without leaking secrets.

Verify tracing using `COOP_TRACE_FILE=traces.jsonl` and confirm the expected fields appear in the JSONL output.

A successful build is not enough.

## Suggested implementation phases

## Phase 1: config and types

- add `agent.subagents` config
- add profile config
- add parsing tests in `config.rs`
- add validation in `config_check.rs`
- add hot-reload diffing in `config_watcher.rs`
- add `SessionKind::Subagent(Uuid)` in `coop-core`
- add any new small shared enums/structs needed for subagent requests/results

## Phase 2: registry and blocking spawn

- add `subagents/registry.rs`
- add `subagent_spawn` tool
- implement `mode = wait`
- implement child model/profile resolution
- implement minimal child prompt
- implement child tool policy
- return structured result

This is the first milestone that should feel useful.

## Phase 3: background runs and control

- add dedicated subagent queue lane
- implement `mode = background`
- persist run state transitions
- inject completion message into parent session
- add `subagents` control tool
- add `/subagents list|inspect|kill`
- add cancellation propagation

## Phase 4: explicit file/media handoff

- support `paths`
- explicitly include supplied images in child input when supported
- add attachment materialization module
- improve artifact-path recording

## Phase 5: remove image-specific runtime design

- remove or deprecate `agent.image_model`
- remove or deprecate `agent.image_generation_model`
- remove `/image-model`
- remove `/image-generation-model`
- remove per-user image-model override storage/use
- simplify `vision_analyze` and `image_generate` to session-model semantics
- update `/status`, `/models`, `/help`
- update `README.md`, `workspaces/default/TOOLS.md`, init templates

Compatibility shims are acceptable briefly during development, but the final docs/help/config story must point users toward subagents and profiles, not image-specific model knobs.

## Test plan

Add tests for:

### Unit tests
- subagent config parsing
- subagent config validation
- model resolution precedence
- profile resolution
- tool intersection and deny-wins behavior
- spawn depth enforcement
- timeout normalization
- child prompt composition
- child session key formatting
- registry state transitions

### Integration tests
- blocking `wait` mode completion
- background `accepted` response
- background completion injection
- parent cancellation cascading to children
- `/subagents list`
- `/subagents inspect`
- `/subagents kill`
- `paths` handoff
- explicit image/file handoff to child prompt
- profile-specific model usage

### Trace tests
- spawn event contains expected model/profile/depth
- completion event contains status/artifacts
- cancel event appears when parent stops child
- explicit media/path handoff is trace-visible

Update existing tests that currently assert image-model-specific behavior.

## Verification commands

Run at minimum:

```bash
cargo fmt
cargo build -p coop-gateway
cargo test -p coop-gateway --bin coop
cargo test -p coop-gateway
cargo clippy -p coop-gateway --all-targets -- -D warnings
```

Then verify tracing:

```bash
COOP_TRACE_FILE=traces.jsonl cargo test -p coop-gateway --bin coop <relevant subagent trace test> -- --nocapture
```

Confirm the expected subagent fields appear in `traces.jsonl`.

Because this change touches agent turns, tools, prompt building, and provider calls, follow the repo rule and run the `signal-e2e-test` skill before finalizing. Unit tests alone are not sufficient.

## Acceptance criteria

The implementation is complete when all of the following are true:

- Coop has a real `subagent_spawn` tool.
- Coop has a real `subagents` control surface.
- Child sessions are explicit and traceable.
- Child model/profile selection works.
- `wait` and `background` modes both work.
- Parent cancellation propagates to active children.
- Child toolsets are bounded and cannot escalate beyond the parent.
- Child context is fresh and minimal by default.
- Explicit `paths` handoff works.
- Multimodal child input is supported through explicit child file handoff, not global image routing.
- Current image/media tools no longer depend on top-level image-model config.
- Help/status/docs steer users toward subagents for specialized models.
- Traces clearly show spawn/run/completion/cancel behavior.
- Build, tests, fmt, clippy, and trace verification all pass.

## Important guidance

Do not build a subagent system that is secretly just another layer of image-specific routing.

The point of this work is to make Coop more general:
- one main model for the main session
- explicit specialized child sessions when needed
- generic enough for code, research, and media
- still practical for image workflows because child sessions can receive files and use multimodal models/tools

That is the architecture to aim for.
