# Task: Auto-detect and inject images into API requests

Implement OpenClaw-style image auto-detection for coop. Before each provider call, scan user message text for image file paths, load matching files from disk, and inject them as native `Content::Image` blocks so vision-capable models can see them. No new tools needed — the system detects paths automatically.

## How it works end-to-end

1. User sends a Signal message with a photo attachment
2. `receive_task` downloads and saves it: `[file saved: /workspace/attachments/1234_photo.jpg]`
3. Message reaches the gateway, gets appended as `Message::user().with_text(...)`
4. Before the provider call, the image detector scans that text, finds the `.jpg` path, reads the file, base64-encodes it, and adds a `Content::Image` block to the message
5. The provider serializes the image block as Anthropic's `image` content type
6. Claude sees the actual image

This also works when the agent itself creates or downloads images via bash and the path appears in a tool result or subsequent message.

## Changes needed

### 1. Image detection and loading (`crates/coop-core/src/images.rs` — new file)

Create a module with:

- `detect_image_paths(text: &str) -> Vec<String>` — regex-scan text for file paths ending in `.jpg`, `.jpeg`, `.png`, `.gif`, `.webp`. Match patterns:
  - Absolute paths: `/path/to/image.png`
  - Paths inside brackets: `[file saved: /path/to/image.jpg]`
  - Home-relative: `~/images/photo.png`
  - Relative: `./screenshot.png`
  - Don't match URLs (`http://`, `https://`)

- `load_image(path: &str) -> Result<(String, String)>` — read file, base64-encode, detect mime type from extension. Returns `(base64_data, mime_type)`. Cap at 5MB (Anthropic's limit). Return error for files that don't exist or exceed the limit.

- `inject_images_into_message(message: &mut Message)` — scan all `Content::Text` blocks in the message for image paths. For each found path, try to load it. On success, append a `Content::Image` block to the message. Deduplicate by path (don't inject the same image twice even if the path appears multiple times). Don't modify the text — the path reference stays so the model has context about what the image is.

Keep this module lightweight — only `std`, `base64`, `anyhow`. No new dependencies on `coop-core` (compile time rule from AGENTS.md). The `base64` crate is already a transitive dep but check if it's direct; if not, add it to `coop-core/Cargo.toml`.

Export from `crates/coop-core/src/lib.rs`.

### 2. Call the image injector before provider calls (`crates/coop-gateway/src/gateway.rs`)

In `run_turn_with_trust`, after `self.append_message(session_key, Message::user().with_text(user_input))` and before the turn iteration loop, call `inject_images_into_message` on the last user message in the session. This handles inbound messages (Signal attachments, terminal input with paths).

Also, inside the turn iteration loop, after building `result_msg` from tool results and before `self.append_message(session_key, result_msg.clone())`, call `inject_images_into_message` on the tool result message. This handles the case where a tool result (e.g. bash output) contains an image path.

For conversation history: before each provider call, scan ALL user and tool-result messages in the session for image paths that haven't been injected yet (messages that have `Content::Text` with image paths but no corresponding `Content::Image`). This enables follow-up questions about images from earlier turns. Only inject into the in-memory session messages passed to the provider — don't persist injected images to the session store (they'd bloat disk storage with base64 data).

### 3. Serialize `Content::Image` in the Anthropic provider (`crates/coop-agent/src/anthropic_provider.rs`)

At line ~399, replace:
```rust
_ => None, // Skip Image, Thinking for now
```

With serialization for `Content::Image`:
```rust
Content::Image { data, mime_type } => Some(json!({
    "type": "image",
    "source": {
        "type": "base64",
        "media_type": mime_type,
        "data": data
    }
})),
Content::Thinking { .. } => None,
```

This matches the [Anthropic vision API format](https://docs.anthropic.com/en/docs/build-with-claude/vision).

### 4. Tests

In `crates/coop-core/tests/` or inline:
- `detect_image_paths` finds absolute, relative, home, and bracket-wrapped paths
- `detect_image_paths` ignores URLs and non-image extensions
- `detect_image_paths` deduplicates
- `load_image` reads a real temp file and returns valid base64 + correct mime type
- `load_image` returns error for missing files
- `load_image` returns error for files over 5MB
- `inject_images_into_message` adds `Content::Image` blocks and preserves original text
- `inject_images_into_message` is idempotent (calling twice doesn't duplicate images)

In `crates/coop-agent/` tests:
- `Content::Image` serializes to the correct Anthropic JSON format

## What NOT to do

- Don't add a `view_image` tool — auto-detection is the mechanism
- Don't modify `Content` or `ToolOutput` types — `Content::Image` already exists
- Don't download remote URLs — local files only (security)
- Don't persist base64 image data to the session store on disk — inject at provider-call time only
- Don't add heavy image processing deps to `coop-core` (compile time) — just read bytes and base64-encode. If the image is too large, reject it; don't resize.

## Reference

See OpenClaw's implementation in `~/openclaw/src/agents/pi-embedded-runner/run/images.ts` for the detection regex patterns and `~/openclaw/src/agents/pi-embedded-runner/run/attempt.ts` lines 778-820 for the injection point before the provider call.
