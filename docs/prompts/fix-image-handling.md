# Task: Fix image handling — validation, Signal sending, injection efficiency

Three distinct image handling bugs, traced from `bug.jsonl`.

## Bug 1: API rejects injected non-image files (400 "Could not process image")

### What happened (from trace)

1. Agent downloaded a meme via `curl -sL -o /tmp/murray_meme.jpg "https://i.imgflip.com/3kwur0.jpg"`
2. Server returned an HTML page (bot protection redirect) instead of the actual JPEG
3. Tool output confirmed: `"/tmp/murray_meme.jpg: HTML document text, ASCII text, with very long lines (611)"`
4. Image injection code in `coop-core/src/images.rs` (`load_image()`) saw the `.jpg` extension, assumed it was JPEG, base64-encoded the HTML content, and injected it as `Content::Image { mime_type: "image/jpeg", .. }`
5. Anthropic rejected: `"Invalid request (400 Bad Request): Could not process image"`
6. Session rolled back 5 messages

### Root cause

`load_image()` determines MIME type solely from file extension via `mime_from_extension()`. It never validates that the file content is actually an image. Any file with a `.jpg`/`.png`/`.gif`/`.webp` extension gets injected, even if it's HTML, JSON, or garbage.

### Fix: `crates/coop-core/src/images.rs`

Add a `validate_image_magic` function that checks file header bytes before base64-encoding. Call it in `load_image()` after reading the file and before encoding.

Magic byte signatures:
- **JPEG**: starts with `FF D8 FF`
- **PNG**: starts with `89 50 4E 47 0D 0A 1A 0A` (8 bytes)
- **GIF**: starts with `47 49 46 38` (`GIF8`)
- **WEBP**: bytes 0-3 are `52 49 46 46` (`RIFF`) AND bytes 8-11 are `57 45 42 50` (`WEBP`)

```rust
fn validate_image_magic(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 12 {
        return None;
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg".to_owned());
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png".to_owned());
    }
    if bytes.starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        return Some("image/gif".to_owned());
    }
    if bytes.starts_with(&[0x52, 0x49, 0x46, 0x46]) && bytes[8..12] == [0x57, 0x45, 0x42, 0x50] {
        return Some("image/webp".to_owned());
    }
    None
}
```

In `load_image()`, after `let bytes = std::fs::read(p)?;`, call `validate_image_magic(&bytes)`. If it returns `None`, bail with an error like `"file content is not a recognized image format: {path}"`. Use the magic-detected MIME type instead of the extension-based one — this is the authoritative source.

Add tests:
- Valid JPEG magic bytes → returns `image/jpeg`
- Valid PNG magic bytes → returns `image/png`
- HTML content with `.jpg` extension → `load_image` returns error
- File too small (< 12 bytes) → returns `None`
- WEBP detection with RIFF+WEBP combo
- Existing `loads_real_file` test should still pass since fake PNG data won't have magic bytes — update that test to write actual PNG magic bytes (`b"\x89PNG\r\n\x1a\n"` + padding)

### Tracing

Add a `warn!` event when magic validation fails, so it shows up in traces:
```
warn!(path = %path, detected = "not an image", extension = %ext, "image file content does not match extension, skipping injection");
```

## Bug 2: No way to send images over Signal

### What happened (from trace)

The user asked "yeah can you send me the meme" — the agent downloaded an image and tried to get the API to see it (via image injection), but there's no tool to actually send an image back over Signal. The `SignalAction` enum has no attachment variant. The `signal_tools.rs` file has `signal_send` (text only), `signal_react`, `signal_reply`, and `signal_history` — no image sending.

### Root cause

Signal image sending was never implemented. The agent can receive images (via `download_and_rewrite_attachments` in `signal.rs`), but has no way to send them back.

### Fix: Three changes needed

#### 1. Add `SendAttachment` to `SignalAction` enum in `crates/coop-channels/src/signal.rs`

```rust
pub enum SignalAction {
    SendText(OutboundMessage),
    SendAttachment {
        target: SignalTarget,
        path: PathBuf,
        mime_type: String,
        caption: Option<String>,
    },
    React { ... },
    // ... existing variants
}
```

#### 2. Implement the send handler in `send_signal_action()` in `crates/coop-channels/src/signal.rs`

In the `match action` block, add a `SignalAction::SendAttachment` arm. Use presage's attachment upload API:

```rust
SignalAction::SendAttachment { target, path, mime_type, caption } => {
    let target_kind = signal_target_kind(&target);
    let target_value = signal_target_value(&target);
    let timestamp = now_epoch_millis();
    let path_display = path.display().to_string();
    let span = info_span!(
        "signal_action_send",
        signal.action = "send_attachment",
        signal.target_kind = target_kind,
        signal.target = %target_value,
        signal.timestamp = timestamp,
        signal.attachment_path = %path_display,
        signal.attachment_mime = %mime_type,
    );

    let file_data = std::fs::read(&path)
        .with_context(|| format!("failed to read attachment: {}", path.display()))?;
    let file_name = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_owned();

    let attachment = manager.upload_attachment(
        &mime_type,
        file_data,
        Some(file_name),
    ).await.context("failed to upload signal attachment")?;

    let message = DataMessage {
        body: caption,
        attachments: vec![attachment],
        group_v2: group_context_for_target(&target),
        ..Default::default()
    };
    send_action_with_trace(manager, span, target, message, timestamp).await
}
```

Note: check the presage API — the upload method may be `upload_attachment` or may require building an `AttachmentPointer` manually. Look at how presage-based projects handle outbound attachments. The key is: read file → upload to Signal CDN → get pointer → include in `DataMessage.attachments`.

#### 3. Add `signal_send_image` tool in `crates/coop-channels/src/signal_tools.rs`

```rust
pub struct SignalSendImageTool {
    action_tx: mpsc::Sender<SignalAction>,
}
```

Tool definition:
```json
{
    "name": "signal_send_image",
    "description": "Send an image file as a Signal attachment to the current conversation. The file must exist on disk.",
    "parameters": {
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Absolute path to the image file to send"
            },
            "caption": {
                "type": "string",
                "description": "Optional text caption to include with the image"
            }
        },
        "required": ["path"]
    }
}
```

In `execute()`: validate the file exists, detect MIME type (reuse `validate_image_magic` from `coop-core/src/images.rs` — export it), derive the `SignalTarget` from session ID (same as `signal_send`), then dispatch `SignalAction::SendAttachment`.

Register it in `SignalToolExecutor::new()` alongside the existing tools.

### Dependency note

The `signal_send_image` tool needs access to `validate_image_magic` from `coop-core`. This function is already in a leaf module and just does byte comparison — no new deps needed. Export it from `coop-core/src/images.rs` and `coop-core/src/lib.rs`.

### Tracing

The `info_span!` on the new action must include `signal.action = "send_attachment"`, `signal.attachment_path`, and `signal.attachment_mime` so the JSONL trace captures it.

## Bug 3: Stale image path retries on every iteration

### What happened (from trace)

A previous tool output mentioned `./clip.gif`. This path stayed in the session history. On every subsequent provider call, `inject_images_for_provider()` scans ALL user messages, finds `./clip.gif`, tries to load it, fails with "No such file or directory", and logs "skipping image injection". This happened 40+ times across multiple turns in the trace — every single iteration of every turn.

### Root cause

`inject_images_for_provider()` in `crates/coop-core/src/images.rs` clones and scans every user message in the entire session on every iteration. Once a referenced file is deleted/moved, the failure repeats forever.

### Fix: `crates/coop-core/src/images.rs`

Change `inject_images_for_provider()` to only process **the last N user messages** instead of the entire history. Images referenced 20+ messages ago are almost certainly stale. A reasonable default: only inject images from messages within the last 4 user messages (current turn's messages plus some buffer).

```rust
pub fn inject_images_for_provider(messages: &[Message]) -> Vec<Message> {
    let mut cloned: Vec<Message> = messages.to_vec();

    // Only process the last few user messages to avoid stale path retries.
    // Images from deep history are almost certainly gone from disk.
    let user_indices: Vec<usize> = cloned.iter().enumerate()
        .filter(|(_, m)| matches!(m.role, crate::Role::User))
        .map(|(i, _)| i)
        .collect();

    let start_from = if user_indices.len() > 4 {
        user_indices[user_indices.len() - 4]
    } else {
        0
    };

    for i in start_from..cloned.len() {
        if matches!(cloned[i].role, crate::Role::User) {
            inject_images_into_message(&mut cloned[i]);
        }
    }
    cloned
}
```

This bounds the scan window and prevents stale paths from triggering repeated I/O on every iteration.

Additionally: demote the "skipping image injection" log from `debug!` to `trace!` — it's noisy and not actionable. Only the first occurrence per path per turn is interesting. Or add a local `HashSet` of failed paths to skip duplicates within a single injection pass.

### Tests

- Add a test with 10 user messages where only messages 7-10 reference images — verify only those are processed
- Add a test where early messages reference a missing file and later messages reference existing files — verify the missing file is not attempted

## Implementation order

1. **Bug 1 (magic validation)** — smallest, most impactful, no cross-crate changes. Prevents 400 errors.
2. **Bug 3 (stale path retries)** — also in `coop-core/src/images.rs`, quick fix.
3. **Bug 2 (Signal image sending)** — largest change, touches `coop-channels` and adds a new tool. Do this last.

## Files to modify

- `crates/coop-core/src/images.rs` — magic validation, stale path fix, export `validate_image_magic`
- `crates/coop-core/src/lib.rs` — export new public fn if needed
- `crates/coop-channels/src/signal.rs` — `SendAttachment` variant + handler
- `crates/coop-channels/src/signal_tools.rs` — `signal_send_image` tool + registration

## Verification

After all fixes:

1. `cargo test -p coop-core` — magic validation tests pass
2. `cargo test -p coop-channels` — signal tools tests pass (mock action channel receives `SendAttachment`)
3. `cargo clippy --all-targets --all-features -- -D warnings` — clean
4. Manual: run with `COOP_TRACE_FILE=traces.jsonl`, have agent download a non-image file with `.jpg` extension → trace shows `warn` about magic mismatch, no 400 error
5. Manual: have agent use `signal_send_image` tool → trace shows `signal.action = "send_attachment"`, recipient receives image in Signal
6. Manual: verify no repeated "skipping image injection" spam for stale paths in traces
