//! `CloudFormation` template parser for ECS task definitions.
//!
//! Parses `CloudFormation` template JSON (including CDK synthesized output)
//! and extracts `AWS::ECS::TaskDefinition` resources, converting them
//! into [`TaskDefinition`].

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{TaskDefError, TaskDefinition};

/// Resource type identifier for ECS task definitions in `CloudFormation`.
const CFN_ECS_TASK_DEF_TYPE: &str = "AWS::ECS::TaskDefinition";

/// Maximum `CloudFormation` template file size (10 MB).
const MAX_CFN_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Known `CloudFormation` intrinsic function keys.
const INTRINSIC_FUNCTION_KEYS: &[&str] = &[
    "Ref",
    "Fn::Sub",
    "Fn::Join",
    "Fn::GetAtt",
    "Fn::ImportValue",
    "Fn::Select",
    "Fn::Split",
    "Fn::If",
    "Fn::Base64",
    "Fn::Cidr",
    "Fn::FindInMap",
    "Fn::GetAZs",
    "Fn::Transform",
];

// ── CloudFormation JSON schema types ────────────────────────────────

/// Top-level structure of a `CloudFormation` template.
#[derive(Debug, Deserialize)]
struct CfnTemplate {
    /// `CloudFormation` resources.
    #[serde(rename = "Resources")]
    resources: Option<HashMap<String, CfnResource>>,
}

/// A `CloudFormation` resource.
#[derive(Debug, Deserialize)]
struct CfnResource {
    /// Resource type (e.g. `AWS::ECS::TaskDefinition`).
    #[serde(rename = "Type")]
    resource_type: String,
    /// Resource properties as raw JSON (`PascalCase` keys).
    #[serde(rename = "Properties")]
    properties: Option<Value>,
}

// ── Public API ───────────────────────────────────────────────────────

/// Parse a `CloudFormation` template file and extract an ECS task definition.
///
/// If the template contains multiple `AWS::ECS::TaskDefinition` resources,
/// `resource_id` must be provided to select one by logical ID.
pub fn from_cfn_file(
    path: &Path,
    resource_id: Option<&str>,
) -> Result<TaskDefinition, TaskDefError> {
    let metadata = std::fs::metadata(path).map_err(|source| TaskDefError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_CFN_FILE_SIZE {
        return Err(TaskDefError::FileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            max: MAX_CFN_FILE_SIZE,
        });
    }
    let content = std::fs::read_to_string(path).map_err(|source| TaskDefError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    from_cfn_json(&content, resource_id)
}

/// Parse a `CloudFormation` template JSON string and extract an ECS task definition.
///
/// If the template contains multiple `AWS::ECS::TaskDefinition` resources,
/// `resource_id` must be provided to select one by logical ID.
pub fn from_cfn_json(
    json: &str,
    resource_id: Option<&str>,
) -> Result<TaskDefinition, TaskDefError> {
    let template: CfnTemplate =
        serde_json::from_str(json).map_err(|e| TaskDefError::ParseCfnJson(e.to_string()))?;

    let resources = collect_ecs_resources(&template)?;

    let (logical_id, properties) = select_resource(&resources, resource_id)?;

    // Check for intrinsic functions before converting keys.
    detect_intrinsic_functions(properties, logical_id)?;

    // Convert PascalCase keys to camelCase for compatibility with existing types.
    let camel_value = convert_keys_to_camel_case(properties.clone());

    let task_def: TaskDefinition = serde_json::from_value(camel_value).map_err(|e| {
        TaskDefError::ParseCfnJson(format!(
            "failed to deserialize CloudFormation properties for '{logical_id}': {e}"
        ))
    })?;

    task_def.validate()?;
    Ok(task_def)
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Collected ECS resource: (`logical_id`, properties).
type EcsResource<'a> = (&'a str, &'a Value);

/// Collect all `AWS::ECS::TaskDefinition` resources from the template.
fn collect_ecs_resources(template: &CfnTemplate) -> Result<Vec<EcsResource<'_>>, TaskDefError> {
    let resources = template
        .resources
        .as_ref()
        .ok_or(TaskDefError::CfnNoEcsResource)?;

    let ecs_resources: Vec<EcsResource<'_>> = resources
        .iter()
        .filter(|(_, r)| r.resource_type == CFN_ECS_TASK_DEF_TYPE)
        .filter_map(|(id, r)| r.properties.as_ref().map(|p| (id.as_str(), p)))
        .collect();

    if ecs_resources.is_empty() {
        return Err(TaskDefError::CfnNoEcsResource);
    }

    Ok(ecs_resources)
}

/// Select a single resource from the collected list.
fn select_resource<'a>(
    resources: &[EcsResource<'a>],
    resource_id: Option<&str>,
) -> Result<EcsResource<'a>, TaskDefError> {
    match resource_id {
        Some(id) => {
            for &(logical_id, properties) in resources {
                if logical_id == id {
                    return Ok((logical_id, properties));
                }
            }
            Err(TaskDefError::CfnResourceNotFound(id.to_string()))
        }
        None => {
            if resources.len() == 1 {
                Ok(resources[0])
            } else {
                let mut ids: Vec<String> =
                    resources.iter().map(|(id, _)| (*id).to_string()).collect();
                ids.sort();
                Err(TaskDefError::CfnMultipleResources { resources: ids })
            }
        }
    }
}

/// Recursively detect `CloudFormation` intrinsic functions in a JSON value.
///
/// This must be called **before** converting keys to camelCase to avoid
/// false positives from user data.
fn detect_intrinsic_functions(value: &Value, context: &str) -> Result<(), TaskDefError> {
    match value {
        Value::Object(map) => {
            // Detect intrinsics by object shape: a single-key map whose key is an intrinsic.
            // This avoids false positives from user-defined maps that happen to contain
            // keys like "Ref" alongside other entries.
            if map.len() == 1
                && let Some(key) = map.keys().next()
                && INTRINSIC_FUNCTION_KEYS.contains(&key.as_str())
            {
                return Err(TaskDefError::CfnIntrinsicFunction {
                    field: context.to_string(),
                    detail: format!(
                        "'{key}' is a CloudFormation intrinsic function and cannot be resolved in ECS task-definition properties. Provide a template where task-definition properties are concrete values (no CloudFormation intrinsics)."
                    ),
                });
            }
            for (key, v) in map {
                detect_intrinsic_functions(v, &format!("{context}.{key}"))?;
            }
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                detect_intrinsic_functions(v, &format!("{context}[{i}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Convert a single `PascalCase` string to `camelCase`.
///
/// Only lowercases the first character. This is sufficient for all
/// ECS `TaskDefinition` fields (e.g. `ContainerPort` → `containerPort`).
fn pascal_to_camel(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |c| {
        let lower: String = c.to_lowercase().collect();
        lower + chars.as_str()
    })
}

/// Recursively convert all object keys from `PascalCase` to `camelCase`.
fn convert_keys_to_camel_case(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let converted = map
                .into_iter()
                .map(|(k, v)| (pascal_to_camel(&k), convert_keys_to_camel_case(v)))
                .collect();
            Value::Object(converted)
        }
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(convert_keys_to_camel_case).collect())
        }
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn minimal_template() -> String {
        r#"{
            "AWSTemplateFormatVersion": "2010-09-09",
            "Resources": {
                "MyTaskDef": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "Essential": true
                            }
                        ]
                    }
                }
            }
        }"#
        .to_string()
    }

    fn full_template() -> String {
        r#"{
            "AWSTemplateFormatVersion": "2010-09-09",
            "Resources": {
                "MyTaskDef": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "TaskRoleArn": "arn:aws:iam::123456789012:role/task-role",
                        "ExecutionRoleArn": "arn:aws:iam::123456789012:role/exec-role",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "Essential": true,
                                "Command": ["nginx", "-g", "daemon off;"],
                                "EntryPoint": ["/docker-entrypoint.sh"],
                                "Environment": [
                                    { "Name": "ENV_VAR", "Value": "some-value" }
                                ],
                                "PortMappings": [
                                    { "ContainerPort": 80, "HostPort": 8080, "Protocol": "tcp" }
                                ],
                                "Cpu": 256,
                                "Memory": 512,
                                "MemoryReservation": 256
                            }
                        ]
                    }
                }
            }
        }"#
        .to_string()
    }

    #[test]
    fn parse_single_resource() {
        let td = from_cfn_json(&full_template(), None).unwrap();
        assert_eq!(td.family, "my-app");
        assert_eq!(td.container_definitions.len(), 1);
        let c = &td.container_definitions[0];
        assert_eq!(c.name, "app");
        assert_eq!(c.image, "nginx:latest");
        assert!(c.essential);
        assert_eq!(c.command, vec!["nginx", "-g", "daemon off;"]);
        assert_eq!(c.entry_point, vec!["/docker-entrypoint.sh"]);
        assert_eq!(c.environment.len(), 1);
        assert_eq!(c.environment[0].name, "ENV_VAR");
        assert_eq!(c.environment[0].value, "some-value");
        assert_eq!(c.port_mappings.len(), 1);
        assert_eq!(c.port_mappings[0].container_port, 80);
        assert_eq!(c.port_mappings[0].host_port, Some(8080));
        assert_eq!(c.port_mappings[0].protocol, "tcp");
        assert_eq!(c.cpu, Some(256));
        assert_eq!(c.memory, Some(512));
        assert_eq!(c.memory_reservation, Some(256));
    }

    #[test]
    fn parse_minimal_template() {
        let td = from_cfn_json(&minimal_template(), None).unwrap();
        assert_eq!(td.family, "my-app");
        assert_eq!(td.container_definitions.len(), 1);
        assert_eq!(td.container_definitions[0].name, "app");
        assert!(td.volumes.is_empty());
    }

    #[test]
    fn parse_with_volumes() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "vol-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "MountPoints": [
                                    {
                                        "SourceVolume": "data",
                                        "ContainerPath": "/data",
                                        "ReadOnly": false
                                    }
                                ]
                            }
                        ],
                        "Volumes": [
                            {
                                "Name": "data",
                                "Host": {
                                    "SourcePath": "/host/data"
                                }
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        assert_eq!(td.volumes.len(), 1);
        assert_eq!(td.volumes[0].name, "data");
        let host = td.volumes[0].host.as_ref().unwrap();
        assert_eq!(host.source_path, "/host/data");
        assert_eq!(td.container_definitions[0].mount_points.len(), 1);
        assert_eq!(
            td.container_definitions[0].mount_points[0].source_volume,
            "data"
        );
        assert_eq!(
            td.container_definitions[0].mount_points[0].container_path,
            "/data"
        );
        assert!(!td.container_definitions[0].mount_points[0].read_only);
    }

    #[test]
    fn parse_with_health_check() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "hc-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "HealthCheck": {
                                    "Command": ["CMD-SHELL", "curl -f http://localhost/"],
                                    "Interval": 30,
                                    "Timeout": 5,
                                    "Retries": 3,
                                    "StartPeriod": 10
                                }
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        let hc = td.container_definitions[0].health_check.as_ref().unwrap();
        assert_eq!(hc.command, vec!["CMD-SHELL", "curl -f http://localhost/"]);
        assert_eq!(hc.interval, 30);
        assert_eq!(hc.timeout, 5);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period, 10);
    }

    #[test]
    fn parse_with_depends_on() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "dep-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "db",
                                "Image": "postgres:15",
                                "Essential": true,
                                "HealthCheck": {
                                    "Command": ["CMD-SHELL", "pg_isready"]
                                }
                            },
                            {
                                "Name": "app",
                                "Image": "myapp:latest",
                                "Essential": true,
                                "DependsOn": [
                                    {
                                        "ContainerName": "db",
                                        "Condition": "HEALTHY"
                                    }
                                ]
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        assert_eq!(td.container_definitions.len(), 2);
        let app = &td.container_definitions[1];
        assert_eq!(app.depends_on.len(), 1);
        assert_eq!(app.depends_on[0].container_name, "db");
    }

    #[test]
    fn parse_with_secrets() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "sec-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "myapp:latest",
                                "Secrets": [
                                    {
                                        "Name": "DB_PASSWORD",
                                        "ValueFrom": "arn:aws:secretsmanager:us-east-1:123456789012:secret:prod/db-pass"
                                    }
                                ]
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        assert_eq!(td.container_definitions[0].secrets.len(), 1);
        assert_eq!(td.container_definitions[0].secrets[0].name, "DB_PASSWORD");
        assert_eq!(
            td.container_definitions[0].secrets[0].value_from,
            "arn:aws:secretsmanager:us-east-1:123456789012:secret:prod/db-pass"
        );
    }

    #[test]
    fn parse_with_port_mappings() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "port-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "myapp:latest",
                                "PortMappings": [
                                    { "ContainerPort": 8080 },
                                    { "ContainerPort": 443, "HostPort": 8443, "Protocol": "tcp" }
                                ]
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        let pm = &td.container_definitions[0].port_mappings;
        assert_eq!(pm.len(), 2);
        assert_eq!(pm[0].container_port, 8080);
        assert_eq!(pm[0].host_port, None);
        assert_eq!(pm[1].container_port, 443);
        assert_eq!(pm[1].host_port, Some(8443));
    }

    #[test]
    fn select_resource_by_id() {
        let json = r#"{
            "Resources": {
                "TaskA": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-a",
                        "ContainerDefinitions": [
                            { "Name": "a", "Image": "a:latest" }
                        ]
                    }
                },
                "TaskB": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-b",
                        "ContainerDefinitions": [
                            { "Name": "b", "Image": "b:latest" }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, Some("TaskB")).unwrap();
        assert_eq!(td.family, "app-b");
    }

    #[test]
    fn error_multiple_resources() {
        let json = r#"{
            "Resources": {
                "TaskA": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-a",
                        "ContainerDefinitions": [
                            { "Name": "a", "Image": "a:latest" }
                        ]
                    }
                },
                "TaskB": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-b",
                        "ContainerDefinitions": [
                            { "Name": "b", "Image": "b:latest" }
                        ]
                    }
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("multiple"));
        assert!(msg.contains("--cfn-resource"));
    }

    #[test]
    fn error_no_ecs_resource() {
        let json = r#"{
            "Resources": {
                "MyBucket": {
                    "Type": "AWS::S3::Bucket",
                    "Properties": {}
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnNoEcsResource),
            "expected CfnNoEcsResource, got: {err}"
        );
    }

    #[test]
    fn error_resource_not_found() {
        let json = &minimal_template();
        let err = from_cfn_json(json, Some("NonExistent")).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnResourceNotFound(_)),
            "expected CfnResourceNotFound, got: {err}"
        );
    }

    #[test]
    fn error_intrinsic_ref() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "TaskRoleArn": { "Ref": "TaskRoleParam" },
                        "ContainerDefinitions": [
                            { "Name": "app", "Image": "nginx:latest" }
                        ]
                    }
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnIntrinsicFunction { .. }),
            "expected CfnIntrinsicFunction, got: {err}"
        );
        assert!(err.to_string().contains("Ref"));
    }

    #[test]
    fn error_intrinsic_fn_sub() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": { "Fn::Sub": "${AWS::AccountId}.dkr.ecr.${AWS::Region}.amazonaws.com/myapp:latest" }
                            }
                        ]
                    }
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnIntrinsicFunction { .. }),
            "expected CfnIntrinsicFunction, got: {err}"
        );
        assert!(err.to_string().contains("Fn::Sub"));
    }

    #[test]
    fn error_intrinsic_fn_join() {
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "Environment": [
                                    {
                                        "Name": "URL",
                                        "Value": { "Fn::Join": ["", ["https://", "example.com"]] }
                                    }
                                ]
                            }
                        ]
                    }
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnIntrinsicFunction { .. }),
            "expected CfnIntrinsicFunction, got: {err}"
        );
    }

    #[test]
    fn error_empty_resources() {
        let json = r#"{ "Resources": {} }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnNoEcsResource),
            "expected CfnNoEcsResource, got: {err}"
        );
    }

    #[test]
    fn error_invalid_json() {
        let err = from_cfn_json("not json", None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::ParseCfnJson(_)),
            "expected ParseCfnJson, got: {err}"
        );
    }

    #[test]
    fn error_file_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.json");
        // Write a file just over the limit.
        #[allow(clippy::cast_possible_truncation)]
        let content = "x".repeat((MAX_CFN_FILE_SIZE + 1) as usize);
        std::fs::write(&path, content).unwrap();
        let err = from_cfn_file(&path, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::FileTooLarge { .. }),
            "expected FileTooLarge, got: {err}"
        );
    }

    #[test]
    fn pascal_to_camel_conversion() {
        assert_eq!(pascal_to_camel("ContainerPort"), "containerPort");
        assert_eq!(pascal_to_camel("Family"), "family");
        assert_eq!(pascal_to_camel("HealthCheck"), "healthCheck");
        assert_eq!(pascal_to_camel("SourcePath"), "sourcePath");
        assert_eq!(pascal_to_camel(""), "");
        assert_eq!(pascal_to_camel("a"), "a");
        assert_eq!(pascal_to_camel("A"), "a");
    }

    #[test]
    fn ignore_unknown_resource_types() {
        let json = r#"{
            "Resources": {
                "MyBucket": {
                    "Type": "AWS::S3::Bucket",
                    "Properties": {}
                },
                "MyTask": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "ContainerDefinitions": [
                            { "Name": "app", "Image": "nginx:latest" }
                        ]
                    }
                },
                "MyQueue": {
                    "Type": "AWS::SQS::Queue",
                    "Properties": {}
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        assert_eq!(td.family, "my-app");
    }

    #[test]
    fn from_cfn_file_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("template.json");
        std::fs::write(&path, minimal_template()).unwrap();
        let td = from_cfn_file(&path, None).unwrap();
        assert_eq!(td.family, "my-app");
    }

    #[test]
    fn no_false_positive_intrinsic_in_multi_key_object() {
        // A user-defined map with "Ref" as one of multiple keys should NOT trigger
        // intrinsic detection (intrinsics are single-key objects).
        let json = r#"{
            "Resources": {
                "Task": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "my-app",
                        "ContainerDefinitions": [
                            {
                                "Name": "app",
                                "Image": "nginx:latest",
                                "DockerLabels": {
                                    "Ref": "some-label-value",
                                    "other": "data"
                                }
                            }
                        ]
                    }
                }
            }
        }"#;
        let td = from_cfn_json(json, None).unwrap();
        assert_eq!(td.family, "my-app");
    }

    #[test]
    fn error_multiple_resources_sorted() {
        let json = r#"{
            "Resources": {
                "ZetaTask": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-z",
                        "ContainerDefinitions": [
                            { "Name": "z", "Image": "z:latest" }
                        ]
                    }
                },
                "AlphaTask": {
                    "Type": "AWS::ECS::TaskDefinition",
                    "Properties": {
                        "Family": "app-a",
                        "ContainerDefinitions": [
                            { "Name": "a", "Image": "a:latest" }
                        ]
                    }
                }
            }
        }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        let msg = err.to_string();
        // IDs should be sorted alphabetically regardless of HashMap order
        let alpha_pos = msg.find("AlphaTask").expect("should contain AlphaTask");
        let zeta_pos = msg.find("ZetaTask").expect("should contain ZetaTask");
        assert!(
            alpha_pos < zeta_pos,
            "AlphaTask should appear before ZetaTask in sorted output"
        );
    }

    #[test]
    fn error_missing_resources_key() {
        let json = r#"{ "AWSTemplateFormatVersion": "2010-09-09" }"#;
        let err = from_cfn_json(json, None).unwrap_err();
        assert!(
            matches!(err, TaskDefError::CfnNoEcsResource),
            "expected CfnNoEcsResource, got: {err}"
        );
    }

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;

        fn arb_logical_id() -> impl Strategy<Value = String> {
            "[A-Z][A-Za-z0-9]{0,20}"
        }

        fn arb_family() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_-]{1,20}"
        }

        fn arb_container_name() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_-]{1,20}"
        }

        fn arb_image() -> impl Strategy<Value = String> {
            "[a-z][a-z0-9]{0,10}:[a-z0-9][a-z0-9.]{0,8}"
        }

        /// Build a `CloudFormation` template with one ECS task-definition resource.
        fn build_single(logical_id: &str, family: &str, containers: &[(String, String)]) -> String {
            let cds: Vec<Value> = containers
                .iter()
                .map(|(n, i)| {
                    serde_json::json!({
                        "Name": n,
                        "Image": i,
                        "Essential": true,
                    })
                })
                .collect();
            let template = serde_json::json!({
                "AWSTemplateFormatVersion": "2010-09-09",
                "Resources": {
                    logical_id: {
                        "Type": "AWS::ECS::TaskDefinition",
                        "Properties": {
                            "Family": family,
                            "ContainerDefinitions": cds,
                        }
                    }
                }
            });
            template.to_string()
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(50))]

            /// Property: A template with PascalCase keys parses OK and preserves
            /// family and container names/count.
            #[test]
            fn cfn_pascal_keys_parse_ok(
                logical_id in arb_logical_id(),
                family in arb_family(),
                containers in proptest::collection::vec(
                    (arb_container_name(), arb_image()),
                    1..=4,
                ),
            ) {
                // Deduplicate container names to satisfy ECS uniqueness validation.
                let mut seen = std::collections::HashSet::new();
                let unique: Vec<_> = containers
                    .into_iter()
                    .filter(|(n, _)| seen.insert(n.clone()))
                    .collect();
                prop_assume!(!unique.is_empty());

                let json = build_single(&logical_id, &family, &unique);
                let td = from_cfn_json(&json, None).expect("parses");
                prop_assert_eq!(td.family, family);
                prop_assert_eq!(td.container_definitions.len(), unique.len());
                let td_names: std::collections::HashSet<_> =
                    td.container_definitions.iter().map(|c| c.name.clone()).collect();
                let expected_names: std::collections::HashSet<_> =
                    unique.iter().map(|(n, _)| n.clone()).collect();
                prop_assert_eq!(td_names, expected_names);
            }

            /// Property: Injecting any known intrinsic function as a single-key
            /// object into the TaskRoleArn property always yields CfnIntrinsicFunction.
            #[test]
            fn cfn_intrinsic_function_always_rejected(
                logical_id in arb_logical_id(),
                family in arb_family(),
                container_name in arb_container_name(),
                image in arb_image(),
                intrinsic_key in prop::sample::select(vec![
                    "Ref", "Fn::Sub", "Fn::Join", "Fn::GetAtt", "Fn::ImportValue",
                    "Fn::Select", "Fn::Split", "Fn::If", "Fn::Base64",
                ]),
                arg_value in "[a-zA-Z0-9_-]{1,20}",
            ) {
                let template = serde_json::json!({
                    "Resources": {
                        logical_id: {
                            "Type": "AWS::ECS::TaskDefinition",
                            "Properties": {
                                "Family": family,
                                "TaskRoleArn": { intrinsic_key: arg_value },
                                "ContainerDefinitions": [{
                                    "Name": container_name,
                                    "Image": image,
                                    "Essential": true,
                                }]
                            }
                        }
                    }
                });
                let err = from_cfn_json(&template.to_string(), None).expect_err("intrinsic err");
                let is_intrinsic = matches!(err, TaskDefError::CfnIntrinsicFunction { .. });
                prop_assert!(is_intrinsic, "unexpected error: {err}");
                prop_assert!(err.to_string().contains(intrinsic_key));
            }

            /// Property: When multiple ECS resources exist and no id is given,
            /// CfnMultipleResources is returned; with a valid id, the right
            /// resource is selected. Unknown id yields CfnResourceNotFound.
            #[test]
            fn cfn_multiple_resources_requires_id(
                id_a in arb_logical_id(),
                id_b in arb_logical_id(),
                fam_a in arb_family(),
                fam_b in arb_family(),
                cname in arb_container_name(),
                image in arb_image(),
            ) {
                prop_assume!(id_a != id_b);
                let template = serde_json::json!({
                    "Resources": {
                        id_a.clone(): {
                            "Type": "AWS::ECS::TaskDefinition",
                            "Properties": {
                                "Family": fam_a,
                                "ContainerDefinitions": [{
                                    "Name": cname,
                                    "Image": image,
                                    "Essential": true,
                                }]
                            }
                        },
                        id_b.clone(): {
                            "Type": "AWS::ECS::TaskDefinition",
                            "Properties": {
                                "Family": fam_b,
                                "ContainerDefinitions": [{
                                    "Name": "second",
                                    "Image": "img:v2",
                                    "Essential": true,
                                }]
                            }
                        }
                    }
                });
                let json = template.to_string();

                // No id: multiple-resources error.
                let err = from_cfn_json(&json, None).expect_err("expected multiple err");
                let is_multiple = matches!(err, TaskDefError::CfnMultipleResources { .. });
                prop_assert!(is_multiple, "unexpected error: {err}");

                // Correct id: selects the expected resource.
                let td_b = from_cfn_json(&json, Some(&id_b)).expect("selects b");
                prop_assert_eq!(td_b.family, fam_b);

                // Unknown id: not-found error.
                let bogus = format!("Zz_{id_a}_{id_b}_XX");
                let err = from_cfn_json(&json, Some(&bogus)).expect_err("not found");
                let is_not_found = matches!(err, TaskDefError::CfnResourceNotFound(_));
                prop_assert!(is_not_found, "unexpected error: {err}");
            }

            /// Property: pascal_to_camel is idempotent on its own output
            /// (since the first character is already lowercase after one call).
            #[test]
            fn pascal_to_camel_idempotent(s in "[A-Za-z][A-Za-z0-9]{0,20}") {
                let once = pascal_to_camel(&s);
                let twice = pascal_to_camel(&once);
                prop_assert_eq!(once, twice);
            }
        }
    }
}
