//! Local override configuration.
//!
//! Applies local overrides (image tags, environment variables, port mappings)
//! to a parsed task definition without modifying the original file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::taskdef::{Environment, PortMapping, TaskDefinition};

/// Override configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum OverrideError {
    #[error("failed to read override file from {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse override JSON: {0}")]
    ParseJson(#[from] serde_json::Error),
}

/// Top-level override configuration (`lecs-override.json`).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideConfig {
    /// Per-container overrides keyed by container name.
    #[serde(default)]
    pub container_overrides: HashMap<String, ContainerOverride>,
}

/// Per-container override values.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerOverride {
    /// Replace the container image (including tag).
    pub image: Option<String>,

    /// Add or replace environment variables (key → value).
    pub environment: Option<HashMap<String, String>>,

    /// Replace all port mappings for this container.
    pub port_mappings: Option<Vec<PortMapping>>,
}

impl OverrideConfig {
    /// Load an override config from a file path.
    pub fn from_file(path: &Path) -> Result<Self, OverrideError> {
        let content = std::fs::read_to_string(path).map_err(|source| OverrideError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&content)
    }

    /// Parse an override config from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, OverrideError> {
        let config: Self = serde_json::from_str(json)?;
        Ok(config)
    }

    /// Apply overrides to a task definition in place.
    ///
    /// Unknown container names are logged as warnings and skipped.
    pub fn apply(&self, task_def: &mut TaskDefinition) {
        for (container_name, overrides) in &self.container_overrides {
            let Some(container) = task_def
                .container_definitions
                .iter_mut()
                .find(|c| c.name == *container_name)
            else {
                tracing::warn!(
                    container = %container_name,
                    "Override references unknown container, skipping"
                );
                continue;
            };

            // Image override
            if let Some(image) = &overrides.image {
                container.image.clone_from(image);
            }

            // Environment override (add or replace by key)
            if let Some(env_overrides) = &overrides.environment {
                for (key, value) in env_overrides {
                    if let Some(existing) =
                        container.environment.iter_mut().find(|e| e.name == *key)
                    {
                        existing.value.clone_from(value);
                    } else {
                        container.environment.push(Environment {
                            name: key.clone(),
                            value: value.clone(),
                        });
                    }
                }
            }

            // Port mappings override (full replacement)
            if let Some(port_mappings) = &overrides.port_mappings {
                container.port_mappings.clone_from(port_mappings);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::taskdef::{ContainerDefinition, Environment, PortMapping};

    fn sample_task_def() -> TaskDefinition {
        TaskDefinition {
            family: "test".to_string(),
            task_role_arn: None,
            execution_role_arn: None,
            volumes: vec![],
            container_definitions: vec![ContainerDefinition {
                name: "app".to_string(),
                image: "nginx:latest".to_string(),
                environment: vec![Environment {
                    name: "ENV_VAR".to_string(),
                    value: "original".to_string(),
                }],
                port_mappings: vec![PortMapping {
                    container_port: 80,
                    host_port: Some(8080),
                    protocol: "tcp".to_string(),
                }],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn parse_full_override() {
        let json = r#"{
            "containerOverrides": {
                "app": {
                    "image": "nginx:1.25-alpine",
                    "environment": {
                        "DEBUG": "true"
                    },
                    "portMappings": [
                        { "containerPort": 80, "hostPort": 9090 }
                    ]
                }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        assert_eq!(config.container_overrides.len(), 1);
        let app = &config.container_overrides["app"];
        assert_eq!(app.image.as_deref(), Some("nginx:1.25-alpine"));
        assert!(app.environment.is_some());
        assert!(app.port_mappings.is_some());
    }

    #[test]
    fn parse_empty_override() {
        let json = r#"{ "containerOverrides": {} }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        assert!(config.container_overrides.is_empty());
    }

    #[test]
    fn apply_replaces_image() {
        let json = r#"{
            "containerOverrides": {
                "app": { "image": "nginx:1.25-alpine" }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        config.apply(&mut task_def);

        assert_eq!(task_def.container_definitions[0].image, "nginx:1.25-alpine");
    }

    #[test]
    fn apply_adds_new_env_var() {
        let json = r#"{
            "containerOverrides": {
                "app": { "environment": { "NEW_VAR": "new-value" } }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        config.apply(&mut task_def);

        let env = &task_def.container_definitions[0].environment;
        assert_eq!(env.len(), 2);
        assert!(
            env.iter()
                .any(|e| e.name == "NEW_VAR" && e.value == "new-value")
        );
        // Original env var preserved
        assert!(
            env.iter()
                .any(|e| e.name == "ENV_VAR" && e.value == "original")
        );
    }

    #[test]
    fn apply_replaces_existing_env_var() {
        let json = r#"{
            "containerOverrides": {
                "app": { "environment": { "ENV_VAR": "overridden" } }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        config.apply(&mut task_def);

        let env = &task_def.container_definitions[0].environment;
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "ENV_VAR");
        assert_eq!(env[0].value, "overridden");
    }

    #[test]
    fn apply_replaces_port_mappings() {
        let json = r#"{
            "containerOverrides": {
                "app": {
                    "portMappings": [
                        { "containerPort": 8080, "hostPort": 9090, "protocol": "tcp" },
                        { "containerPort": 443 }
                    ]
                }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        config.apply(&mut task_def);

        let ports = &task_def.container_definitions[0].port_mappings;
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].container_port, 8080);
        assert_eq!(ports[0].host_port, Some(9090));
        assert_eq!(ports[1].container_port, 443);
        assert_eq!(ports[1].host_port, None);
    }

    #[test]
    fn apply_unknown_container_skips() {
        let json = r#"{
            "containerOverrides": {
                "nonexistent": { "image": "foo:bar" }
            }
        }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        let original_image = task_def.container_definitions[0].image.clone();
        config.apply(&mut task_def);

        // Original task def unchanged
        assert_eq!(task_def.container_definitions[0].image, original_image);
    }

    #[test]
    fn apply_no_mutation_when_empty() {
        let json = r#"{ "containerOverrides": {} }"#;
        let config = OverrideConfig::from_json(json).expect("should parse");
        let mut task_def = sample_task_def();
        let original_image = task_def.container_definitions[0].image.clone();
        config.apply(&mut task_def);

        assert_eq!(task_def.container_definitions[0].image, original_image);
    }

    #[test]
    fn error_invalid_json() {
        let err = OverrideConfig::from_json("not json").unwrap_err();
        assert!(matches!(err, OverrideError::ParseJson(_)));
    }

    #[test]
    fn error_file_not_found() {
        let err = OverrideConfig::from_file(Path::new("/nonexistent/override.json")).unwrap_err();
        assert!(
            matches!(err, OverrideError::ReadFile { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn error_display_messages() {
        let err = OverrideConfig::from_json("bad").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to parse override JSON"));

        let err = OverrideConfig::from_file(Path::new("/no/such")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to read override file"));
    }

    // --- Property-based tests ---

    mod pbt {
        use super::*;
        use crate::taskdef::{ContainerDefinition, Environment, PortMapping, TaskDefinition};
        use proptest::prelude::*;
        use std::collections::{HashMap, HashSet};

        // ── Generators ─────────────────────────────────────────────────

        fn arb_container_name() -> impl Strategy<Value = String> {
            "[a-z][a-z0-9_-]{0,9}"
        }

        fn arb_env_key() -> impl Strategy<Value = String> {
            "[A-Z][A-Z0-9_]{0,7}"
        }

        fn arb_env_value() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9/._:-]{0,15}"
        }

        fn arb_image() -> impl Strategy<Value = String> {
            (
                "[a-z][a-z0-9]{0,8}",
                proptest::option::of(":[a-z0-9][a-z0-9._-]{0,8}"),
            )
                .prop_map(|(name, tag)| match tag {
                    Some(t) => format!("{name}{t}"),
                    None => name,
                })
        }

        fn arb_protocol() -> impl Strategy<Value = String> {
            prop_oneof![Just("tcp".to_string()), Just("udp".to_string())]
        }

        fn arb_port_mapping() -> impl Strategy<Value = PortMapping> {
            (
                1024u16..65_535,
                proptest::option::of(1024u16..65_535),
                arb_protocol(),
            )
                .prop_map(|(cp, hp, protocol)| PortMapping {
                    container_port: cp,
                    host_port: hp,
                    protocol,
                })
        }

        fn arb_environment() -> impl Strategy<Value = Environment> {
            (arb_env_key(), arb_env_value()).prop_map(|(name, value)| Environment { name, value })
        }

        /// Generate a `ContainerDefinition` with given name and random image/env/ports.
        fn arb_container_def(name: String) -> impl Strategy<Value = ContainerDefinition> {
            (
                arb_image(),
                proptest::collection::vec(arb_environment(), 0..=4),
                proptest::collection::vec(arb_port_mapping(), 0..=3),
            )
                .prop_map(move |(image, environment, port_mappings)| {
                    // Dedupe env keys to mirror realistic task defs.
                    let mut seen_keys = HashSet::new();
                    let environment: Vec<Environment> = environment
                        .into_iter()
                        .filter(|e| seen_keys.insert(e.name.clone()))
                        .collect();
                    ContainerDefinition {
                        name: name.clone(),
                        image,
                        environment,
                        port_mappings,
                        ..Default::default()
                    }
                })
        }

        /// Generate a `TaskDefinition` with 1..=4 containers with unique names.
        pub(super) fn arb_task_def() -> impl Strategy<Value = TaskDefinition> {
            proptest::collection::vec(arb_container_name(), 1..=4).prop_flat_map(|names| {
                // Deduplicate container names.
                let mut seen = HashSet::new();
                let unique: Vec<String> = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| {
                        if seen.contains(&n) {
                            format!("{n}{i}")
                        } else {
                            seen.insert(n.clone());
                            n
                        }
                    })
                    .collect();
                let strategies: Vec<_> = unique.iter().cloned().map(arb_container_def).collect();
                strategies.prop_map(|containers| TaskDefinition {
                    family: "proptest".to_string(),
                    task_role_arn: None,
                    execution_role_arn: None,
                    volumes: vec![],
                    container_definitions: containers,
                })
            })
        }

        fn arb_container_override() -> impl Strategy<Value = ContainerOverride> {
            (
                proptest::option::of(arb_image()),
                proptest::option::of(proptest::collection::hash_map(
                    arb_env_key(),
                    arb_env_value(),
                    0..=4,
                )),
                proptest::option::of(proptest::collection::vec(arb_port_mapping(), 0..=3)),
            )
                .prop_map(|(image, environment, port_mappings)| ContainerOverride {
                    image,
                    environment,
                    port_mappings,
                })
        }

        /// Generate a pair of (`TaskDefinition`, `OverrideConfig`) where the
        /// config's keys are a subset of the task def's container names.
        fn arb_td_and_cfg() -> impl Strategy<Value = (TaskDefinition, OverrideConfig)> {
            arb_task_def().prop_flat_map(|td| {
                let names: Vec<String> = td
                    .container_definitions
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
                let n = names.len();
                let indices_strat = proptest::collection::vec(0..n.max(1), 0..=n);
                indices_strat.prop_flat_map(move |indices| {
                    let mut seen = HashSet::new();
                    let selected: Vec<String> = indices
                        .into_iter()
                        .filter_map(|i| {
                            if i < names.len() && seen.insert(i) {
                                Some(names[i].clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let overrides_strat: Vec<_> =
                        selected.iter().map(|_| arb_container_override()).collect();
                    let td = td.clone();
                    (Just(selected), overrides_strat).prop_map(move |(keys, values)| {
                        let cfg = OverrideConfig {
                            container_overrides: keys.into_iter().zip(values).collect(),
                        };
                        (td.clone(), cfg)
                    })
                })
            })
        }

        // ── Helpers for semantic comparison ────────────────────────────

        fn container_by_name<'a>(
            td: &'a TaskDefinition,
            name: &str,
        ) -> Option<&'a ContainerDefinition> {
            td.container_definitions.iter().find(|c| c.name == name)
        }

        fn env_to_map(env: &[Environment]) -> HashMap<&str, &str> {
            env.iter()
                .map(|e| (e.name.as_str(), e.value.as_str()))
                .collect()
        }

        fn ports_tuples(ports: &[PortMapping]) -> Vec<(u16, Option<u16>, String)> {
            ports
                .iter()
                .map(|p| (p.container_port, p.host_port, p.protocol.clone()))
                .collect()
        }

        /// Snapshot of a container's observable state (name, image, sorted env, ports).
        type ContainerSnap = (
            String,
            String,
            Vec<(String, String)>,
            Vec<(u16, Option<u16>, String)>,
        );

        fn td_snapshot(td: &TaskDefinition) -> Vec<ContainerSnap> {
            td.container_definitions
                .iter()
                .map(|c| {
                    let mut env: Vec<(String, String)> = c
                        .environment
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect();
                    env.sort();
                    (
                        c.name.clone(),
                        c.image.clone(),
                        env,
                        ports_tuples(&c.port_mappings),
                    )
                })
                .collect()
        }

        // ── Properties ────────────────────────────────────────────────

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            /// Property: apply is idempotent — running twice equals running once.
            #[test]
            fn apply_is_idempotent((td, cfg) in arb_td_and_cfg()) {
                let mut once = td;
                cfg.apply(&mut once);
                let snap_once = td_snapshot(&once);
                cfg.apply(&mut once);
                let snap_twice = td_snapshot(&once);
                prop_assert_eq!(snap_once, snap_twice);
            }

            /// Property: Overrides for unknown containers cause no mutation.
            #[test]
            fn apply_unknown_container_no_mutation(
                td in arb_task_def(),
                bogus in "[z]{5,8}[0-9]{2}",
                o in arb_container_override(),
            ) {
                // Ensure the name does not clash with any existing container.
                let mut bogus_name = bogus;
                while td.container_definitions.iter().any(|c| c.name == bogus_name) {
                    bogus_name.push('z');
                }
                let mut map = HashMap::new();
                map.insert(bogus_name, o);
                let cfg = OverrideConfig { container_overrides: map };
                let before = td_snapshot(&td);
                let mut td = td;
                cfg.apply(&mut td);
                let after = td_snapshot(&td);
                prop_assert_eq!(before, after);
            }

            /// Property: Image override is a full replacement.
            #[test]
            fn apply_image_is_replacement(td in arb_task_def(), new_image in arb_image()) {
                let mut td = td;
                if td.container_definitions.is_empty() { return Ok(()); }
                let target = td.container_definitions[0].name.clone();
                let mut map = HashMap::new();
                map.insert(
                    target.clone(),
                    ContainerOverride {
                        image: Some(new_image.clone()),
                        environment: None,
                        port_mappings: None,
                    },
                );
                let cfg = OverrideConfig { container_overrides: map };
                cfg.apply(&mut td);
                let c = container_by_name(&td, &target).expect("container exists");
                prop_assert_eq!(&c.image, &new_image);
            }

            /// Property: Environment is merged by key (upsert semantics).
            #[test]
            fn apply_env_key_upsert(
                td in arb_task_def(),
                key in arb_env_key(),
                value in arb_env_value(),
            ) {
                let mut td = td;
                if td.container_definitions.is_empty() { return Ok(()); }
                let target = td.container_definitions[0].name.clone();
                let mut env_map = HashMap::new();
                env_map.insert(key.clone(), value.clone());
                let mut map = HashMap::new();
                map.insert(
                    target.clone(),
                    ContainerOverride {
                        image: None,
                        environment: Some(env_map),
                        port_mappings: None,
                    },
                );
                let cfg = OverrideConfig { container_overrides: map };
                cfg.apply(&mut td);
                let c = container_by_name(&td, &target).expect("container exists");
                let m = env_to_map(&c.environment);
                prop_assert_eq!(m.get(key.as_str()), Some(&value.as_str()));
                // No duplicate keys after apply.
                let mut seen = HashSet::new();
                for e in &c.environment {
                    prop_assert!(seen.insert(e.name.clone()), "duplicate key '{}'", e.name);
                }
            }

            /// Property: Environment keys not mentioned in the override are preserved.
            #[test]
            fn apply_env_preserves_untouched_keys(
                td in arb_task_def(),
                key in arb_env_key(),
                value in arb_env_value(),
            ) {
                if td.container_definitions.is_empty() { return Ok(()); }
                let target = td.container_definitions[0].name.clone();
                let before_map: HashMap<String, String> = container_by_name(&td, &target)
                    .expect("container exists")
                    .environment
                    .iter()
                    .map(|e| (e.name.clone(), e.value.clone()))
                    .collect();
                let mut env_map = HashMap::new();
                env_map.insert(key.clone(), value);
                let mut map = HashMap::new();
                map.insert(
                    target.clone(),
                    ContainerOverride {
                        image: None,
                        environment: Some(env_map),
                        port_mappings: None,
                    },
                );
                let cfg = OverrideConfig { container_overrides: map };
                let mut td = td;
                cfg.apply(&mut td);
                let after_map = env_to_map(&container_by_name(&td, &target).expect("exists").environment);
                for (k, v) in &before_map {
                    if k == &key { continue; }
                    prop_assert_eq!(after_map.get(k.as_str()), Some(&v.as_str()));
                }
            }

            /// Property: Port mappings override is a full replacement.
            #[test]
            fn apply_port_mappings_full_replace(
                td in arb_task_def(),
                new_ports in proptest::collection::vec(arb_port_mapping(), 0..=4),
            ) {
                let mut td = td;
                if td.container_definitions.is_empty() { return Ok(()); }
                let target = td.container_definitions[0].name.clone();
                let mut map = HashMap::new();
                map.insert(
                    target.clone(),
                    ContainerOverride {
                        image: None,
                        environment: None,
                        port_mappings: Some(new_ports.clone()),
                    },
                );
                let cfg = OverrideConfig { container_overrides: map };
                cfg.apply(&mut td);
                let c = container_by_name(&td, &target).expect("exists");
                prop_assert_eq!(ports_tuples(&c.port_mappings), ports_tuples(&new_ports));
            }

            /// Property: Env var count never decreases after apply (upsert only adds/overwrites).
            #[test]
            fn apply_env_count_monotonic((td, cfg) in arb_td_and_cfg()) {
                let before: HashMap<String, usize> = td
                    .container_definitions
                    .iter()
                    .map(|c| (c.name.clone(), c.environment.len()))
                    .collect();
                let mut td = td;
                cfg.apply(&mut td);
                for c in &td.container_definitions {
                    let prior = before.get(&c.name).copied().unwrap_or(0);
                    prop_assert!(
                        c.environment.len() >= prior,
                        "env count shrank: {} -> {}",
                        prior, c.environment.len()
                    );
                }
            }

            /// Property: Container count is invariant across apply.
            #[test]
            fn apply_container_count_unchanged((td, cfg) in arb_td_and_cfg()) {
                let before = td.container_definitions.len();
                let mut td = td;
                cfg.apply(&mut td);
                prop_assert_eq!(td.container_definitions.len(), before);
            }
        }
    }
}
