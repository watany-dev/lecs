//! Rich validation diagnostics for task definitions.
//!
//! Provides structured diagnostic types that collect all validation issues
//! (rather than fail-fast) with field paths, suggestions, and severity levels.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::orchestrator::{self, DependencyInfo};
use crate::overrides::OverrideConfig;

use super::TaskDefinition;

/// Severity level for validation diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Must be fixed before running.
    Error,
    /// Likely a mistake but not fatal.
    Warning,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Error => write!(f, "error"),
            Self::Warning => write!(f, "warning"),
        }
    }
}

/// A structured validation diagnostic with context for human-friendly output.
#[derive(Debug, Clone)]
pub struct ValidationDiagnostic {
    /// Severity of this diagnostic.
    pub severity: Severity,
    /// JSON-like field path (e.g., `containerDefinitions[0].image`).
    pub field_path: String,
    /// Human-readable error message.
    pub message: String,
    /// Optional suggestion for how to fix the issue.
    pub suggestion: Option<String>,
}

impl fmt::Display for ValidationDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} - {}",
            self.severity, self.field_path, self.message
        )?;
        if let Some(suggestion) = &self.suggestion {
            write!(f, " (hint: {suggestion})")?;
        }
        Ok(())
    }
}

/// Result of comprehensive validation containing all diagnostics.
#[derive(Debug)]
pub struct ValidationReport {
    /// All collected diagnostics.
    pub diagnostics: Vec<ValidationDiagnostic>,
}

impl ValidationReport {
    /// Returns `true` if the report contains any error-level diagnostics.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Count of error-level diagnostics.
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    /// Count of warning-level diagnostics.
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diagnostic in &self.diagnostics {
            writeln!(f, "{diagnostic}")?;
        }
        let errors = self.error_count();
        let warnings = self.warning_count();
        write!(f, "{errors} error(s), {warnings} warning(s)")
    }
}

/// Run all extended validation checks on a parsed task definition.
///
/// Unlike `TaskDefinition::validate()` (fail-fast), this collects all issues.
pub fn validate_extended(task_def: &TaskDefinition) -> ValidationReport {
    let mut diagnostics = Vec::new();

    // Image format checks
    for (i, container) in task_def.container_definitions.iter().enumerate() {
        if let Some(d) = check_image_format(
            &container.image,
            &format!("containerDefinitions[{i}].image"),
        ) {
            diagnostics.push(d);
        }
    }

    // Port conflict checks
    diagnostics.extend(check_port_conflicts(task_def));

    // DependsOn reference + cycle checks
    diagnostics.extend(check_depends_on(task_def));

    // Secret ARN format checks
    diagnostics.extend(check_secret_arn_format(task_def));

    // Common mistake warnings
    diagnostics.extend(check_common_mistakes(task_def));

    ValidationReport { diagnostics }
}

/// Cross-validate override container names against task definition.
pub fn validate_overrides(
    task_def: &TaskDefinition,
    overrides: &OverrideConfig,
) -> Vec<ValidationDiagnostic> {
    let names: HashSet<&str> = task_def
        .container_definitions
        .iter()
        .map(|c| c.name.as_str())
        .collect();

    overrides
        .container_overrides
        .keys()
        .filter(|name| !names.contains(name.as_str()))
        .map(|name| ValidationDiagnostic {
            severity: Severity::Error,
            field_path: format!("containerOverrides.{name}"),
            message: format!("override references unknown container '{name}'"),
            suggestion: Some(format!("available containers: {}", {
                let mut sorted: Vec<_> = names.iter().copied().collect();
                sorted.sort_unstable();
                sorted.join(", ")
            })),
        })
        .collect()
}

/// Validate Docker image name format.
///
/// Rejects images with whitespace, leading/trailing `/` or `:`,
/// consecutive `//` or `::`, and names not starting with an alphanumeric character.
fn check_image_format(image: &str, field_path: &str) -> Option<ValidationDiagnostic> {
    if image.is_empty() {
        // Already caught by TaskDefinition::validate(), but include for completeness.
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: "image name must not be empty".to_string(),
            suggestion: Some("specify a valid image like 'nginx:latest'".to_string()),
        });
    }

    if image.chars().any(char::is_whitespace) {
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: format!("image name '{image}' contains whitespace"),
            suggestion: Some("remove whitespace from the image name".to_string()),
        });
    }

    if image.starts_with('/') || image.ends_with('/') {
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: format!("image name '{image}' must not start or end with '/'"),
            suggestion: None,
        });
    }

    if image.starts_with(':') || image.ends_with(':') {
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: format!("image name '{image}' must not start or end with ':'"),
            suggestion: None,
        });
    }

    if image.contains("//") || image.contains("::") {
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: format!("image name '{image}' contains consecutive '//' or '::'"),
            suggestion: None,
        });
    }

    // Must start with an alphanumeric character.
    if let Some(first) = image.chars().next()
        && !first.is_ascii_alphanumeric()
    {
        return Some(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: field_path.to_string(),
            message: format!("image name '{image}' must start with an alphanumeric character"),
            suggestion: None,
        });
    }

    None
}

/// Check `dependsOn` references and detect circular dependencies.
fn check_depends_on(task_def: &TaskDefinition) -> Vec<ValidationDiagnostic> {
    let mut diagnostics = Vec::new();
    let names: HashSet<&str> = task_def
        .container_definitions
        .iter()
        .map(|c| c.name.as_str())
        .collect();

    // Check references exist
    for (ci, container) in task_def.container_definitions.iter().enumerate() {
        for (di, dep) in container.depends_on.iter().enumerate() {
            if dep.container_name == container.name {
                diagnostics.push(ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: format!("containerDefinitions[{ci}].dependsOn[{di}]"),
                    message: format!(
                        "container '{}' has a self-referencing dependency",
                        container.name
                    ),
                    suggestion: None,
                });
            } else if !names.contains(dep.container_name.as_str()) {
                diagnostics.push(ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: format!("containerDefinitions[{ci}].dependsOn[{di}]"),
                    message: format!(
                        "container '{}' depends on unknown container '{}'",
                        container.name, dep.container_name
                    ),
                    suggestion: Some(format!("available containers: {}", {
                        let mut sorted: Vec<_> = names.iter().copied().collect();
                        sorted.sort_unstable();
                        sorted.join(", ")
                    })),
                });
            }
        }
    }

    // Cycle detection via orchestrator's resolve_start_order
    let deps: Vec<DependencyInfo> = task_def
        .container_definitions
        .iter()
        .map(|c| DependencyInfo {
            name: c.name.clone(),
            depends_on: c.depends_on.clone(),
        })
        .collect();

    if let Err(e) = orchestrator::resolve_start_order(&deps) {
        diagnostics.push(ValidationDiagnostic {
            severity: Severity::Error,
            field_path: "containerDefinitions.dependsOn".to_string(),
            message: e.to_string(),
            suggestion: Some("remove or reorder dependencies to break the cycle".to_string()),
        });
    }

    diagnostics
}

/// Validate secret ARN format.
///
/// Expects ARNs to start with `arn:aws:secretsmanager:` and have at least 7 colon-separated segments.
fn check_secret_arn_format(task_def: &TaskDefinition) -> Vec<ValidationDiagnostic> {
    let mut diagnostics = Vec::new();

    for (ci, container) in task_def.container_definitions.iter().enumerate() {
        for (si, secret) in container.secrets.iter().enumerate() {
            let arn = &secret.value_from;
            if !arn.starts_with("arn:aws:secretsmanager:") {
                diagnostics.push(ValidationDiagnostic {
                    severity: Severity::Warning,
                    field_path: format!(
                        "containerDefinitions[{ci}].secrets[{si}].valueFrom"
                    ),
                    message: format!(
                        "secret ARN '{arn}' does not match expected format 'arn:aws:secretsmanager:...'"
                    ),
                    suggestion: Some(
                        "use a full ARN like 'arn:aws:secretsmanager:us-east-1:123456789012:secret:name'"
                            .to_string(),
                    ),
                });
            } else if arn.split(':').count() < 7 {
                diagnostics.push(ValidationDiagnostic {
                    severity: Severity::Warning,
                    field_path: format!(
                        "containerDefinitions[{ci}].secrets[{si}].valueFrom"
                    ),
                    message: format!(
                        "secret ARN '{arn}' appears incomplete (expected at least 7 colon-separated segments)"
                    ),
                    suggestion: Some(
                        "use a full ARN like 'arn:aws:secretsmanager:us-east-1:123456789012:secret:name'"
                            .to_string(),
                    ),
                });
            }
        }
    }

    diagnostics
}

/// Warn on common mistakes in task definitions.
fn check_common_mistakes(task_def: &TaskDefinition) -> Vec<ValidationDiagnostic> {
    let mut diagnostics = Vec::new();

    // All containers have essential=false
    if !task_def.container_definitions.is_empty()
        && task_def.container_definitions.iter().all(|c| !c.essential)
    {
        diagnostics.push(ValidationDiagnostic {
            severity: Severity::Warning,
            field_path: "containerDefinitions.essential".to_string(),
            message: "all containers have essential=false; the task will not stop when any container exits".to_string(),
            suggestion: Some("set at least one container as essential".to_string()),
        });
    }

    // No port mappings in entire task
    if !task_def.container_definitions.is_empty()
        && task_def
            .container_definitions
            .iter()
            .all(|c| c.port_mappings.is_empty())
    {
        diagnostics.push(ValidationDiagnostic {
            severity: Severity::Warning,
            field_path: "containerDefinitions.portMappings".to_string(),
            message: "no port mappings defined in any container".to_string(),
            suggestion: Some(
                "add port mappings if you need to access containers from the host".to_string(),
            ),
        });
    }

    diagnostics
}

/// Detect host port conflicts across all containers.
///
/// Two port mappings conflict if they map to the same host port with the same protocol.
fn check_port_conflicts(task_def: &TaskDefinition) -> Vec<ValidationDiagnostic> {
    let mut diagnostics = Vec::new();
    // (host_port, protocol) -> (container_index, port_mapping_index, container_name)
    let mut seen: HashMap<(u16, String), (usize, usize, String)> = HashMap::new();

    for (ci, container) in task_def.container_definitions.iter().enumerate() {
        for (pi, pm) in container.port_mappings.iter().enumerate() {
            let host_port = pm.host_port.unwrap_or(pm.container_port);
            let key = (host_port, pm.protocol.clone());

            if let Some((prev_ci, prev_pi, prev_name)) = seen.get(&key) {
                diagnostics.push(ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: format!(
                        "containerDefinitions[{ci}].portMappings[{pi}]"
                    ),
                    message: format!(
                        "host port {host_port}/{} conflicts with containerDefinitions[{prev_ci}].portMappings[{prev_pi}] (container '{prev_name}')",
                        pm.protocol
                    ),
                    suggestion: Some("use a different host port".to_string()),
                });
            } else {
                seen.insert(key, (ci, pi, container.name.clone()));
            }
        }
    }

    diagnostics
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn severity_display() {
        assert_eq!(Severity::Error.to_string(), "error");
        assert_eq!(Severity::Warning.to_string(), "warning");
    }

    #[test]
    fn diagnostic_display_without_suggestion() {
        let d = ValidationDiagnostic {
            severity: Severity::Error,
            field_path: "containerDefinitions[0].image".to_string(),
            message: "image name contains whitespace".to_string(),
            suggestion: None,
        };
        assert_eq!(
            d.to_string(),
            "error: containerDefinitions[0].image - image name contains whitespace"
        );
    }

    #[test]
    fn diagnostic_display_with_suggestion() {
        let d = ValidationDiagnostic {
            severity: Severity::Warning,
            field_path: "containerDefinitions[0].essential".to_string(),
            message: "all containers have essential=false".to_string(),
            suggestion: Some("set at least one container as essential".to_string()),
        };
        assert_eq!(
            d.to_string(),
            "warning: containerDefinitions[0].essential - all containers have essential=false (hint: set at least one container as essential)"
        );
    }

    #[test]
    fn report_counts_and_has_errors() {
        let report = ValidationReport {
            diagnostics: vec![
                ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: "family".to_string(),
                    message: "empty".to_string(),
                    suggestion: None,
                },
                ValidationDiagnostic {
                    severity: Severity::Warning,
                    field_path: "containerDefinitions".to_string(),
                    message: "no port mappings".to_string(),
                    suggestion: None,
                },
                ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: "containerDefinitions[0].image".to_string(),
                    message: "invalid".to_string(),
                    suggestion: None,
                },
            ],
        };
        assert!(report.has_errors());
        assert_eq!(report.error_count(), 2);
        assert_eq!(report.warning_count(), 1);
    }

    #[test]
    fn report_no_errors() {
        let report = ValidationReport {
            diagnostics: vec![ValidationDiagnostic {
                severity: Severity::Warning,
                field_path: "containerDefinitions".to_string(),
                message: "no port mappings".to_string(),
                suggestion: None,
            }],
        };
        assert!(!report.has_errors());
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 1);
    }

    #[test]
    fn report_display() {
        let report = ValidationReport {
            diagnostics: vec![
                ValidationDiagnostic {
                    severity: Severity::Error,
                    field_path: "family".to_string(),
                    message: "empty".to_string(),
                    suggestion: None,
                },
                ValidationDiagnostic {
                    severity: Severity::Warning,
                    field_path: "ports".to_string(),
                    message: "none".to_string(),
                    suggestion: Some("add port mappings".to_string()),
                },
            ],
        };
        let output = report.to_string();
        assert!(output.contains("error: family - empty"));
        assert!(output.contains("warning: ports - none (hint: add port mappings)"));
        assert!(output.contains("1 error(s), 1 warning(s)"));
    }

    #[test]
    fn empty_report() {
        let report = ValidationReport {
            diagnostics: vec![],
        };
        assert!(!report.has_errors());
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 0);
        assert!(report.to_string().contains("0 error(s), 0 warning(s)"));
    }

    // --- Image format validation tests ---

    #[test]
    fn image_format_valid_simple() {
        assert!(check_image_format("nginx", "test").is_none());
    }

    #[test]
    fn image_format_valid_with_tag() {
        assert!(check_image_format("nginx:latest", "test").is_none());
    }

    #[test]
    fn image_format_valid_registry() {
        assert!(check_image_format("myregistry.io/app:v1.2.3", "test").is_none());
    }

    #[test]
    fn image_format_valid_multi_level() {
        assert!(check_image_format("registry.example.com/org/app:latest", "test").is_none());
    }

    #[test]
    fn image_format_valid_sha256() {
        assert!(check_image_format("nginx@sha256:abc123def", "test").is_none());
    }

    #[test]
    fn image_format_invalid_whitespace() {
        let d = check_image_format("nginx latest", "containerDefinitions[0].image").unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("whitespace"));
    }

    #[test]
    fn image_format_invalid_leading_slash() {
        let d = check_image_format("/nginx", "test").unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("start or end with '/'"));
    }

    #[test]
    fn image_format_invalid_trailing_colon() {
        let d = check_image_format("nginx:", "test").unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("start or end with ':'"));
    }

    #[test]
    fn image_format_invalid_consecutive_slashes() {
        let d = check_image_format("registry//image", "test").unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("consecutive"));
    }

    #[test]
    fn image_format_invalid_starts_nonalpha() {
        let d = check_image_format(".hidden", "test").unwrap();
        assert_eq!(d.severity, Severity::Error);
        assert!(d.message.contains("alphanumeric"));
    }

    // --- Port conflict tests ---

    fn make_task_def(json: &str) -> TaskDefinition {
        TaskDefinition::from_json(json).expect("test task def should parse")
    }

    /// Parse a task definition without running fail-fast validation.
    /// Used for testing diagnostics on invalid task definitions that would be
    /// rejected by `TaskDefinition::validate()`.
    fn make_task_def_unchecked(json: &str) -> TaskDefinition {
        serde_json::from_str(json).expect("test task def should parse JSON")
    }

    #[test]
    fn port_conflicts_none() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080 }
                    ]
                },
                {
                    "name": "b",
                    "image": "alpine:latest",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8081 }
                    ]
                }
            ]
        }"#,
        );
        assert!(check_port_conflicts(&td).is_empty());
    }

    #[test]
    fn port_conflicts_same_host_port_different_protocol_ok() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080, "protocol": "tcp" },
                        { "containerPort": 80, "hostPort": 8080, "protocol": "udp" }
                    ]
                }
            ]
        }"#,
        );
        assert!(check_port_conflicts(&td).is_empty());
    }

    #[test]
    fn port_conflicts_same_host_port_same_protocol() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080 },
                        { "containerPort": 81, "hostPort": 8080 }
                    ]
                }
            ]
        }"#,
        );
        let conflicts = check_port_conflicts(&td);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].severity, Severity::Error);
        assert!(conflicts[0].message.contains("8080/tcp"));
    }

    #[test]
    fn port_conflicts_across_containers() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "web",
                    "image": "nginx:latest",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080 }
                    ]
                },
                {
                    "name": "api",
                    "image": "node:latest",
                    "portMappings": [
                        { "containerPort": 3000, "hostPort": 8080 }
                    ]
                }
            ]
        }"#,
        );
        let conflicts = check_port_conflicts(&td);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].message.contains("web"));
    }

    // --- validate_extended integration tests ---

    #[test]
    fn validate_extended_valid_task() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "nginx:latest",
                    "portMappings": [{ "containerPort": 80, "hostPort": 8080 }]
                }
            ]
        }"#,
        );
        let report = validate_extended(&td);
        assert!(!report.has_errors());
    }

    #[test]
    fn validate_extended_catches_image_and_port_issues() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": ".invalid-image",
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080 },
                        { "containerPort": 81, "hostPort": 8080 }
                    ]
                }
            ]
        }"#,
        );
        let report = validate_extended(&td);
        assert_eq!(report.error_count(), 2);
    }

    #[test]
    fn port_conflicts_host_port_defaults_to_container_port() {
        // When hostPort is omitted, it defaults to containerPort.
        // Two containers both mapping containerPort=80 without hostPort should conflict.
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "portMappings": [{ "containerPort": 80 }]
                },
                {
                    "name": "b",
                    "image": "alpine:latest",
                    "portMappings": [{ "containerPort": 80 }]
                }
            ]
        }"#,
        );
        let conflicts = check_port_conflicts(&td);
        assert_eq!(conflicts.len(), 1);
    }

    // --- dependsOn validation tests ---

    #[test]
    fn depends_on_valid_references() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "db",
                    "image": "postgres:16",
                    "healthCheck": { "command": ["CMD-SHELL", "pg_isready"] }
                },
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "db", "condition": "HEALTHY" }]
                }
            ]
        }"#,
        );
        let diags = check_depends_on(&td);
        assert!(diags.is_empty());
    }

    #[test]
    fn depends_on_unknown_reference() {
        let td = make_task_def_unchecked(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "nonexistent", "condition": "START" }]
                }
            ]
        }"#,
        );
        let diags = check_depends_on(&td);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Error && d.message.contains("unknown container"))
        );
    }

    #[test]
    fn depends_on_self_reference() {
        let td = make_task_def_unchecked(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "app", "condition": "START" }]
                }
            ]
        }"#,
        );
        let diags = check_depends_on(&td);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Error && d.message.contains("self-referencing"))
        );
    }

    #[test]
    fn depends_on_circular_two_nodes() {
        let td = make_task_def_unchecked(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "b", "condition": "START" }]
                },
                {
                    "name": "b",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "a", "condition": "START" }]
                }
            ]
        }"#,
        );
        let diags = check_depends_on(&td);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Error && d.message.contains("cyclic"))
        );
    }

    #[test]
    fn depends_on_circular_three_nodes() {
        let td = make_task_def_unchecked(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "c", "condition": "START" }]
                },
                {
                    "name": "b",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "a", "condition": "START" }]
                },
                {
                    "name": "c",
                    "image": "alpine:latest",
                    "dependsOn": [{ "containerName": "b", "condition": "START" }]
                }
            ]
        }"#,
        );
        let diags = check_depends_on(&td);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Error && d.message.contains("cyclic"))
        );
    }

    // --- Secret ARN format tests ---

    #[test]
    fn secret_arn_valid() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "secrets": [
                        { "name": "DB_PASS", "valueFrom": "arn:aws:secretsmanager:us-east-1:123456789012:secret:my-secret" }
                    ]
                }
            ]
        }"#,
        );
        assert!(check_secret_arn_format(&td).is_empty());
    }

    #[test]
    fn secret_arn_invalid_prefix() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "secrets": [
                        { "name": "DB_PASS", "valueFrom": "not-an-arn" }
                    ]
                }
            ]
        }"#,
        );
        let diags = check_secret_arn_format(&td);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert!(diags[0].message.contains("does not match"));
    }

    #[test]
    fn secret_arn_incomplete() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "secrets": [
                        { "name": "DB_PASS", "valueFrom": "arn:aws:secretsmanager:us-east-1:123" }
                    ]
                }
            ]
        }"#,
        );
        let diags = check_secret_arn_format(&td);
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("incomplete"));
    }

    #[test]
    fn secret_arn_no_secrets_no_warnings() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "app", "image": "alpine:latest" }
            ]
        }"#,
        );
        assert!(check_secret_arn_format(&td).is_empty());
    }

    // --- Common mistakes tests ---

    #[test]
    fn common_mistakes_all_essential_false() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "a", "image": "alpine:latest", "essential": false },
                { "name": "b", "image": "alpine:latest", "essential": false }
            ]
        }"#,
        );
        let diags = check_common_mistakes(&td);
        assert!(diags.iter().any(|d| d.message.contains("essential=false")));
    }

    #[test]
    fn common_mistakes_no_port_mappings() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "a", "image": "alpine:latest" },
                { "name": "b", "image": "alpine:latest" }
            ]
        }"#,
        );
        let diags = check_common_mistakes(&td);
        assert!(diags.iter().any(|d| d.message.contains("no port mappings")));
    }

    #[test]
    fn common_mistakes_normal_task_no_warnings() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "app",
                    "image": "alpine:latest",
                    "essential": true,
                    "portMappings": [{ "containerPort": 80, "hostPort": 8080 }]
                }
            ]
        }"#,
        );
        let diags = check_common_mistakes(&td);
        assert!(diags.is_empty());
    }

    #[test]
    fn common_mistakes_has_essential_true_no_warning() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "a", "image": "alpine:latest", "essential": true },
                { "name": "b", "image": "alpine:latest", "essential": false }
            ]
        }"#,
        );
        let diags = check_common_mistakes(&td);
        assert!(!diags.iter().any(|d| d.message.contains("essential=false")));
    }

    // --- validate_overrides tests ---

    #[test]
    fn validate_overrides_valid_names() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "app", "image": "alpine:latest" }
            ]
        }"#,
        );
        let overrides = OverrideConfig::from_json(
            r#"{
            "containerOverrides": {
                "app": { "image": "alpine:3.18" }
            }
        }"#,
        )
        .unwrap();
        let diags = validate_overrides(&td, &overrides);
        assert!(diags.is_empty());
    }

    #[test]
    fn validate_overrides_unknown_container() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "app", "image": "alpine:latest" }
            ]
        }"#,
        );
        let overrides = OverrideConfig::from_json(
            r#"{
            "containerOverrides": {
                "nonexistent": { "image": "alpine:3.18" }
            }
        }"#,
        )
        .unwrap();
        let diags = validate_overrides(&td, &overrides);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("unknown container 'nonexistent'"));
    }

    #[test]
    fn validate_overrides_empty() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                { "name": "app", "image": "alpine:latest" }
            ]
        }"#,
        );
        let overrides = OverrideConfig::from_json(
            r#"{
            "containerOverrides": {}
        }"#,
        )
        .unwrap();
        let diags = validate_overrides(&td, &overrides);
        assert!(diags.is_empty());
    }

    // --- validate_extended integration with all checks ---

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;

        /// Generate a valid Docker image name: alphanumeric start, no whitespace,
        /// no leading/trailing `/` or `:`, no `//` or `::`.
        fn arb_valid_image() -> impl Strategy<Value = String> {
            // registry/name:tag pattern
            (
                "[a-z0-9][a-z0-9.-]{0,15}",                         // registry or name
                proptest::option::of("/[a-z0-9][a-z0-9.-]{0,10}"),  // optional path
                proptest::option::of(":[a-z0-9][a-z0-9._-]{0,15}"), // optional tag
            )
                .prop_map(|(name, path, tag)| {
                    let mut image = name;
                    if let Some(p) = path {
                        image.push_str(&p);
                    }
                    if let Some(t) = tag {
                        image.push_str(&t);
                    }
                    image
                })
        }

        /// Build a minimal valid `TaskDefinition` with given containers.
        #[allow(clippy::type_complexity)]
        fn make_task_def_with_containers(
            containers: Vec<(String, String, Vec<(u16, Option<u16>, String)>)>,
        ) -> TaskDefinition {
            use crate::taskdef::*;
            TaskDefinition {
                family: "test".into(),
                task_role_arn: None,
                execution_role_arn: None,
                volumes: vec![],
                container_definitions: containers
                    .into_iter()
                    .map(|(name, image, ports)| ContainerDefinition {
                        name,
                        image,
                        port_mappings: ports
                            .into_iter()
                            .map(|(cp, hp, proto)| PortMapping {
                                container_port: cp,
                                host_port: hp,
                                protocol: proto,
                            })
                            .collect(),
                        ..Default::default()
                    })
                    .collect(),
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(300))]

            /// Property: Valid image names produce no diagnostics.
            #[test]
            fn valid_image_no_diagnostic(image in arb_valid_image()) {
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_none(), "valid image '{}' should not produce diagnostic: {:?}", image, result);
            }

            /// Property: Images with whitespace always produce an error.
            #[test]
            fn image_with_whitespace_is_error(
                prefix in "[a-z]{1,5}",
                ws in "[ \t\n\r]",
                suffix in "[a-z]{1,5}",
            ) {
                let image = format!("{prefix}{ws}{suffix}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' with whitespace should be rejected", image);
                prop_assert_eq!(result.as_ref().map(|d| d.severity), Some(Severity::Error));
            }

            /// Property: Empty image always produces an error.
            #[test]
            fn empty_image_is_error(_seed in 0u32..10u32) {
                let result = check_image_format("", "test");
                prop_assert!(result.is_some());
                prop_assert_eq!(result.as_ref().map(|d| d.severity), Some(Severity::Error));
            }

            /// Property: Images starting with '/' always produce an error.
            #[test]
            fn leading_slash_is_error(rest in "[a-z0-9]{1,10}") {
                let image = format!("/{rest}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images ending with '/' always produce an error.
            #[test]
            fn trailing_slash_is_error(prefix in "[a-z0-9]{1,10}") {
                let image = format!("{prefix}/");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images starting with ':' always produce an error.
            #[test]
            fn leading_colon_is_error(rest in "[a-z0-9]{1,10}") {
                let image = format!(":{rest}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images ending with ':' always produce an error.
            #[test]
            fn trailing_colon_is_error(prefix in "[a-z0-9]{1,10}") {
                let image = format!("{prefix}:");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images containing '//' always produce an error.
            #[test]
            fn double_slash_is_error(
                prefix in "[a-z0-9]{1,5}",
                suffix in "[a-z0-9]{1,5}",
            ) {
                let image = format!("{prefix}//{suffix}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images containing '::' always produce an error.
            #[test]
            fn double_colon_is_error(
                prefix in "[a-z0-9]{1,5}",
                suffix in "[a-z0-9]{1,5}",
            ) {
                let image = format!("{prefix}::{suffix}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' should be rejected", image);
            }

            /// Property: Images not starting with alphanumeric always produce an error.
            #[test]
            fn non_alphanumeric_start_is_error(
                first in "[^a-zA-Z0-9/ :\t\n\r]",
                rest in "[a-z0-9]{0,10}",
            ) {
                let image = format!("{first}{rest}");
                let result = check_image_format(&image, "test");
                prop_assert!(result.is_some(), "image '{}' starting with non-alphanumeric should be rejected", image);
            }

            /// Property: All diagnostics from check_image_format are Error severity.
            #[test]
            fn image_diagnostics_are_always_errors(image in "\\PC{1,30}") {
                if let Some(d) = check_image_format(&image, "test") {
                    prop_assert_eq!(d.severity, Severity::Error, "image format diagnostics should always be Error");
                }
            }

            /// Property: Port conflicts are commutative — if we swap container order,
            /// we still detect the same number of conflicts.
            #[test]
            fn port_conflict_count_independent_of_order(
                port in 1u16..65535u16,
                proto in prop_oneof![Just("tcp".to_string()), Just("udp".to_string())],
            ) {
                let td1 = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(port, Some(port), proto.clone())]),
                    ("b".into(), "alpine:latest".into(), vec![(port, Some(port), proto.clone())]),
                ]);
                let td2 = make_task_def_with_containers(vec![
                    ("b".into(), "alpine:latest".into(), vec![(port, Some(port), proto.clone())]),
                    ("a".into(), "alpine:latest".into(), vec![(port, Some(port), proto)]),
                ]);

                let c1 = check_port_conflicts(&td1).len();
                let c2 = check_port_conflicts(&td2).len();
                prop_assert_eq!(c1, c2, "port conflict count should be order-independent");
            }

            /// Property: Distinct host ports never produce conflicts.
            #[test]
            fn distinct_ports_no_conflict(
                port1 in 1u16..32000u16,
                port2 in 32001u16..65535u16,
            ) {
                let td = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(80, Some(port1), "tcp".into())]),
                    ("b".into(), "alpine:latest".into(), vec![(80, Some(port2), "tcp".into())]),
                ]);
                prop_assert!(check_port_conflicts(&td).is_empty());
            }

            /// Property: Same host port but different protocols don't conflict.
            #[test]
            fn same_port_different_protocol_no_conflict(port in 1u16..65535u16) {
                let td = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(80, Some(port), "tcp".into())]),
                    ("b".into(), "alpine:latest".into(), vec![(80, Some(port), "udp".into())]),
                ]);
                prop_assert!(check_port_conflicts(&td).is_empty());
            }

            /// Property: Valid ARNs produce no warnings.
            #[test]
            fn valid_secret_arn_no_warning(
                region in "(us|eu|ap)-(east|west|central)-[1-3]",
                account in "[0-9]{12}",
                name in "[a-zA-Z][a-zA-Z0-9_-]{0,20}",
            ) {
                let arn = format!("arn:aws:secretsmanager:{region}:{account}:secret:{name}");
                let td = TaskDefinition {
                    family: "test".into(),
                    task_role_arn: None,
                    execution_role_arn: None,
                    volumes: vec![],
                    container_definitions: vec![crate::taskdef::ContainerDefinition {
                        name: "app".into(),
                        image: "alpine:latest".into(),
                        secrets: vec![crate::taskdef::Secret {
                            name: "SECRET".into(),
                            value_from: arn,
                        }],
                        ..Default::default()
                    }],
                };
                let diags = check_secret_arn_format(&td);
                prop_assert!(diags.is_empty(), "valid ARN should produce no warnings: {:?}", diags);
            }

            /// Property: A task definition with no port mappings anywhere produces no diagnostics.
            #[test]
            fn no_ports_no_conflicts(
                n_containers in 1usize..=5usize,
            ) {
                let containers: Vec<_> = (0..n_containers)
                    .map(|i| (format!("c{i}"), "alpine:latest".to_string(), vec![]))
                    .collect();
                let td = make_task_def_with_containers(containers);
                prop_assert!(check_port_conflicts(&td).is_empty());
            }

            /// Property: A set of pairwise-distinct (host_port, protocol) pairs yields no conflicts.
            #[test]
            fn unique_host_ports_no_conflicts(
                pairs in proptest::collection::btree_set(
                    (1024u16..65_000, prop_oneof![Just("tcp".to_string()), Just("udp".to_string())]),
                    1..=8,
                ),
            ) {
                // Put each mapping in a separate container to avoid container-port duplication.
                let containers: Vec<_> = pairs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (hp, proto))| {
                        (
                            format!("c{i}"),
                            "alpine:latest".to_string(),
                            vec![(80, Some(hp), proto)],
                        )
                    })
                    .collect();
                let td = make_task_def_with_containers(containers);
                prop_assert!(
                    check_port_conflicts(&td).is_empty(),
                    "unique (host_port, protocol) pairs should not conflict: {:?}",
                    check_port_conflicts(&td)
                );
            }

            /// Property: Inserting an exact duplicate (host_port, protocol) always produces
            /// at least one Error diagnostic.
            #[test]
            fn duplicate_pair_always_conflicts(
                host_port in 1024u16..65_000,
                proto in prop_oneof![Just("tcp".to_string()), Just("udp".to_string())],
            ) {
                let td = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(80, Some(host_port), proto.clone())]),
                    ("b".into(), "alpine:latest".into(), vec![(81, Some(host_port), proto)]),
                ]);
                let diags = check_port_conflicts(&td);
                prop_assert!(!diags.is_empty(), "expected at least one conflict");
                prop_assert!(
                    diags.iter().all(|d| d.severity == Severity::Error),
                    "all port conflicts must be Error severity"
                );
            }

            /// Property: When hostPort is omitted, it defaults to containerPort for conflict detection.
            #[test]
            fn host_port_defaults_to_container_port(
                container_port in 1024u16..65_000,
            ) {
                // Two containers both omitting hostPort with the same containerPort ⇒ conflict.
                let td = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(container_port, None, "tcp".into())]),
                    ("b".into(), "alpine:latest".into(), vec![(container_port, None, "tcp".into())]),
                ]);
                prop_assert!(!check_port_conflicts(&td).is_empty());
            }

            /// Property: Adding a port mapping never reduces the conflict count.
            #[test]
            fn conflict_count_monotonic_in_mappings(
                extra_hp in 1024u16..65_000,
                base_hp in 1024u16..65_000,
            ) {
                let base = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![(80, Some(base_hp), "tcp".into())]),
                    ("b".into(), "alpine:latest".into(), vec![(80, Some(base_hp), "tcp".into())]),
                ]);
                let base_count = check_port_conflicts(&base).len();
                let extended = make_task_def_with_containers(vec![
                    ("a".into(), "alpine:latest".into(), vec![
                        (80, Some(base_hp), "tcp".into()),
                        (81, Some(extra_hp), "tcp".into()),
                    ]),
                    ("b".into(), "alpine:latest".into(), vec![(80, Some(base_hp), "tcp".into())]),
                ]);
                let ext_count = check_port_conflicts(&extended).len();
                prop_assert!(
                    ext_count >= base_count,
                    "adding mapping reduced conflicts: {base_count} -> {ext_count}"
                );
            }

            /// Property: validate_extended never panics on arbitrary valid task definitions.
            #[test]
            fn validate_extended_never_panics(
                family in "[a-zA-Z][a-zA-Z0-9_-]{0,10}",
                n_containers in 1usize..=5usize,
            ) {
                let containers: Vec<_> = (0..n_containers)
                    .map(|i| crate::taskdef::ContainerDefinition {
                        name: format!("c{i}"),
                        image: "alpine:latest".into(),
                        ..Default::default()
                    })
                    .collect();
                let td = TaskDefinition {
                    family,
                    task_role_arn: None,
                    execution_role_arn: None,
                    volumes: vec![],
                    container_definitions: containers,
                };
                // Should not panic
                let _report = validate_extended(&td);
            }
        }
    }

    #[test]
    fn validate_extended_collects_all_issues() {
        let td = make_task_def(
            r#"{
            "family": "test",
            "containerDefinitions": [
                {
                    "name": "a",
                    "image": ".bad-image",
                    "essential": false,
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 8080 },
                        { "containerPort": 81, "hostPort": 8080 }
                    ],
                    "secrets": [
                        { "name": "S", "valueFrom": "not-an-arn" }
                    ]
                },
                {
                    "name": "b",
                    "image": "alpine:latest",
                    "essential": false
                }
            ]
        }"#,
        );
        let report = validate_extended(&td);
        // Image error + port conflict error + secret ARN warning + essential warning + no ports warning (only for "all")
        assert!(report.has_errors());
        assert!(report.error_count() >= 2); // image + port conflict
        assert!(report.warning_count() >= 1); // secret ARN at minimum
    }
}
