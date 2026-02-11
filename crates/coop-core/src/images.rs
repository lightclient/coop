//! Auto-detect image file paths in message text and inject them as
//! `Content::Image` blocks so vision-capable models can see them.

use anyhow::{Result, bail};
use base64::Engine as _;
use std::collections::HashSet;
use std::path::Path;

use crate::{Content, Message};

/// Maximum image file size (5 MB — Anthropic's limit).
const MAX_IMAGE_SIZE: u64 = 5 * 1024 * 1024;

/// Image extensions we recognize (lowercase, without dot).
const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

/// Scan text for file paths ending in a recognized image extension.
///
/// Matches absolute, home-relative (`~/`), and relative (`./`) paths, as well
/// as paths wrapped in brackets like `[file saved: /path/to/image.jpg]`.
/// URLs (`http://`, `https://`) are excluded.
pub fn detect_image_paths(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();

    for path in extract_candidate_paths(text) {
        if seen.insert(path.clone()) {
            result.push(path);
        }
    }

    result
}

/// Read a file, base64-encode it, and return `(base64_data, mime_type)`.
pub fn load_image(path: &str) -> Result<(String, String)> {
    let expanded = expand_home(path);
    let p = Path::new(&expanded);

    let meta = std::fs::metadata(p)?;
    if meta.len() > MAX_IMAGE_SIZE {
        bail!(
            "image file exceeds 5 MB limit ({} bytes): {path}",
            meta.len()
        );
    }

    let bytes = std::fs::read(p)?;
    let mime = mime_from_extension(p);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

    Ok((b64, mime))
}

/// Scan all `Content::Text` and `Content::ToolResult` blocks in a message for
/// image paths and append `Content::Image` blocks for each found file.
///
/// Deduplicates by path — the same image is not injected twice even if the
/// path appears in multiple content blocks. Already-present `Content::Image`
/// blocks are counted toward deduplication (idempotency).
pub fn inject_images_into_message(message: &mut Message) {
    // Collect base64 data from already-present Image blocks for deduplication.
    let existing_data: HashSet<String> = message
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Image { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect();

    // Gather all text to scan, deduplicating by path.
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut candidate_paths = Vec::new();
    for block in &message.content {
        let text = match block {
            Content::Text { text } => text.as_str(),
            Content::ToolResult { output, .. } => output.as_str(),
            _ => continue,
        };
        for path in detect_image_paths(text) {
            if seen_paths.insert(path.clone()) {
                candidate_paths.push(path);
            }
        }
    }

    for path in candidate_paths {
        match load_image(&path) {
            Ok((data, mime_type)) => {
                if existing_data.contains(&data) {
                    continue;
                }
                tracing::debug!(path = %path, mime = %mime_type, "injecting image into message");
                message.content.push(Content::image(data, mime_type));
            }
            Err(e) => {
                tracing::debug!(path = %path, error = %e, "skipping image injection");
            }
        }
    }
}

/// Inject images into a cloned list of messages for provider calls.
///
/// Scans all user and tool-result messages for image paths and injects
/// `Content::Image` blocks where missing. Returns the modified messages.
/// The original session messages are not mutated.
pub fn inject_images_for_provider(messages: &[Message]) -> Vec<Message> {
    let mut cloned: Vec<Message> = messages.to_vec();
    for msg in &mut cloned {
        if matches!(msg.role, crate::Role::User) {
            inject_images_into_message(msg);
        }
    }
    cloned
}

// ---- internal helpers -----------------------------------------------------

fn extract_candidate_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();

    for line in text.lines() {
        // Bracket-wrapped: [file saved: /path/to/image.jpg]
        // or [/path/to/image.jpg] etc.
        let mut rest = line;
        while let Some(start) = rest.find('[') {
            if let Some(end) = rest[start..].find(']') {
                let inside = &rest[start + 1..start + end];
                for word in inside.split_whitespace() {
                    if let Some(p) = try_image_path(word) {
                        paths.push(p);
                    }
                }
                rest = &rest[start + end + 1..];
            } else {
                break;
            }
        }

        // Also scan bare words outside brackets.
        for word in line.split_whitespace() {
            // Strip surrounding brackets/parens that might remain
            let word = word.trim_matches(&['[', ']', '(', ')', '<', '>'] as &[char]);
            if let Some(p) = try_image_path(word) {
                paths.push(p);
            }
        }
    }

    paths
}

/// Returns `Some(path)` if `word` looks like a local image file path.
fn try_image_path(word: &str) -> Option<String> {
    // Reject URLs
    if word.starts_with("http://") || word.starts_with("https://") {
        return None;
    }

    // Must look like a path
    if !(word.starts_with('/')
        || word.starts_with("~/")
        || word.starts_with("./")
        || word.starts_with("../"))
    {
        return None;
    }

    // Must end with a recognized image extension
    let lower = word.to_lowercase();
    let has_ext = IMAGE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")));
    if !has_ext {
        return None;
    }

    Some(word.to_owned())
}

fn mime_from_extension(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg".to_owned(),
        "png" => "image/png".to_owned(),
        "gif" => "image/gif".to_owned(),
        "webp" => "image/webp".to_owned(),
        _ => "application/octet-stream".to_owned(),
    }
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    path.to_owned()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // ---- detect_image_paths ----

    #[test]
    fn detects_absolute_paths() {
        let paths = detect_image_paths("Look at /tmp/photo.jpg please");
        assert_eq!(paths, vec!["/tmp/photo.jpg"]);
    }

    #[test]
    fn detects_multiple_extensions() {
        let text = "/a/b.png /c/d.jpeg /e/f.gif /g/h.webp /i/j.jpg";
        let paths = detect_image_paths(text);
        assert_eq!(paths.len(), 5);
    }

    #[test]
    fn detects_bracket_wrapped_paths() {
        let text = "[file saved: /workspace/attachments/1234_photo.jpg]";
        let paths = detect_image_paths(text);
        assert!(paths.contains(&"/workspace/attachments/1234_photo.jpg".to_owned()));
    }

    #[test]
    fn detects_home_relative() {
        let paths = detect_image_paths("~/images/photo.png");
        assert_eq!(paths, vec!["~/images/photo.png"]);
    }

    #[test]
    fn detects_relative_dot() {
        let paths = detect_image_paths("see ./screenshot.png");
        assert_eq!(paths, vec!["./screenshot.png"]);
    }

    #[test]
    fn detects_parent_relative() {
        let paths = detect_image_paths("check ../output/result.webp");
        assert_eq!(paths, vec!["../output/result.webp"]);
    }

    #[test]
    fn ignores_urls() {
        let paths = detect_image_paths("https://example.com/photo.jpg");
        assert!(paths.is_empty());
    }

    #[test]
    fn ignores_http_urls() {
        let paths = detect_image_paths("http://example.com/photo.png");
        assert!(paths.is_empty());
    }

    #[test]
    fn ignores_non_image_extensions() {
        let paths = detect_image_paths("/tmp/file.txt /tmp/data.json ./code.rs");
        assert!(paths.is_empty());
    }

    #[test]
    fn deduplicates_paths() {
        let text = "/tmp/photo.jpg and again /tmp/photo.jpg";
        let paths = detect_image_paths(text);
        assert_eq!(paths, vec!["/tmp/photo.jpg"]);
    }

    // ---- load_image ----

    #[test]
    fn loads_real_file() {
        let mut f = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        f.write_all(b"fake png data").unwrap();
        f.flush().unwrap();

        let (b64, mime) = load_image(f.path().to_str().unwrap()).unwrap();
        assert_eq!(mime, "image/png");

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .unwrap();
        assert_eq!(decoded, b"fake png data");
    }

    #[test]
    fn returns_correct_mime_for_jpg() {
        let mut f = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        f.write_all(b"data").unwrap();
        f.flush().unwrap();

        let (_, mime) = load_image(f.path().to_str().unwrap()).unwrap();
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn returns_correct_mime_for_webp() {
        let mut f = tempfile::NamedTempFile::with_suffix(".webp").unwrap();
        f.write_all(b"data").unwrap();
        f.flush().unwrap();

        let (_, mime) = load_image(f.path().to_str().unwrap()).unwrap();
        assert_eq!(mime, "image/webp");
    }

    #[test]
    fn error_on_missing_file() {
        assert!(load_image("/nonexistent/path/photo.png").is_err());
    }

    #[test]
    fn error_on_oversized_file() {
        let mut f = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        // Write just over 5 MB
        #[allow(clippy::cast_possible_truncation)]
        let buf = vec![0u8; (MAX_IMAGE_SIZE + 1) as usize];
        f.write_all(&buf).unwrap();
        f.flush().unwrap();

        let err = load_image(f.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("5 MB"));
    }

    // ---- inject_images_into_message ----

    #[test]
    fn injects_image_from_text_block() {
        let mut f = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        f.write_all(b"img").unwrap();
        f.flush().unwrap();

        let path = f.path().to_str().unwrap();
        let mut msg = Message::user().with_text(format!("Look at {path}"));

        inject_images_into_message(&mut msg);

        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[0], Content::Text { .. }));
        assert!(matches!(
            &msg.content[1],
            Content::Image { mime_type, .. } if mime_type == "image/png"
        ));
    }

    #[test]
    fn preserves_original_text() {
        let mut f = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        f.write_all(b"img").unwrap();
        f.flush().unwrap();

        let path = f.path().to_str().unwrap();
        let original_text = format!("Check {path}");
        let mut msg = Message::user().with_text(original_text.clone());

        inject_images_into_message(&mut msg);

        assert_eq!(msg.content[0].as_text().unwrap(), original_text);
    }

    #[test]
    fn idempotent_injection() {
        let mut f = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        f.write_all(b"img").unwrap();
        f.flush().unwrap();

        let path = f.path().to_str().unwrap();
        let mut msg = Message::user().with_text(format!("Look at {path}"));

        inject_images_into_message(&mut msg);
        let count_after_first = msg.content.len();

        inject_images_into_message(&mut msg);
        assert_eq!(msg.content.len(), count_after_first);
    }

    #[test]
    fn injects_from_tool_result() {
        let mut f = tempfile::NamedTempFile::with_suffix(".jpg").unwrap();
        f.write_all(b"img").unwrap();
        f.flush().unwrap();

        let path = f.path().to_str().unwrap();
        let mut msg =
            Message::user().with_tool_result("t1", format!("[file saved: {path}]"), false);

        inject_images_into_message(&mut msg);

        assert_eq!(msg.content.len(), 2);
        assert!(matches!(&msg.content[1], Content::Image { .. }));
    }

    #[test]
    fn skips_missing_files_gracefully() {
        let mut msg = Message::user().with_text("/nonexistent/photo.png");
        inject_images_into_message(&mut msg);
        assert_eq!(msg.content.len(), 1); // only the text block
    }
}
