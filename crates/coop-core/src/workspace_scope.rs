use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use tracing::{debug, warn};

use crate::{SessionKind, TrustLevel};

const DENIED_SCOPE_DIR: &str = ".scope-denied";
const MAX_DIR_SLUG_LEN: usize = 48;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspacePrincipal {
    Global,
    User { name: String, dir_name: String },
    Group { id: String, dir_name: String },
    UnmappedUser,
}

#[derive(Debug, Clone)]
pub struct WorkspaceScope {
    workspace_root: PathBuf,
    scope_root: Option<PathBuf>,
    scope_relative_root: Option<PathBuf>,
    session_kind: SessionKind,
    trust: TrustLevel,
    user_name: Option<String>,
    principal: WorkspacePrincipal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOperation {
    Read,
    Write,
}

impl PathOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

impl WorkspaceScope {
    pub fn for_turn(
        workspace_root: impl AsRef<Path>,
        session_kind: &SessionKind,
        trust: TrustLevel,
        user_name: Option<&str>,
    ) -> Self {
        let workspace_root = canonical_workspace_root(workspace_root.as_ref());
        let user_name = user_name.map(str::to_owned);

        let (principal, scope_relative_root) = match session_kind {
            SessionKind::Group(group_id) => {
                let dir_name = group_workspace_dir_name(group_id);
                (
                    WorkspacePrincipal::Group {
                        id: group_id.clone(),
                        dir_name: dir_name.clone(),
                    },
                    Some(PathBuf::from("groups").join(dir_name)),
                )
            }
            SessionKind::Main
            | SessionKind::Dm(_)
            | SessionKind::Isolated(_)
            | SessionKind::Cron(_)
                if trust <= TrustLevel::Full =>
            {
                (WorkspacePrincipal::Global, Some(PathBuf::new()))
            }
            SessionKind::Main
            | SessionKind::Dm(_)
            | SessionKind::Isolated(_)
            | SessionKind::Cron(_) => match user_name.as_deref() {
                Some(user) => {
                    let dir_name = user_workspace_dir_name(user);
                    (
                        WorkspacePrincipal::User {
                            name: user.to_owned(),
                            dir_name: dir_name.clone(),
                        },
                        Some(PathBuf::from("users").join(dir_name)),
                    )
                }
                None => (WorkspacePrincipal::UnmappedUser, None),
            },
        };

        let scope_root = scope_relative_root
            .as_ref()
            .map(|relative| workspace_root.join(relative));

        let scope = Self {
            workspace_root,
            scope_root,
            scope_relative_root,
            session_kind: session_kind.clone(),
            trust,
            user_name,
            principal,
        };

        debug!(
            session_kind = ?scope.session_kind,
            trust = ?scope.trust,
            user = ?scope.user_name,
            principal = ?scope.principal,
            workspace_root = %scope.workspace_root.display(),
            scoped_root = %scope.scope_display(),
            "resolved workspace scope"
        );

        scope
    }

    pub fn for_user_principal(workspace_root: impl AsRef<Path>, user_name: &str) -> Self {
        let workspace_root = canonical_workspace_root(workspace_root.as_ref());
        let dir_name = user_workspace_dir_name(user_name);
        let relative = PathBuf::from("users").join(&dir_name);

        let scope = Self {
            workspace_root: workspace_root.clone(),
            scope_root: Some(workspace_root.join(&relative)),
            scope_relative_root: Some(relative),
            session_kind: SessionKind::Dm(format!("principal:{user_name}")),
            trust: TrustLevel::Inner,
            user_name: Some(user_name.to_owned()),
            principal: WorkspacePrincipal::User {
                name: user_name.to_owned(),
                dir_name,
            },
        };

        debug!(
            session_kind = ?scope.session_kind,
            trust = ?scope.trust,
            user = ?scope.user_name,
            principal = ?scope.principal,
            workspace_root = %scope.workspace_root.display(),
            scoped_root = %scope.scope_display(),
            "resolved principal workspace scope"
        );

        scope
    }

    pub fn for_group_principal(workspace_root: impl AsRef<Path>, group_id: &str) -> Self {
        let workspace_root = canonical_workspace_root(workspace_root.as_ref());
        let dir_name = group_workspace_dir_name(group_id);
        let relative = PathBuf::from("groups").join(&dir_name);

        let scope = Self {
            workspace_root: workspace_root.clone(),
            scope_root: Some(workspace_root.join(&relative)),
            scope_relative_root: Some(relative),
            session_kind: SessionKind::Group(group_id.to_owned()),
            trust: TrustLevel::Familiar,
            user_name: None,
            principal: WorkspacePrincipal::Group {
                id: group_id.to_owned(),
                dir_name,
            },
        };

        debug!(
            session_kind = ?scope.session_kind,
            trust = ?scope.trust,
            principal = ?scope.principal,
            workspace_root = %scope.workspace_root.display(),
            scoped_root = %scope.scope_display(),
            "resolved principal workspace scope"
        );

        scope
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn scope_root(&self) -> Result<&Path> {
        self.scope_root.as_deref().ok_or_else(|| {
            denied_scope_error(
                self,
                None,
                PathOperation::Read,
                "path-based access is disabled because this turn has no mapped user workspace",
            )
        })
    }

    pub fn tool_workspace_root(&self) -> PathBuf {
        self.scope_root
            .clone()
            .unwrap_or_else(|| self.workspace_root.join(DENIED_SCOPE_DIR))
    }

    pub fn scope_relative_root(&self) -> Option<&Path> {
        self.scope_relative_root.as_deref()
    }

    pub fn scope_display(&self) -> String {
        match self.scope_relative_root() {
            Some(path) if path.as_os_str().is_empty() => "./".to_owned(),
            Some(path) => format!("{}/", path.display()),
            None => "<unavailable>".to_owned(),
        }
    }

    pub fn principal(&self) -> &WorkspacePrincipal {
        &self.principal
    }

    pub fn is_group_scope(&self) -> bool {
        matches!(self.principal, WorkspacePrincipal::Group { .. })
    }

    pub fn ensure_scope_root_exists(&self) -> Result<()> {
        let Some(scope_root) = &self.scope_root else {
            return Ok(());
        };
        std::fs::create_dir_all(scope_root)?;
        Ok(())
    }

    pub fn attachments_dir(&self) -> Result<PathBuf> {
        Ok(self.scope_root()?.join("attachments"))
    }

    pub fn resolve_user_path_for_read(&self, path: &str) -> Result<PathBuf> {
        self.resolve_user_path(path, PathOperation::Read)
    }

    pub fn resolve_user_path_for_write(&self, path: &str) -> Result<PathBuf> {
        self.resolve_user_path(path, PathOperation::Write)
    }

    pub fn resolve_host_path_for_read(&self, path: &str) -> Result<PathBuf> {
        self.resolve_host_path(path, PathOperation::Read)
    }

    pub fn resolve_host_path_for_write(&self, path: &str) -> Result<PathBuf> {
        self.resolve_host_path(path, PathOperation::Write)
    }

    pub fn contains_host_path(&self, path: &Path) -> bool {
        if let Some(request) = path.to_str() {
            self.resolve_host_path_for_read(request).is_ok()
        } else {
            false
        }
    }

    pub fn scope_relative_path(&self, host_path: &Path) -> Result<String> {
        let scope_root = self.scope_root()?;
        let relative = host_path.strip_prefix(scope_root).map_err(|_strip_error| {
            denied_scope_error(
                self,
                host_path.to_str(),
                PathOperation::Read,
                "path is outside the current workspace scope",
            )
        })?;

        if relative.as_os_str().is_empty() {
            return Ok("./".to_owned());
        }

        Ok(format!("./{}", relative.display()))
    }

    fn resolve_user_path(&self, path: &str, operation: PathOperation) -> Result<PathBuf> {
        let scope_root = self.scope_root()?;
        let user_path = Path::new(path);

        if user_path.is_absolute() {
            return Err(denied_scope_error(
                self,
                Some(path),
                operation,
                "absolute paths are not allowed; use paths relative to the current workspace scope",
            ));
        }

        validate_relative_components(self, user_path, Some(path), operation)?;

        let resolved = scope_root.join(user_path);
        match operation {
            PathOperation::Read => ensure_read_within_scope(self, &resolved, Some(path), false),
            PathOperation::Write => ensure_write_within_scope(self, &resolved, Some(path), false),
        }
    }

    fn resolve_host_path(&self, path: &str, operation: PathOperation) -> Result<PathBuf> {
        let expanded = expand_home(path);
        let host_path = Path::new(&expanded);

        if host_path.is_absolute() {
            if !self.allows_absolute_host_paths() {
                return Err(denied_scope_error(
                    self,
                    Some(path),
                    operation,
                    "absolute host paths are not allowed in the current workspace scope",
                ));
            }

            return match operation {
                PathOperation::Read => ensure_read_within_scope(self, host_path, Some(path), true),
                PathOperation::Write => {
                    ensure_write_within_scope(self, host_path, Some(path), true)
                }
            };
        }

        validate_relative_components(self, host_path, Some(path), operation)?;

        let resolved = self.scope_root()?.join(host_path);
        match operation {
            PathOperation::Read => ensure_read_within_scope(self, &resolved, Some(path), false),
            PathOperation::Write => ensure_write_within_scope(self, &resolved, Some(path), false),
        }
    }

    fn allows_absolute_host_paths(&self) -> bool {
        matches!(self.principal, WorkspacePrincipal::Global)
    }
}

pub fn user_workspace_dir_name(user_name: &str) -> String {
    sanitized_dir_name(user_name)
}

pub fn group_workspace_dir_name(group_id: &str) -> String {
    sanitized_dir_name(group_id)
}

fn canonical_workspace_root(workspace_root: &Path) -> PathBuf {
    workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf())
}

fn sanitized_dir_name(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_separator = false;

    for ch in value.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            slug.push(ch);
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }

    let mut slug = slug
        .trim_matches(|ch: char| matches!(ch, '.' | '-' | '_'))
        .to_owned();

    let mut changed = slug != value;
    if slug.is_empty() {
        slug.clear();
        slug.push_str("principal");
        changed = true;
    }
    if slug.len() > MAX_DIR_SLUG_LEN {
        slug.truncate(MAX_DIR_SLUG_LEN);
        slug = slug
            .trim_matches(|ch: char| matches!(ch, '.' | '-' | '_'))
            .to_owned();
        if slug.is_empty() {
            slug.clear();
            slug.push_str("principal");
        }
        changed = true;
    }

    if !changed {
        return slug;
    }

    format!("{slug}-{}", short_hash(value))
}

fn short_hash(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    let bytes = hasher.finish().to_le_bytes();
    let short = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    format!("{short:08x}")
}

fn validate_relative_components(
    scope: &WorkspaceScope,
    path: &Path,
    requested: Option<&str>,
    operation: PathOperation,
) -> Result<()> {
    for component in path.components() {
        match component {
            Component::CurDir | Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(denied_scope_error(
                    scope,
                    requested,
                    operation,
                    "path traversal outside the current workspace scope is not allowed",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(denied_scope_error(
                    scope,
                    requested,
                    operation,
                    "absolute paths are not allowed; use paths relative to the current workspace scope",
                ));
            }
        }
    }

    Ok(())
}

fn ensure_read_within_scope(
    scope: &WorkspaceScope,
    path: &Path,
    requested: Option<&str>,
    allow_absolute_request: bool,
) -> Result<PathBuf> {
    let canon_target = path
        .canonicalize()
        .map_err(|error| access_io_error(scope, requested, PathOperation::Read, &error))?;
    let scope_root = scope.scope_root()?;

    if !canon_target.starts_with(scope_root) {
        return Err(denied_scope_error(
            scope,
            requested,
            PathOperation::Read,
            "path is outside the current workspace scope",
        ));
    }

    let _ = allow_absolute_request;

    Ok(canon_target)
}

fn ensure_write_within_scope(
    scope: &WorkspaceScope,
    path: &Path,
    requested: Option<&str>,
    absolute_request: bool,
) -> Result<PathBuf> {
    let scope_root = scope.scope_root()?;
    let existing = nearest_existing_ancestor(path).unwrap_or_else(|| scope.workspace_root.clone());
    let canon_existing = existing
        .canonicalize()
        .map_err(|error| access_io_error(scope, requested, PathOperation::Write, &error))?;

    if !canon_existing.starts_with(scope_root) && !scope_root.starts_with(&canon_existing) {
        return Err(denied_scope_error(
            scope,
            requested,
            PathOperation::Write,
            "path is outside the current workspace scope",
        ));
    }

    if absolute_request && !scope.allows_absolute_host_paths() {
        return Err(denied_scope_error(
            scope,
            requested,
            PathOperation::Write,
            "absolute host paths are not allowed in the current workspace scope",
        ));
    }

    Ok(path.to_path_buf())
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn access_io_error(
    scope: &WorkspaceScope,
    requested: Option<&str>,
    operation: PathOperation,
    error: &std::io::Error,
) -> anyhow::Error {
    let context = format!(
        "failed to {} path inside the current workspace scope: {error}",
        operation.as_str()
    );
    warn!(
        session_kind = ?scope.session_kind,
        trust = ?scope.trust,
        user = ?scope.user_name,
        principal = ?scope.principal,
        workspace_root = %scope.workspace_root.display(),
        scoped_root = %scope.scope_display(),
        attempted_path = requested.unwrap_or("<unknown>"),
        operation = operation.as_str(),
        error = %error,
        "workspace scope access failed"
    );
    anyhow::anyhow!(context)
}

fn denied_scope_error(
    scope: &WorkspaceScope,
    requested: Option<&str>,
    operation: PathOperation,
    reason: &str,
) -> anyhow::Error {
    warn!(
        session_kind = ?scope.session_kind,
        trust = ?scope.trust,
        user = ?scope.user_name,
        principal = ?scope.principal,
        workspace_root = %scope.workspace_root.display(),
        scoped_root = %scope.scope_display(),
        attempted_path = requested.unwrap_or("<none>"),
        operation = operation.as_str(),
        reason,
        "workspace scope denied access"
    );
    anyhow::anyhow!(reason.to_owned())
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
    use std::fs;

    fn relative_path(scope: &WorkspaceScope) -> Option<String> {
        scope
            .scope_relative_root()
            .map(|path| path.display().to_string())
    }

    #[test]
    fn global_scope_for_full_dm() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:alice-uuid".to_owned()),
            TrustLevel::Full,
            Some("alice"),
        );

        assert_eq!(scope.principal(), &WorkspacePrincipal::Global);
        assert_eq!(relative_path(&scope).as_deref(), Some(""));
        assert_eq!(scope.scope_display(), "./");
    }

    #[test]
    fn low_trust_dm_uses_user_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            Some("bob"),
        );

        assert_eq!(relative_path(&scope).as_deref(), Some("users/bob"));
        assert_eq!(scope.scope_display(), "users/bob/");
    }

    #[test]
    fn group_scope_overrides_sender_trust() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Group("signal:group:deadbeef".to_owned()),
            TrustLevel::Owner,
            Some("alice"),
        );

        let relative = relative_path(&scope).unwrap();
        assert!(relative.starts_with("groups/"));
        assert!(scope.is_group_scope());
        assert_ne!(scope.principal(), &WorkspacePrincipal::Global);
    }

    #[test]
    fn low_trust_without_user_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let scope =
            WorkspaceScope::for_turn(dir.path(), &SessionKind::Main, TrustLevel::Inner, None);

        assert!(scope.scope_root().is_err());
        assert_eq!(scope.scope_display(), "<unavailable>");
    }

    #[test]
    fn sanitized_group_dir_name_is_stable_and_safe() {
        let group = "signal:group:00112233445566778899aabbccddeeff";
        let first = group_workspace_dir_name(group);
        let second = group_workspace_dir_name(group);

        assert_eq!(first, second);
        assert!(first.chars().all(|ch| ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || matches!(ch, '.' | '-' | '_')));
        assert!(first.starts_with("signal-group-00112233445566778899aabbccddeeff"));
        assert!(first.len() <= MAX_DIR_SLUG_LEN + 1 + 8);
    }

    #[test]
    fn resolve_user_write_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            Some("bob"),
        );

        let error = scope
            .resolve_user_path_for_write("../alice/secret.txt")
            .unwrap_err();
        assert!(error.to_string().contains("path traversal"));
    }

    #[test]
    fn resolve_host_path_for_read_allows_in_scope_absolute_for_global() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("photo.png");
        fs::write(&file, b"test").unwrap();

        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Main,
            TrustLevel::Full,
            Some("alice"),
        );

        let resolved = scope
            .resolve_host_path_for_read(file.to_str().unwrap())
            .unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_host_path_for_read_rejects_absolute_for_user_scope() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("users/bob/photo.png");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"test").unwrap();

        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            Some("bob"),
        );

        let error = scope
            .resolve_host_path_for_read(file.to_str().unwrap())
            .unwrap_err();
        assert!(error.to_string().contains("absolute host paths"));
    }

    #[test]
    fn contains_host_path_rejects_out_of_scope_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            Some("bob"),
        );

        assert!(!scope.contains_host_path(outside.path()));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("users/bob")).unwrap();
        fs::write(outside_dir.path().join("secret.txt"), "secret").unwrap();
        symlink(outside_dir.path(), dir.path().join("users/bob/link")).unwrap();

        let scope = WorkspaceScope::for_turn(
            dir.path(),
            &SessionKind::Dm("signal:bob-uuid".to_owned()),
            TrustLevel::Inner,
            Some("bob"),
        );

        let error = scope
            .resolve_user_path_for_read("link/secret.txt")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the current workspace scope")
        );
    }

    #[test]
    fn scope_relative_path_formats_attachment_reference() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_user_principal(dir.path(), "alice");
        let path = scope.attachments_dir().unwrap().join("photo.png");

        assert_eq!(
            scope.scope_relative_path(&path).unwrap(),
            "./attachments/photo.png"
        );
    }

    #[test]
    fn write_allows_missing_scope_root() {
        let dir = tempfile::tempdir().unwrap();
        let scope = WorkspaceScope::for_user_principal(dir.path(), "bob");
        let resolved = scope.resolve_user_path_for_write("notes/todo.txt").unwrap();

        assert_eq!(resolved, dir.path().join("users/bob/notes/todo.txt"));
    }
}
