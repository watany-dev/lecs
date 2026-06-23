//! Terraform `show -json` output parser for ECS task definitions.
//!
//! Parses `terraform show -json` plan or state output and extracts
//! `aws_ecs_task_definition` resources, converting them into [`TaskDefinition`].

use std::path::Path;

use serde::Deserialize;

use super::{TaskDefError, TaskDefinition, Volume, VolumeHost};

/// Resource type identifier for ECS task definitions in Terraform.
const ECS_TASK_DEF_TYPE: &str = "aws_ecs_task_definition";

/// Maximum terraform JSON file size (10 MB).
const MAX_TERRAFORM_FILE_SIZE: u64 = 10 * 1024 * 1024;

// ── Terraform JSON schema types ──────────────────────────────────────

/// Top-level structure of `terraform show -json` output.
#[derive(Debug, Deserialize)]
struct TerraformShowJson {
    /// Present in plan output.
    planned_values: Option<PlannedValues>,
    /// Present in state output (no plan file argument).
    values: Option<StateValues>,
    /// Present in plan output; used as fallback.
    resource_changes: Option<Vec<ResourceChange>>,
}

/// Wrapper for `planned_values` in plan output.
#[derive(Debug, Deserialize)]
struct PlannedValues {
    root_module: Module,
}

/// Wrapper for `values` in state output.
#[derive(Debug, Deserialize)]
struct StateValues {
    root_module: Module,
}

/// A Terraform module containing resources and optional child modules.
#[derive(Debug, Deserialize)]
struct Module {
    #[serde(default)]
    resources: Vec<Resource>,
    #[serde(default)]
    child_modules: Vec<Self>,
}

/// A Terraform resource with its evaluated attribute values.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct Resource {
    /// Full resource address (e.g. `module.ecs.aws_ecs_task_definition.app`).
    address: String,
    /// Resource type (e.g. `aws_ecs_task_definition`).
    #[serde(rename = "type")]
    resource_type: String,
    /// Evaluated attribute values.
    values: serde_json::Value,
}

/// An entry in `resource_changes` from plan output.
#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct ResourceChange {
    address: String,
    #[serde(rename = "type")]
    resource_type: String,
    change: Change,
}

/// The change block inside a `ResourceChange`.
#[derive(Debug, Deserialize)]
struct Change {
    /// The resource attributes after the change is applied.
    /// `None` for destroy actions.
    after: Option<serde_json::Value>,
}

/// Terraform volume block (`snake_case`, singular).
#[derive(Debug, Deserialize)]
struct TerraformVolume {
    name: String,
    #[serde(default)]
    host_path: Option<String>,
}

// ── Public API ───────────────────────────────────────────────────────

/// Parse a Terraform `show -json` file and extract an ECS task definition.
///
/// If the file contains multiple `aws_ecs_task_definition` resources,
/// `resource_address` must be provided to select one.
pub fn from_terraform_file(
    path: &Path,
    resource_address: Option<&str>,
) -> Result<TaskDefinition, TaskDefError> {
    let metadata = std::fs::metadata(path).map_err(|source| TaskDefError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_TERRAFORM_FILE_SIZE {
        return Err(TaskDefError::FileTooLarge {
            path: path.to_path_buf(),
            size: metadata.len(),
            max: MAX_TERRAFORM_FILE_SIZE,
        });
    }
    let content = std::fs::read_to_string(path).map_err(|source| TaskDefError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    from_terraform_json(&content, resource_address)
}

/// Parse a Terraform `show -json` string and extract an ECS task definition.
///
/// If the JSON contains multiple `aws_ecs_task_definition` resources,
/// `resource_address` must be provided to select one.
pub fn from_terraform_json(
    json: &str,
    resource_address: Option<&str>,
) -> Result<TaskDefinition, TaskDefError> {
    let tf: TerraformShowJson =
        serde_json::from_str(json).map_err(|e| TaskDefError::ParseTerraformJson(e.to_string()))?;

    let resources = collect_ecs_resources(&tf)?;

    let (address, values) = select_resource(&resources, resource_address)?;

    convert_to_task_definition(address, values)
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Collected ECS resource: (address, attribute values).
type EcsResource<'a> = (&'a str, &'a serde_json::Value);

/// Collect all `aws_ecs_task_definition` resources from the Terraform JSON,
/// using the following priority:
/// 1. `planned_values.root_module` (plan output)
/// 2. `values.root_module` (state output)
/// 3. `resource_changes[].change.after` (fallback)
fn collect_ecs_resources(tf: &TerraformShowJson) -> Result<Vec<EcsResource<'_>>, TaskDefError> {
    // Try planned_values first (plan output).
    if let Some(pv) = &tf.planned_values {
        let resources = collect_from_module(&pv.root_module);
        if !resources.is_empty() {
            return Ok(resources);
        }
    }

    // Try values (state output).
    if let Some(sv) = &tf.values {
        let resources = collect_from_module(&sv.root_module);
        if !resources.is_empty() {
            return Ok(resources);
        }
    }

    // Fallback: resource_changes.
    if let Some(changes) = &tf.resource_changes {
        let resources: Vec<EcsResource<'_>> = changes
            .iter()
            .filter(|rc| rc.resource_type == ECS_TASK_DEF_TYPE)
            .filter_map(|rc| rc.change.after.as_ref().map(|v| (rc.address.as_str(), v)))
            .collect();
        if !resources.is_empty() {
            return Ok(resources);
        }
    }

    Err(TaskDefError::TerraformNoEcsResource)
}

/// Recursively collect ECS task definition resources from a module.
fn collect_from_module(module: &Module) -> Vec<EcsResource<'_>> {
    let mut results = Vec::new();
    for r in &module.resources {
        if r.resource_type == ECS_TASK_DEF_TYPE {
            results.push((r.address.as_str(), &r.values));
        }
    }
    for child in &module.child_modules {
        results.extend(collect_from_module(child));
    }
    results
}

/// Select a single resource from the collected list.
fn select_resource<'a>(
    resources: &[EcsResource<'a>],
    resource_address: Option<&str>,
) -> Result<(&'a str, &'a serde_json::Value), TaskDefError> {
    match resource_address {
        Some(addr) => {
            for &(address, values) in resources {
                if address == addr {
                    return Ok((address, values));
                }
            }
            Err(TaskDefError::TerraformResourceNotFound(addr.to_string()))
        }
        None => {
            if resources.len() == 1 {
                Ok(resources[0])
            } else {
                let addresses: Vec<String> =
                    resources.iter().map(|(a, _)| (*a).to_string()).collect();
                Err(TaskDefError::TerraformMultipleResources {
                    resources: addresses,
                })
            }
        }
    }
}

/// Convert Terraform resource values to a `TaskDefinition`.
fn convert_to_task_definition(
    _address: &str,
    values: &serde_json::Value,
) -> Result<TaskDefinition, TaskDefError> {
    let family = values
        .get("family")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            TaskDefError::ParseTerraformJson("missing 'family' field in ECS resource".to_string())
        })?
        .to_string();

    let task_role_arn = values
        .get("task_role_arn")
        .and_then(serde_json::Value::as_str)
        .map(String::from);

    let execution_role_arn = values
        .get("execution_role_arn")
        .and_then(serde_json::Value::as_str)
        .map(String::from);

    // container_definitions is a JSON-encoded string in Terraform output.
    let container_defs_str = values
        .get("container_definitions")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            TaskDefError::ParseTerraformJson(
                "missing 'container_definitions' field in ECS resource".to_string(),
            )
        })?;

    // Double-deserialize: the inner JSON string uses ECS API camelCase format.
    let container_definitions = serde_json::from_str(container_defs_str).map_err(|e| {
        TaskDefError::ParseTerraformJson(format!(
            "failed to parse container_definitions JSON string: {e}"
        ))
    })?;

    // Convert Terraform's `volume` (singular, snake_case) to `volumes`.
    let volumes = convert_volumes(values);

    let task_def = TaskDefinition {
        family,
        task_role_arn,
        execution_role_arn,
        volumes,
        container_definitions,
    };

    task_def.validate()?;
    Ok(task_def)
}

/// Convert Terraform volume blocks to Lecs `Volume` structs.
///
/// Volumes with `host_path` are converted to bind mounts; volumes without
/// `host_path` (e.g. Docker-managed or EFS) are included with `host: None`.
/// Unparseable volume entries are skipped with a tracing warning.
fn convert_volumes(values: &serde_json::Value) -> Vec<Volume> {
    let Some(volume_arr) = values.get("volume").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    let mut volumes = Vec::new();
    for v in volume_arr {
        let Ok(tf_vol) = serde_json::from_value::<TerraformVolume>(v.clone()) else {
            tracing::warn!("skipping unparseable volume entry in Terraform JSON");
            continue;
        };

        let host = tf_vol.host_path.map(|p| VolumeHost { source_path: p });
        volumes.push(Volume {
            name: tf_vol.name,
            host,
        });
    }
    volumes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_plan_json(resources_json: &str) -> String {
        format!(
            r#"{{
  "format_version": "1.0",
  "terraform_version": "1.6.0",
  "planned_values": {{
    "root_module": {{
      "resources": [{resources_json}]
    }}
  }}
}}"#
        )
    }

    fn make_single_ecs_resource() -> String {
        make_plan_json(&format!(
            r#"{{
      "address": "aws_ecs_task_definition.app",
      "type": "aws_ecs_task_definition",
      "values": {{
        "family": "my-task",
        "task_role_arn": "arn:aws:iam::123456789012:role/task-role",
        "execution_role_arn": "arn:aws:iam::123456789012:role/exec-role",
        "container_definitions": {container_defs},
        "volume": [
          {{
            "name": "data",
            "host_path": "/host/data"
          }}
        ]
      }}
    }}"#,
            container_defs = serde_json::to_string(
                &serde_json::json!([{
                    "name": "app",
                    "image": "nginx:latest",
                    "essential": true,
                    "portMappings": [{"containerPort": 80, "hostPort": 8080, "protocol": "tcp"}],
                    "environment": [{"name": "ENV", "value": "prod"}],
                    "cpu": 256,
                    "memory": 512
                }])
                .to_string()
            )
            .unwrap()
        ))
    }

    #[test]
    fn parse_single_resource_from_plan() {
        let json = make_single_ecs_resource();
        let td = from_terraform_json(&json, None).expect("should parse");
        assert_eq!(td.family, "my-task");
        assert_eq!(td.container_definitions.len(), 1);
        assert_eq!(td.container_definitions[0].name, "app");
        assert_eq!(td.container_definitions[0].image, "nginx:latest");
        assert_eq!(
            td.task_role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/task-role")
        );
        assert_eq!(
            td.execution_role_arn.as_deref(),
            Some("arn:aws:iam::123456789012:role/exec-role")
        );
    }

    #[test]
    fn parse_volumes_from_plan() {
        let json = make_single_ecs_resource();
        let td = from_terraform_json(&json, None).expect("should parse");
        assert_eq!(td.volumes.len(), 1);
        assert_eq!(td.volumes[0].name, "data");
        assert_eq!(
            td.volumes[0].host.as_ref().map(|h| h.source_path.as_str()),
            Some("/host/data")
        );
    }

    #[test]
    fn parse_with_tf_resource_selection() {
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "planned_values": {{
    "root_module": {{
      "resources": [
        {{
          "address": "aws_ecs_task_definition.web",
          "type": "aws_ecs_task_definition",
          "values": {{
            "family": "web-task",
            "container_definitions": {cd1}
          }}
        }},
        {{
          "address": "aws_ecs_task_definition.worker",
          "type": "aws_ecs_task_definition",
          "values": {{
            "family": "worker-task",
            "container_definitions": {cd2}
          }}
        }}
      ]
    }}
  }}
}}"#,
            cd1 = serde_json::to_string(
                &serde_json::json!([{"name": "web", "image": "web:latest", "essential": true}])
                    .to_string()
            )
            .unwrap(),
            cd2 = serde_json::to_string(
                &serde_json::json!(
                    [{"name": "worker", "image": "worker:latest", "essential": true}]
                )
                .to_string()
            )
            .unwrap(),
        );

        let td = from_terraform_json(&json, Some("aws_ecs_task_definition.worker"))
            .expect("should select worker");
        assert_eq!(td.family, "worker-task");
        assert_eq!(td.container_definitions[0].name, "worker");
    }

    #[test]
    fn error_multiple_resources_without_selector() {
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "planned_values": {{
    "root_module": {{
      "resources": [
        {{
          "address": "aws_ecs_task_definition.web",
          "type": "aws_ecs_task_definition",
          "values": {{
            "family": "web",
            "container_definitions": {cd}
          }}
        }},
        {{
          "address": "aws_ecs_task_definition.worker",
          "type": "aws_ecs_task_definition",
          "values": {{
            "family": "worker",
            "container_definitions": {cd}
          }}
        }}
      ]
    }}
  }}
}}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "app", "image": "img:latest", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        );

        let err = from_terraform_json(&json, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("multiple"),
            "expected 'multiple' in error: {msg}"
        );
        assert!(msg.contains("aws_ecs_task_definition.web"));
        assert!(msg.contains("aws_ecs_task_definition.worker"));
    }

    #[test]
    fn error_no_ecs_resource() {
        let json = make_plan_json(
            r#"{
      "address": "aws_s3_bucket.data",
      "type": "aws_s3_bucket",
      "values": { "bucket": "my-bucket" }
    }"#,
        );
        let err = from_terraform_json(&json, None).unwrap_err();
        assert!(
            err.to_string().contains("no aws_ecs_task_definition"),
            "got: {err}"
        );
    }

    #[test]
    fn error_resource_not_found() {
        let json = make_single_ecs_resource();
        let err =
            from_terraform_json(&json, Some("aws_ecs_task_definition.nonexistent")).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected 'not found' in: {err}"
        );
    }

    #[test]
    fn error_invalid_container_definitions_json() {
        let json = make_plan_json(
            r#"{
      "address": "aws_ecs_task_definition.app",
      "type": "aws_ecs_task_definition",
      "values": {
        "family": "my-task",
        "container_definitions": "not valid json ["
      }
    }"#,
        );
        let err = from_terraform_json(&json, None).unwrap_err();
        assert!(
            err.to_string().contains("container_definitions"),
            "got: {err}"
        );
    }

    #[test]
    fn error_invalid_terraform_json() {
        let err = from_terraform_json("{ invalid json", None).unwrap_err();
        assert!(
            err.to_string().contains("parse Terraform JSON"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_state_output() {
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "values": {{
    "root_module": {{
      "resources": [{{
        "address": "aws_ecs_task_definition.app",
        "type": "aws_ecs_task_definition",
        "values": {{
          "family": "state-task",
          "container_definitions": {cd}
        }}
      }}]
    }}
  }}
}}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "app", "image": "app:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        );

        let td = from_terraform_json(&json, None).expect("should parse state output");
        assert_eq!(td.family, "state-task");
    }

    #[test]
    fn parse_resource_changes_fallback() {
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "resource_changes": [{{
    "address": "aws_ecs_task_definition.app",
    "type": "aws_ecs_task_definition",
    "change": {{
      "actions": ["create"],
      "after": {{
        "family": "rc-task",
        "container_definitions": {cd}
      }}
    }}
  }}]
}}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "rc", "image": "rc:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        );

        let td = from_terraform_json(&json, None).expect("should parse from resource_changes");
        assert_eq!(td.family, "rc-task");
    }

    #[test]
    fn skip_destroy_in_resource_changes() {
        // Resource with after=null (destroy) should be skipped.
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "resource_changes": [
    {{
      "address": "aws_ecs_task_definition.old",
      "type": "aws_ecs_task_definition",
      "change": {{
        "actions": ["delete"],
        "after": null
      }}
    }},
    {{
      "address": "aws_ecs_task_definition.new",
      "type": "aws_ecs_task_definition",
      "change": {{
        "actions": ["create"],
        "after": {{
          "family": "new-task",
          "container_definitions": {cd}
        }}
      }}
    }}
  ]
}}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "app", "image": "app:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        );

        let td = from_terraform_json(&json, None).expect("should skip destroy resource");
        assert_eq!(td.family, "new-task");
    }

    #[test]
    fn parse_child_modules() {
        let json = format!(
            r#"{{
  "format_version": "1.0",
  "planned_values": {{
    "root_module": {{
      "resources": [],
      "child_modules": [{{
        "resources": [{{
          "address": "module.ecs.aws_ecs_task_definition.app",
          "type": "aws_ecs_task_definition",
          "values": {{
            "family": "nested-task",
            "container_definitions": {cd}
          }}
        }}],
        "child_modules": []
      }}]
    }}
  }}
}}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "nested", "image": "n:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        );

        let td = from_terraform_json(&json, None).expect("should find resource in child module");
        assert_eq!(td.family, "nested-task");
        assert_eq!(td.container_definitions[0].name, "nested");
    }

    #[test]
    fn volume_without_host_path() {
        let json = make_plan_json(&format!(
            r#"{{
      "address": "aws_ecs_task_definition.app",
      "type": "aws_ecs_task_definition",
      "values": {{
        "family": "vol-task",
        "container_definitions": {cd},
        "volume": [
          {{ "name": "docker-vol" }},
          {{ "name": "bind-vol", "host_path": "/data" }}
        ]
      }}
    }}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "app", "image": "app:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        ));

        let td = from_terraform_json(&json, None).expect("should parse volumes");
        assert_eq!(td.volumes.len(), 2);
        assert_eq!(td.volumes[0].name, "docker-vol");
        assert!(td.volumes[0].host.is_none());
        assert_eq!(td.volumes[1].name, "bind-vol");
        assert_eq!(
            td.volumes[1].host.as_ref().map(|h| h.source_path.as_str()),
            Some("/data")
        );
    }

    #[test]
    fn missing_family_field() {
        let json = make_plan_json(&format!(
            r#"{{
      "address": "aws_ecs_task_definition.app",
      "type": "aws_ecs_task_definition",
      "values": {{
        "container_definitions": {cd}
      }}
    }}"#,
            cd = serde_json::to_string(
                &serde_json::json!([{"name": "app", "image": "app:v1", "essential": true}])
                    .to_string()
            )
            .unwrap(),
        ));

        let err = from_terraform_json(&json, None).unwrap_err();
        assert!(
            err.to_string().contains("family"),
            "expected 'family' in error: {err}"
        );
    }

    #[test]
    fn missing_container_definitions_field() {
        let json = make_plan_json(
            r#"{
      "address": "aws_ecs_task_definition.app",
      "type": "aws_ecs_task_definition",
      "values": {
        "family": "my-task"
      }
    }"#,
        );

        let err = from_terraform_json(&json, None).unwrap_err();
        assert!(
            err.to_string().contains("container_definitions"),
            "expected 'container_definitions' in error: {err}"
        );
    }

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;

        fn arb_family() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_-]{1,20}"
        }

        fn arb_container_name() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_-]{1,20}"
        }

        fn arb_image() -> impl Strategy<Value = String> {
            "[a-z][a-z0-9]{0,10}:[a-z0-9][a-z0-9.]{0,8}"
        }

        fn arb_address() -> impl Strategy<Value = String> {
            "aws_ecs_task_definition\\.[a-z][a-z0-9_]{0,10}"
        }

        type TfResource = (String, String, Vec<(String, String)>);

        /// Build a Terraform plan JSON with the given resources (address, family, container names).
        fn build_tf_json(resources: &[TfResource]) -> String {
            let res_json: Vec<serde_json::Value> = resources
                .iter()
                .map(|(address, family, containers)| {
                    let cds: Vec<serde_json::Value> = containers
                        .iter()
                        .map(|(name, image)| {
                            serde_json::json!({
                                "name": name,
                                "image": image,
                                "essential": true,
                            })
                        })
                        .collect();
                    let cd_string = serde_json::Value::Array(cds).to_string();
                    serde_json::json!({
                        "address": address,
                        "type": "aws_ecs_task_definition",
                        "values": {
                            "family": family,
                            "container_definitions": cd_string,
                        }
                    })
                })
                .collect();
            serde_json::json!({
                "format_version": "1.0",
                "planned_values": {
                    "root_module": { "resources": res_json }
                }
            })
            .to_string()
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(50))]

            /// Property: A well-formed Terraform plan with one ECS resource parses OK
            /// and preserves family and container count/names.
            #[test]
            fn terraform_planned_values_parse_ok(
                family in arb_family(),
                address in arb_address(),
                containers in proptest::collection::vec(
                    (arb_container_name(), arb_image()),
                    1..=4,
                ),
            ) {
                // Deduplicate container names.
                let mut seen = std::collections::HashSet::new();
                let unique: Vec<_> = containers
                    .into_iter()
                    .filter(|(n, _)| seen.insert(n.clone()))
                    .collect();
                prop_assume!(!unique.is_empty());

                let json = build_tf_json(&[(address, family.clone(), unique.clone())]);
                let td = from_terraform_json(&json, None).expect("parses");
                prop_assert_eq!(td.family, family);
                prop_assert_eq!(td.container_definitions.len(), unique.len());
                for (i, (name, _)) in unique.iter().enumerate() {
                    prop_assert_eq!(&td.container_definitions[i].name, name);
                }
            }

            /// Property: When multiple resources exist, address selection returns
            /// the correct one identified by family.
            #[test]
            fn terraform_resource_address_selects_correct_one(
                fam_a in arb_family(),
                fam_b in arb_family(),
                container_name in arb_container_name(),
                image in arb_image(),
                addr_suffix_a in "[a-z][a-z0-9_]{0,8}",
                addr_suffix_b in "[a-z][a-z0-9_]{0,8}",
            ) {
                prop_assume!(addr_suffix_a != addr_suffix_b);
                let addr_a = format!("aws_ecs_task_definition.{addr_suffix_a}");
                let addr_b = format!("aws_ecs_task_definition.{addr_suffix_b}");
                let cds = vec![(container_name, image)];
                let json = build_tf_json(&[
                    (addr_a.clone(), fam_a.clone(), cds.clone()),
                    (addr_b.clone(), fam_b.clone(), cds),
                ]);

                let td_a = from_terraform_json(&json, Some(&addr_a)).expect("selects a");
                prop_assert_eq!(td_a.family, fam_a);

                let td_b = from_terraform_json(&json, Some(&addr_b)).expect("selects b");
                prop_assert_eq!(td_b.family, fam_b);

                // Missing selector should error with 'multiple'.
                let err = from_terraform_json(&json, None).expect_err("expected multiple err");
                let is_multiple = matches!(err, TaskDefError::TerraformMultipleResources { .. });
                prop_assert!(is_multiple);
            }

            /// Property: An unknown address selector always yields
            /// TerraformResourceNotFound, regardless of resource content.
            #[test]
            fn terraform_unknown_address_always_not_found(
                family in arb_family(),
                address in arb_address(),
                container_name in arb_container_name(),
                image in arb_image(),
                bogus_suffix in "[a-z][a-z0-9_]{0,8}",
            ) {
                let bogus = format!("aws_ecs_task_definition.xx_bogus_{bogus_suffix}");
                prop_assume!(bogus != address);

                let json = build_tf_json(&[(
                    address,
                    family,
                    vec![(container_name, image)],
                )]);
                let err = from_terraform_json(&json, Some(&bogus)).expect_err("not found");
                prop_assert!(matches!(err, TaskDefError::TerraformResourceNotFound(_)));
            }
        }
    }
}
