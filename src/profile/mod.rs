//! Profile resolution for convention-based override and secrets file loading.
//!
//! Resolves `--profile <name>` to `lecs-override.<name>.json` and `secrets.<name>.json`
//! paths, with optional `.lecs.toml` configuration for default profiles.

use std::path::{Path, PathBuf};

/// Errors that can occur during profile configuration loading.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// Failed to read the configuration file from disk.
    #[error("failed to read config file {path}: {source}")]
    ReadConfig {
        /// Path to the configuration file.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to parse the TOML configuration file.
    #[error("failed to parse config file {path}: {source}")]
    ParseConfig {
        /// Path to the configuration file.
        path: PathBuf,
        /// Underlying TOML parse error.
        source: toml::de::Error,
    },

    /// Profile name contains invalid characters.
    #[error("invalid profile name '{name}': must match [A-Za-z0-9_-]+")]
    InvalidProfileName {
        /// The invalid profile name.
        name: String,
    },
}

/// Parsed `.lecs.toml` configuration.
#[derive(Debug, Default, serde::Deserialize)]
pub struct LecsConfig {
    /// Default profile name (e.g., `"dev"`).
    pub default_profile: Option<String>,
}

/// Resolved file paths after profile resolution.
///
/// `override_path` and `secrets_path` are `Some` only when the file exists
/// (for convention-based paths) or when explicitly specified via CLI flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    /// Resolved override file path.
    pub override_path: Option<PathBuf>,
    /// Resolved secrets file path.
    pub secrets_path: Option<PathBuf>,
}

impl LecsConfig {
    /// Load configuration from a file path.
    ///
    /// # Errors
    ///
    /// Returns `ProfileError::ReadConfig` if the file cannot be read,
    /// or `ProfileError::ParseConfig` if the TOML is invalid.
    pub fn from_file(path: &Path) -> Result<Self, ProfileError> {
        let content = std::fs::read_to_string(path).map_err(|source| ProfileError::ReadConfig {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&content, path)
    }

    /// Parse configuration from a TOML string.
    ///
    /// # Errors
    ///
    /// Returns `ProfileError::ParseConfig` if the TOML is invalid.
    pub fn from_toml(toml_str: &str, source_path: &Path) -> Result<Self, ProfileError> {
        toml::from_str(toml_str).map_err(|source| ProfileError::ParseConfig {
            path: source_path.to_path_buf(),
            source,
        })
    }
}

/// Validate that a profile name contains only safe characters (`[A-Za-z0-9_-]+`).
///
/// Rejects names containing path separators, `..`, or other unsafe characters
/// to prevent path traversal attacks.
///
/// # Errors
///
/// Returns `ProfileError::InvalidProfileName` if the name is empty or contains
/// characters outside `[A-Za-z0-9_-]`.
pub fn validate_profile_name(name: &str) -> Result<(), ProfileError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ProfileError::InvalidProfileName {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// Search for `.lecs.toml` starting from `start_dir` and walking up parent directories.
///
/// Returns the path to the first `.lecs.toml` found, or `None` if not found.
#[must_use]
pub fn find_config(start_dir: &Path) -> Option<PathBuf> {
    let mut current = if start_dir.as_os_str().is_empty() {
        Path::new(".").to_path_buf()
    } else {
        start_dir.to_path_buf()
    };

    loop {
        let candidate = current.join(".lecs.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Build the conventional override file path for a profile.
///
/// Returns `base_dir/lecs-override.<profile>.json`.
#[must_use]
pub fn profile_override_path(base_dir: &Path, profile: &str) -> PathBuf {
    base_dir.join(format!("lecs-override.{profile}.json"))
}

/// Build the conventional secrets file path for a profile.
///
/// Returns `base_dir/secrets.<profile>.json`.
#[must_use]
pub fn profile_secrets_path(base_dir: &Path, profile: &str) -> PathBuf {
    base_dir.join(format!("secrets.{profile}.json"))
}

/// Resolve final override and secrets file paths given CLI args and profile name.
///
/// Priority (per axis):
/// 1. Explicit CLI flags (`--override`, `--secrets`) — highest
/// 2. Profile-derived convention paths (only if file exists)
/// 3. `None`
///
/// When a profile is specified but the convention file does not exist,
/// that axis is silently skipped (returns `None`).
///
/// # Errors
///
/// Returns `ProfileError::InvalidProfileName` if the profile name contains
/// unsafe characters (path separators, `..`, etc.).
pub fn resolve(
    base_dir: &Path,
    profile: Option<&str>,
    explicit_override: Option<&Path>,
    explicit_secrets: Option<&Path>,
) -> Result<ResolvedPaths, ProfileError> {
    if let Some(prof) = profile {
        validate_profile_name(prof)?;
    }

    let base = if base_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base_dir
    };

    let override_path = explicit_override.map_or_else(
        || {
            profile.and_then(|prof| {
                let path = profile_override_path(base, prof);
                path.is_file().then_some(path)
            })
        },
        |p| Some(p.to_path_buf()),
    );

    let secrets_path = explicit_secrets.map_or_else(
        || {
            profile.and_then(|prof| {
                let path = profile_secrets_path(base, prof);
                path.is_file().then_some(path)
            })
        },
        |p| Some(p.to_path_buf()),
    );

    Ok(ResolvedPaths {
        override_path,
        secrets_path,
    })
}

/// Convenience wrapper that combines config loading, profile resolution, and path resolution.
///
/// This encapsulates the common pattern used by CLI commands that accept
/// `--profile`, `--override`, and `--secrets` flags alongside a task definition path.
///
/// # Errors
///
/// Returns `ProfileError::InvalidProfileName` if the resolved profile name is invalid.
pub fn resolve_from_args(
    task_definition: &Path,
    profile: Option<&str>,
    explicit_override: Option<&Path>,
    explicit_secrets: Option<&Path>,
) -> Result<ResolvedPaths, ProfileError> {
    let base_dir = task_definition.parent().unwrap_or_else(|| Path::new("."));
    let config = load_config_with_warning(base_dir);
    let effective = effective_profile(profile, config.as_ref());
    resolve(base_dir, effective, explicit_override, explicit_secrets)
}

/// Load `.lecs.toml` config from `base_dir` (searching upward), logging a warning on errors.
///
/// Returns `None` if no config file is found or if loading/parsing fails.
#[must_use]
pub fn load_config_with_warning(base_dir: &Path) -> Option<LecsConfig> {
    let config_path = find_config(base_dir)?;
    match LecsConfig::from_file(&config_path) {
        Ok(config) => Some(config),
        Err(err) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %err,
                "Failed to load .lecs.toml; ignoring"
            );
            None
        }
    }
}

/// Determine the effective profile name from CLI arg and `.lecs.toml` default.
///
/// Priority: explicit CLI `--profile` > `.lecs.toml` `default_profile` > `None`.
#[must_use]
pub fn effective_profile<'a>(
    cli_profile: Option<&'a str>,
    config: Option<&'a LecsConfig>,
) -> Option<&'a str> {
    cli_profile.or_else(|| config.as_ref().and_then(|c| c.default_profile.as_deref()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── LecsConfig parsing tests ──

    #[test]
    fn parse_config_with_defaults() {
        let toml_str = r#"default_profile = "dev""#;
        let config = LecsConfig::from_toml(toml_str, Path::new("test.toml")).unwrap();
        assert_eq!(config.default_profile.as_deref(), Some("dev"));
    }

    #[test]
    fn parse_config_empty() {
        let config = LecsConfig::from_toml("", Path::new("test.toml")).unwrap();
        assert!(config.default_profile.is_none());
    }

    #[test]
    fn parse_config_invalid_toml() {
        let result = LecsConfig::from_toml("not valid toml [[[", Path::new("test.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("test.toml"));
    }

    #[test]
    fn parse_config_ignores_unknown_fields() {
        let toml_str = r#"
            default_profile = "staging"
            unknown_field = "value"
        "#;
        // serde default behavior ignores unknown fields
        let config = LecsConfig::from_toml(toml_str, Path::new("test.toml")).unwrap();
        assert_eq!(config.default_profile.as_deref(), Some("staging"));
    }

    // ── LecsConfig::from_file tests ──

    #[test]
    fn parse_config_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".lecs.toml");
        std::fs::write(&config_path, r#"default_profile = "prod""#).unwrap();

        let config = LecsConfig::from_file(&config_path).unwrap();
        assert_eq!(config.default_profile.as_deref(), Some("prod"));
    }

    #[test]
    fn parse_config_file_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join(".lecs.toml");

        let result = LecsConfig::from_file(&missing_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let missing_path_str = missing_path.to_string_lossy();
        assert!(err.to_string().contains(missing_path_str.as_ref()));
    }

    // ── Error display tests ──

    #[test]
    fn error_display_read_config() {
        let err = ProfileError::ReadConfig {
            path: PathBuf::from("/foo/.lecs.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/foo/.lecs.toml"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn error_display_parse_config() {
        let toml_err = toml::from_str::<LecsConfig>("invalid [[[").unwrap_err();
        let err = ProfileError::ParseConfig {
            path: PathBuf::from("/bar/.lecs.toml"),
            source: toml_err,
        };
        let msg = err.to_string();
        assert!(msg.contains("/bar/.lecs.toml"));
    }

    // ── Convention path builder tests ──

    #[test]
    fn profile_override_path_dev() {
        let path = profile_override_path(Path::new("/project"), "dev");
        assert_eq!(path, PathBuf::from("/project/lecs-override.dev.json"));
    }

    #[test]
    fn profile_override_path_staging() {
        let path = profile_override_path(Path::new("/project"), "staging");
        assert_eq!(path, PathBuf::from("/project/lecs-override.staging.json"));
    }

    #[test]
    fn profile_secrets_path_dev() {
        let path = profile_secrets_path(Path::new("/project"), "dev");
        assert_eq!(path, PathBuf::from("/project/secrets.dev.json"));
    }

    #[test]
    fn profile_secrets_path_prod() {
        let path = profile_secrets_path(Path::new("/project"), "prod");
        assert_eq!(path, PathBuf::from("/project/secrets.prod.json"));
    }

    // ── find_config tests ──

    #[test]
    fn find_config_in_current_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".lecs.toml");
        std::fs::write(&config_path, "").unwrap();

        let found = find_config(dir.path());
        assert_eq!(found, Some(config_path));
    }

    #[test]
    fn find_config_in_parent_dir() {
        let parent = tempfile::tempdir().unwrap();
        let config_path = parent.path().join(".lecs.toml");
        std::fs::write(&config_path, "").unwrap();

        let child = parent.path().join("subdir");
        std::fs::create_dir(&child).unwrap();

        let found = find_config(&child);
        assert_eq!(found, Some(config_path));
    }

    #[test]
    fn find_config_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_config(dir.path());
        assert!(found.is_none());
    }

    #[test]
    fn find_config_in_grandparent_dir() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join(".lecs.toml");
        std::fs::write(&config_path, "").unwrap();

        let child = root.path().join("a").join("b");
        std::fs::create_dir_all(&child).unwrap();

        let found = find_config(&child);
        assert_eq!(found, Some(config_path));
    }

    // ── validate_profile_name tests ──

    #[test]
    fn validate_profile_name_valid_alphanumeric() {
        assert!(validate_profile_name("dev").is_ok());
        assert!(validate_profile_name("staging").is_ok());
        assert!(validate_profile_name("prod-01").is_ok());
        assert!(validate_profile_name("my_profile").is_ok());
        assert!(validate_profile_name("Dev-2").is_ok());
    }

    #[test]
    fn validate_profile_name_rejects_empty() {
        let err = validate_profile_name("").unwrap_err();
        assert!(err.to_string().contains("invalid profile name"));
    }

    #[test]
    fn validate_profile_name_rejects_path_traversal() {
        assert!(validate_profile_name("../etc").is_err());
        assert!(validate_profile_name("foo/bar").is_err());
        assert!(validate_profile_name("foo\\bar").is_err());
        assert!(validate_profile_name("..").is_err());
    }

    #[test]
    fn validate_profile_name_rejects_special_chars() {
        assert!(validate_profile_name("dev;rm -rf").is_err());
        assert!(validate_profile_name("a b").is_err());
        assert!(validate_profile_name("dev.staging").is_err());
    }

    // ── effective_profile tests ──

    #[test]
    fn effective_profile_cli_takes_precedence() {
        let config = LecsConfig {
            default_profile: Some("from-config".to_string()),
        };
        assert_eq!(
            effective_profile(Some("from-cli"), Some(&config)),
            Some("from-cli")
        );
    }

    #[test]
    fn effective_profile_falls_back_to_config() {
        let config = LecsConfig {
            default_profile: Some("from-config".to_string()),
        };
        assert_eq!(effective_profile(None, Some(&config)), Some("from-config"));
    }

    #[test]
    fn effective_profile_none_when_both_absent() {
        let config = LecsConfig {
            default_profile: None,
        };
        assert_eq!(effective_profile(None, Some(&config)), None);
        assert_eq!(effective_profile(None, None), None);
    }

    // ── resolve tests ──

    #[test]
    fn resolve_no_profile_no_flags() {
        let resolved = resolve(Path::new("/project"), None, None, None).unwrap();
        assert_eq!(
            resolved,
            ResolvedPaths {
                override_path: None,
                secrets_path: None,
            }
        );
    }

    #[test]
    fn resolve_profile_sets_convention_paths_when_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let override_file = dir.path().join("lecs-override.dev.json");
        let secrets_file = dir.path().join("secrets.dev.json");
        std::fs::write(&override_file, "{}").unwrap();
        std::fs::write(&secrets_file, "{}").unwrap();

        let resolved = resolve(dir.path(), Some("dev"), None, None).unwrap();
        assert_eq!(resolved.override_path, Some(override_file));
        assert_eq!(resolved.secrets_path, Some(secrets_file));
    }

    #[test]
    fn resolve_profile_returns_none_when_files_missing() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve(dir.path(), Some("dev"), None, None).unwrap();
        assert_eq!(
            resolved,
            ResolvedPaths {
                override_path: None,
                secrets_path: None,
            }
        );
    }

    #[test]
    fn resolve_explicit_override_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_file = dir.path().join("secrets.dev.json");
        std::fs::write(&secrets_file, "{}").unwrap();

        let explicit = Path::new("custom-override.json");
        let resolved = resolve(dir.path(), Some("dev"), Some(explicit), None).unwrap();
        assert_eq!(
            resolved.override_path,
            Some(PathBuf::from("custom-override.json"))
        );
        assert_eq!(resolved.secrets_path, Some(secrets_file));
    }

    #[test]
    fn resolve_explicit_secrets_takes_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let override_file = dir.path().join("lecs-override.dev.json");
        std::fs::write(&override_file, "{}").unwrap();

        let explicit = Path::new("custom-secrets.json");
        let resolved = resolve(dir.path(), Some("dev"), None, Some(explicit)).unwrap();
        assert_eq!(resolved.override_path, Some(override_file));
        assert_eq!(
            resolved.secrets_path,
            Some(PathBuf::from("custom-secrets.json"))
        );
    }

    #[test]
    fn resolve_both_explicit_ignores_profile() {
        let explicit_override = Path::new("o.json");
        let explicit_secrets = Path::new("s.json");
        let resolved = resolve(
            Path::new("/project"),
            Some("dev"),
            Some(explicit_override),
            Some(explicit_secrets),
        )
        .unwrap();
        assert_eq!(resolved.override_path, Some(PathBuf::from("o.json")));
        assert_eq!(resolved.secrets_path, Some(PathBuf::from("s.json")));
    }

    #[test]
    fn resolve_partial_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        // Only override file exists, no secrets file
        let override_file = dir.path().join("lecs-override.dev.json");
        std::fs::write(&override_file, "{}").unwrap();

        let resolved = resolve(dir.path(), Some("dev"), None, None).unwrap();
        assert_eq!(resolved.override_path, Some(override_file));
        assert!(resolved.secrets_path.is_none());
    }

    #[test]
    fn resolve_empty_base_dir_treated_as_current() {
        // When base_dir is empty string, treat as "."
        let resolved = resolve(Path::new(""), Some("dev"), None, None).unwrap();
        // Files won't exist in ".", so both should be None
        assert!(resolved.override_path.is_none());
        assert!(resolved.secrets_path.is_none());
    }

    #[test]
    fn resolve_rejects_path_traversal_profile() {
        let result = resolve(Path::new("/project"), Some("../etc"), None, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("invalid profile name"));
    }

    #[test]
    fn resolve_rejects_slash_in_profile() {
        let result = resolve(Path::new("/project"), Some("foo/bar"), None, None);
        assert!(result.is_err());
    }

    // ── resolve_from_args tests ──

    #[test]
    fn resolve_from_args_uses_task_def_parent_as_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let override_file = dir.path().join("lecs-override.dev.json");
        std::fs::write(&override_file, "{}").unwrap();

        let task_def_path = dir.path().join("task-definition.json");
        let resolved = resolve_from_args(&task_def_path, Some("dev"), None, None).unwrap();
        assert_eq!(resolved.override_path, Some(override_file));
    }

    #[test]
    fn resolve_from_args_explicit_flags_take_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let task_def_path = dir.path().join("task-definition.json");
        let explicit = Path::new("custom.json");
        let resolved =
            resolve_from_args(&task_def_path, Some("dev"), Some(explicit), None).unwrap();
        assert_eq!(resolved.override_path, Some(PathBuf::from("custom.json")));
    }

    #[test]
    fn resolve_from_args_no_profile_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let task_def_path = dir.path().join("task-definition.json");
        let resolved = resolve_from_args(&task_def_path, None, None, None).unwrap();
        assert!(resolved.override_path.is_none());
        assert!(resolved.secrets_path.is_none());
    }

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;

        /// Generate a profile name that matches the accepted character class.
        fn arb_valid_profile_name() -> impl Strategy<Value = String> {
            "[A-Za-z0-9_-]{1,20}"
        }

        /// Generate any unicode char that is *not* allowed in a profile name.
        fn arb_invalid_profile_char() -> impl Strategy<Value = char> {
            proptest::char::any().prop_filter("must be disallowed", |c| {
                !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            })
        }

        /// Generate a non-empty profile name that contains at least one invalid char.
        fn arb_invalid_profile_name() -> impl Strategy<Value = String> {
            (
                arb_valid_profile_name(),
                arb_invalid_profile_char(),
                0usize..6,
            )
                .prop_map(|(valid, bad, raw_pos)| {
                    let bytes = valid.len();
                    let insert_at = raw_pos.min(bytes);
                    let mut out = String::with_capacity(bytes + bad.len_utf8());
                    out.push_str(&valid[..insert_at]);
                    out.push(bad);
                    out.push_str(&valid[insert_at..]);
                    out
                })
        }

        /// Generate a POSIX-ish absolute base directory with simple segments.
        fn arb_base_path() -> impl Strategy<Value = PathBuf> {
            "(/[a-z][a-z0-9_]{0,8}){1,4}".prop_map(PathBuf::from)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(500))]

            /// Property: Valid profile names (regex `[A-Za-z0-9_-]{1,20}`) are always accepted.
            #[test]
            fn valid_profile_name_always_ok(name in arb_valid_profile_name()) {
                prop_assert!(
                    validate_profile_name(&name).is_ok(),
                    "expected '{}' to be accepted",
                    name
                );
            }

            /// Property: Names containing at least one disallowed char are always rejected.
            #[test]
            fn invalid_profile_name_always_errors(name in arb_invalid_profile_name()) {
                let result = validate_profile_name(&name);
                let is_invalid = matches!(
                    result,
                    Err(ProfileError::InvalidProfileName { .. })
                );
                prop_assert!(is_invalid, "expected '{}' to be rejected, got {:?}", name, result);
            }

            /// Property: Empty profile name is always rejected.
            #[test]
            fn empty_profile_name_rejected(_seed in 0u32..10u32) {
                let result = validate_profile_name("");
                let is_invalid = matches!(
                    result,
                    Err(ProfileError::InvalidProfileName { .. })
                );
                prop_assert!(is_invalid);
            }

            /// Property: override path has the expected `lecs-override.<profile>.json` shape.
            #[test]
            fn override_path_shape(
                base in arb_base_path(),
                profile in arb_valid_profile_name(),
            ) {
                let path = profile_override_path(&base, &profile);
                let expected_file = format!("lecs-override.{profile}.json");
                prop_assert_eq!(
                    path.file_name().and_then(|s| s.to_str()),
                    Some(expected_file.as_str())
                );
                prop_assert_eq!(path.parent(), Some(base.as_path()));
            }

            /// Property: secrets path has the expected `secrets.<profile>.json` shape.
            #[test]
            fn secrets_path_shape(
                base in arb_base_path(),
                profile in arb_valid_profile_name(),
            ) {
                let path = profile_secrets_path(&base, &profile);
                let expected_file = format!("secrets.{profile}.json");
                prop_assert_eq!(
                    path.file_name().and_then(|s| s.to_str()),
                    Some(expected_file.as_str())
                );
                prop_assert_eq!(path.parent(), Some(base.as_path()));
            }

            /// Property: override and secrets paths for the same profile never collide.
            #[test]
            fn profile_paths_differ(
                base in arb_base_path(),
                profile in arb_valid_profile_name(),
            ) {
                let o = profile_override_path(&base, &profile);
                let s = profile_secrets_path(&base, &profile);
                prop_assert_ne!(o, s);
            }

            /// Property: the profile name always appears in both rendered paths.
            #[test]
            fn path_contains_profile_name(
                base in arb_base_path(),
                profile in arb_valid_profile_name(),
            ) {
                let o = profile_override_path(&base, &profile);
                let s = profile_secrets_path(&base, &profile);
                prop_assert!(o.to_string_lossy().contains(&profile));
                prop_assert!(s.to_string_lossy().contains(&profile));
            }

            /// Property: CLI profile always wins over config default.
            #[test]
            fn effective_profile_cli_wins(
                cli in arb_valid_profile_name(),
                default in arb_valid_profile_name(),
            ) {
                let config = LecsConfig { default_profile: Some(default) };
                let result = effective_profile(Some(&cli), Some(&config));
                prop_assert_eq!(result, Some(cli.as_str()));
            }

            /// Property: Without CLI profile, config's default_profile is returned.
            #[test]
            fn effective_profile_falls_back(default in arb_valid_profile_name()) {
                let expected = default.clone();
                let config = LecsConfig { default_profile: Some(default) };
                let result = effective_profile(None, Some(&config));
                prop_assert_eq!(result, Some(expected.as_str()));
            }

            /// Property: When both explicit override and secrets paths are given,
            /// the profile is ignored entirely on both axes.
            #[test]
            fn resolve_explicit_dominates(
                base in arb_base_path(),
                profile in arb_valid_profile_name(),
                o_name in "[a-z]{1,10}\\.json",
                s_name in "[a-z]{1,10}\\.json",
            ) {
                let ov = PathBuf::from(&o_name);
                let sc = PathBuf::from(&s_name);
                let resolved = resolve(
                    &base,
                    Some(&profile),
                    Some(ov.as_path()),
                    Some(sc.as_path()),
                ).expect("valid profile should not error");
                prop_assert_eq!(resolved.override_path, Some(ov));
                prop_assert_eq!(resolved.secrets_path, Some(sc));
            }

            /// Property: resolve rejects invalid profile names.
            #[test]
            fn resolve_rejects_invalid_profile(
                base in arb_base_path(),
                bad in arb_invalid_profile_name(),
            ) {
                let result = resolve(&base, Some(&bad), None, None);
                let is_invalid = matches!(
                    result,
                    Err(ProfileError::InvalidProfileName { .. })
                );
                prop_assert!(is_invalid);
            }
        }
    }
}
