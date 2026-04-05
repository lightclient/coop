use anyhow::Result;
use base64::Engine as _;
use std::path::{Path, PathBuf};

use crate::WorkspaceScope;

pub fn save_base64_image(
    scope: &WorkspaceScope,
    output_dir: &str,
    file_stem: &str,
    index: usize,
    data: &str,
    mime_type: &str,
) -> Result<String> {
    let host_dir = resolve_output_dir(scope, output_dir)?;
    std::fs::create_dir_all(&host_dir)?;

    let extension = extension_for_mime(mime_type);
    let file_name = if index == 0 {
        format!("{file_stem}.{extension}")
    } else {
        format!("{file_stem}-{:03}.{extension}", index + 1)
    };
    let host_path = unique_file_path(host_dir.join(file_name));
    let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
    std::fs::write(&host_path, bytes)?;

    scope.scope_relative_path(&host_path).or_else(|_| {
        host_path
            .strip_prefix(scope.workspace_root())
            .map(|path| format!("./{}", path.display()))
            .map_err(anyhow::Error::from)
    })
}

pub fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        _ => "bin",
    }
}

fn resolve_output_dir(scope: &WorkspaceScope, output_dir: &str) -> Result<PathBuf> {
    scope
        .resolve_user_path_for_write(output_dir)
        .or_else(|_| scope.resolve_host_path_for_write(output_dir))
}

fn unique_file_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }

    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("image");
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("bin");
    let parent = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    for index in 1..10_000 {
        let candidate = parent.join(format!("{stem}-{index:03}.{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    path
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionKind, TrustLevel};

    #[test]
    fn saves_image_under_scope() {
        let dir = tempfile::tempdir().unwrap();
        let scope =
            WorkspaceScope::for_turn(dir.path(), &SessionKind::Main, TrustLevel::Full, None);
        let path = save_base64_image(
            &scope,
            "generated/images",
            "result",
            0,
            &base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"abc"),
            "image/png",
        )
        .unwrap();
        assert!(path.starts_with("./generated/images/result"));
        assert!(dir.path().join("generated/images/result.png").exists());
    }
}
