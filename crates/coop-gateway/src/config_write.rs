use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config_check::{CheckReport, CheckResult, Severity, validate_config};

pub(crate) fn backup_config(path: &Path) -> Result<PathBuf> {
    let backup = path.with_extension("toml.bak");
    std::fs::copy(path, &backup)?;
    Ok(backup)
}

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn safe_write_config(
    config_path: &Path,
    new_content: &str,
) -> (CheckReport, Option<PathBuf>) {
    // 1. Write to staging file and validate
    let staging = config_path.with_extension("toml.staging");
    if let Err(e) = std::fs::write(&staging, new_content) {
        let mut report = CheckReport::default();
        report.push(CheckResult {
            name: "write_staging",
            severity: Severity::Error,
            passed: false,
            message: format!("failed to write staging file: {e}"),
        });
        return (report, None);
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let mut report = validate_config(&staging, config_dir);
    let _ = std::fs::remove_file(&staging);

    if report.has_errors() {
        return (report, None);
    }

    // 2. Backup current config (if it exists)
    let backup = if config_path.exists() {
        match backup_config(config_path) {
            Ok(p) => Some(p),
            Err(e) => {
                report.push(CheckResult {
                    name: "backup",
                    severity: Severity::Error,
                    passed: false,
                    message: format!("failed to backup config: {e}"),
                });
                return (report, None);
            }
        }
    } else {
        None
    };

    // 3. Write atomically
    if let Err(e) = atomic_write(config_path, new_content) {
        report.push(CheckResult {
            name: "atomic_write",
            severity: Severity::Error,
            passed: false,
            message: format!("failed to write config: {e}"),
        });
        return (report, backup);
    }

    (report, backup)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_test_config(dir: &Path) -> PathBuf {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "test").unwrap();

        let config_path = dir.join("coop.toml");
        std::fs::write(
            &config_path,
            format!(
                "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
                workspace.display()
            ),
        )
        .unwrap();
        config_path
    }

    /// The api_key_present check depends on the environment. When running
    /// `safe_write_config`, if ANTHROPIC_API_KEY is not set, the report will
    /// have an error and the write will be rejected. We test the write path
    /// by verifying the error comes only from api_key_present, not from other
    /// checks, so the test is still meaningful.
    fn only_env_errors(report: &CheckReport) -> bool {
        report
            .results
            .iter()
            .filter(|r| r.severity == Severity::Error && !r.passed)
            .all(|r| r.name == "api_key_present")
    }

    #[test]
    fn test_backup_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(&config_path, "original content").unwrap();

        let backup = backup_config(&config_path).unwrap();
        assert!(backup.exists());
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "original content"
        );
    }

    #[test]
    fn test_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("coop.toml");
        std::fs::write(&config_path, "old").unwrap();

        atomic_write(&config_path, "new content").unwrap();
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "new content"
        );
        assert!(!config_path.with_extension("toml.tmp").exists());
    }

    #[test]
    fn test_safe_write_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());

        let workspace = dir.path().join("workspace");
        let new_toml = format!(
            "[agent]\nid = \"updated\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"anthropic\"\n",
            workspace.display()
        );

        let (report, backup) = safe_write_config(&config_path, &new_toml);

        if report.has_errors() {
            // If ANTHROPIC_API_KEY is not set, the write is rejected.
            // Verify the only error is the env check.
            assert!(
                only_env_errors(&report),
                "unexpected errors: {:?}",
                report.results
            );
        } else {
            assert!(backup.is_some());
            assert!(
                std::fs::read_to_string(&config_path)
                    .unwrap()
                    .contains("updated")
            );
        }
    }

    #[test]
    fn test_safe_write_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();

        let (report, _backup) = safe_write_config(&config_path, "{{not valid toml");
        assert!(report.has_errors());

        let current = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(current, original);
    }

    #[test]
    fn test_safe_write_invalid_provider() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_test_config(dir.path());
        let original = std::fs::read_to_string(&config_path).unwrap();

        let workspace = dir.path().join("workspace");
        let bad_toml = format!(
            "[agent]\nid = \"test\"\nmodel = \"test-model\"\nworkspace = \"{}\"\n\n[provider]\nname = \"openai\"\n",
            workspace.display()
        );

        let (report, _backup) = safe_write_config(&config_path, &bad_toml);
        assert!(report.has_errors());

        let current = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(current, original);
    }
}
