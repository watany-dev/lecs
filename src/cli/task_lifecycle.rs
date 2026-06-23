//! Shared task lifecycle management: start, metadata, cleanup, container config.
//!
//! Extracted from `run.rs` so that both `run` and `watch` commands can share
//! the same lifecycle operations without coupling to each other's implementation.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::container::{ContainerConfig, ContainerRuntime, HealthCheckConfig, PortMappingConfig};
use crate::credentials;
use crate::events::{EventSink, EventType, LifecycleEvent};
use crate::metadata::{
    self, ContainerMetadata, MetadataServer, ServerState, SharedState, build_container_metadata,
    build_task_metadata,
};
use crate::orchestrator::{ContainerSpec, orchestrate_startup};
use crate::taskdef::{ContainerDefinition, MountPoint, TaskDefinition, Volume};

/// Create the network and start all containers for a task definition.
///
/// Uses the orchestrator to resolve `dependsOn` DAG and start containers in
/// the correct order, waiting for dependency conditions between layers.
pub async fn run_task(
    client: &(impl ContainerRuntime + ?Sized),
    task_def: &TaskDefinition,
    metadata_port: Option<u16>,
    auth_token: Option<&str>,
    event_sink: &dyn EventSink,
) -> Result<(String, Vec<(String, String)>)> {
    let network_name = client.create_network(&task_def.family).await?;
    tracing::info!(network = %network_name, "Created network");

    let specs: Vec<ContainerSpec> = task_def
        .container_definitions
        .iter()
        .map(|def| {
            let config = build_container_config(
                &task_def.family,
                def,
                &network_name,
                metadata_port,
                &task_def.volumes,
                auth_token,
            );
            ContainerSpec {
                name: def.name.clone(),
                config,
                depends_on: def.depends_on.clone(),
                health_check: def.health_check.clone(),
                essential: def.essential,
            }
        })
        .collect();

    match orchestrate_startup(client, specs, event_sink).await {
        Ok(result) => Ok((network_name, result.started)),
        Err((partial, err)) => {
            cleanup(
                client,
                &partial.started,
                &network_name,
                event_sink,
                &task_def.family,
            )
            .await;
            Err(err.into())
        }
    }
}

/// Start the metadata/credentials sidecar server.
#[cfg(not(tarpaulin_include))]
pub async fn start_metadata_server(
    task_def: &TaskDefinition,
) -> Result<(MetadataServer, SharedState)> {
    // Load AWS credentials (best-effort)
    let aws_creds = match credentials::load_local_credentials(task_def.task_role_arn.as_deref())
        .await
    {
        Ok(creds) => {
            tracing::info!("Loaded AWS credentials for metadata server");
            Some(creds)
        }
        Err(e) => {
            tracing::warn!(error = %e, "Could not load AWS credentials; credential endpoint will return 404");
            None
        }
    };

    let task_metadata = build_task_metadata(task_def);
    let container_metadata: HashMap<String, ContainerMetadata> = task_def
        .container_definitions
        .iter()
        .map(|def| {
            (
                def.name.clone(),
                build_container_metadata(&task_def.family, def),
            )
        })
        .collect();

    let auth_token = metadata::generate_auth_token();
    tracing::debug!("Generated credentials auth token");

    let state = Arc::new(tokio::sync::RwLock::new(ServerState {
        task_metadata,
        container_metadata,
        credentials: aws_creds,
        container_ids: HashMap::new(),
        auth_token,
    }));

    let server = MetadataServer::start(state.clone()).await?;
    tracing::info!(
        port = server.port,
        "ECS metadata server running on http://127.0.0.1:{}",
        server.port
    );

    Ok((server, state))
}

/// Shared context for restart operations, grouping metadata/event/family state.
pub struct RestartContext<'a> {
    /// Shared metadata state (None if metadata is disabled).
    pub metadata_state: Option<&'a SharedState>,
    /// Event sink for lifecycle events.
    pub event_sink: &'a dyn EventSink,
    /// Task family name.
    pub family: &'a str,
}

/// Result of attempting to restart a single container.
#[derive(Debug)]
pub enum RestartOutcome {
    /// Old container removed and replaced by a new one with the returned ID.
    Replaced { new_id: String },
    /// Creating or starting the new container failed, and no container is
    /// left in the runtime (either the old one was already removed, or
    /// there was no old container to begin with on the create-only path).
    /// The caller should retain `old_id = None` and retry at next backoff.
    CreateFailed(anyhow::Error),
    /// Failed before the old container was removed (still present in the
    /// runtime); the caller should keep the existing `old_id` and retry.
    FailedBeforeRemoval(anyhow::Error),
}

/// Restart a single container within a running task (service mode).
///
/// Performs: stop(old) → remove(old) → create(new) → start(new) → update metadata.
/// Emits a `Restarting` lifecycle event at the start and a `Started` event after
/// the new container has started. Metadata state's `container_id` mapping is
/// updated atomically so running HTTP requests see the new ID on next poll.
///
/// If `old_id` is `None`, the stop/remove phase is skipped (create-only retry
/// path, used after a previous attempt that left no old container behind).
pub async fn restart_container(
    client: &(impl ContainerRuntime + ?Sized),
    container_name: &str,
    old_id: Option<&str>,
    config: &crate::container::ContainerConfig,
    ctx: &RestartContext<'_>,
) -> RestartOutcome {
    ctx.event_sink.emit(&LifecycleEvent::new(
        EventType::Restarting,
        ctx.family,
        Some(container_name),
        None,
    ));

    // Best-effort pull: failure is logged but does not block restart.
    if let Err(e) = client.pull_image(&config.image).await {
        tracing::warn!(container = %container_name, error = %e, "Image pull failed during restart (continuing)");
    }

    if let Some(id) = old_id {
        // Best-effort stop: already-stopped containers are fine.
        if let Err(e) = client.stop_container(id, None).await {
            tracing::warn!(container = %container_name, error = %e, "Stop failed during restart (continuing)");
        }

        if let Err(e) = client.remove_container(id).await {
            return RestartOutcome::FailedBeforeRemoval(e.into());
        }
    }

    let new_id = match client.create_container(config).await {
        Ok(id) => id,
        Err(e) => return RestartOutcome::CreateFailed(e.into()),
    };

    ctx.event_sink.emit(&LifecycleEvent::new(
        EventType::Created,
        ctx.family,
        Some(container_name),
        None,
    ));

    if let Err(e) = client.start_container(&new_id).await {
        // Container was created but not started — clean it up.
        let _ = client.remove_container(&new_id).await;
        return RestartOutcome::CreateFailed(e.into());
    }

    ctx.event_sink.emit(&LifecycleEvent::new(
        EventType::Started,
        ctx.family,
        Some(container_name),
        None,
    ));

    if let Some(state) = ctx.metadata_state {
        metadata::update_container_id(state, container_name, &new_id).await;
    }

    tracing::info!(container = %container_name, new_id = %new_id, "Container restarted");
    RestartOutcome::Replaced { new_id }
}

/// Best-effort cleanup: stop and remove containers, then remove the network.
pub async fn cleanup(
    client: &(impl ContainerRuntime + ?Sized),
    containers: &[(String, String)],
    network: &str,
    event_sink: &dyn EventSink,
    family: &str,
) {
    for (id, name) in containers {
        if let Err(e) = client.stop_container(id, None).await {
            tracing::warn!(container = %name, error = %e, "Failed to stop container");
        }
        if let Err(e) = client.remove_container(id).await {
            tracing::warn!(container = %name, error = %e, "Failed to remove container");
        }
        tracing::info!(container = %name, "Cleaned up");
    }

    if let Err(e) = client.remove_network(network).await {
        tracing::warn!(network = %network, error = %e, "Failed to remove network");
    }
    tracing::info!(network = %network, "Network removed");

    event_sink.emit(&LifecycleEvent::new(
        EventType::CleanupCompleted,
        family,
        None,
        None,
    ));
}

/// Resolve mount points against task-level volumes into Docker bind mount strings.
///
/// Returns bind strings in format `host_path:container_path` or `host_path:container_path:ro`.
/// Volumes without `host.source_path` (Docker-managed volumes) are skipped with a warning.
fn resolve_binds(mount_points: &[MountPoint], volumes: &[Volume]) -> Vec<String> {
    let volume_map: HashMap<&str, &Volume> = volumes.iter().map(|v| (v.name.as_str(), v)).collect();

    mount_points
        .iter()
        .filter_map(|mp| {
            let volume = volume_map.get(mp.source_volume.as_str())?;
            let Some(host) = &volume.host else {
                tracing::warn!(
                    volume = %volume.name,
                    "Skipping volume without host.sourcePath (Docker-managed volumes not supported)"
                );
                return None;
            };

            let bind = if mp.read_only {
                format!("{}:{}:ro", host.source_path, mp.container_path)
            } else {
                format!("{}:{}", host.source_path, mp.container_path)
            };
            Some(bind)
        })
        .collect()
}

/// Build container labels from the task definition.
fn build_container_labels(family: &str, def: &ContainerDefinition) -> HashMap<String, String> {
    let mut labels: HashMap<String, String> = def.docker_labels.clone();
    labels.insert(crate::labels::MANAGED.into(), "true".into());
    labels.insert(crate::labels::TASK.into(), family.into());
    labels.insert(crate::labels::CONTAINER.into(), def.name.clone());

    if !def.secrets.is_empty() {
        let secret_names: Vec<String> = def.secrets.iter().map(|s| s.name.clone()).collect();
        labels.insert(crate::labels::SECRETS.into(), secret_names.join(","));
    }

    if let Some(timeout) = def.stop_timeout {
        labels.insert(crate::labels::STOP_TIMEOUT.into(), timeout.to_string());
    }

    if !def.depends_on.is_empty() {
        let deps: Vec<String> = def
            .depends_on
            .iter()
            .map(|d| format!("{}:{:?}", d.container_name, d.condition))
            .collect();
        labels.insert(crate::labels::DEPENDS_ON.into(), deps.join(","));
    }

    labels
}

/// Build environment variables, injecting ECS metadata/credential URIs when metadata is enabled.
fn build_container_env(
    def: &ContainerDefinition,
    metadata_port: Option<u16>,
    auth_token: Option<&str>,
) -> Vec<String> {
    let mut env: Vec<String> = def
        .environment
        .iter()
        .map(|e| format!("{}={}", e.name, e.value))
        .collect();

    if let Some(port) = metadata_port {
        env.push(format!(
            "ECS_CONTAINER_METADATA_URI_V4=http://host.docker.internal:{port}/v4/{}",
            def.name
        ));
        env.push(format!(
            "AWS_CONTAINER_CREDENTIALS_FULL_URI=http://host.docker.internal:{port}/credentials"
        ));
        if let Some(token) = auth_token {
            env.push(format!("AWS_CONTAINER_AUTHORIZATION_TOKEN={token}"));
        }
    }

    env
}

/// Convert an ECS health check definition to the container runtime format.
fn build_health_check_config(hc: &crate::taskdef::HealthCheck) -> HealthCheckConfig {
    const NANOS_PER_SEC: i64 = 1_000_000_000;
    HealthCheckConfig {
        test: hc.command.clone(),
        interval_ns: i64::from(hc.interval) * NANOS_PER_SEC,
        timeout_ns: i64::from(hc.timeout) * NANOS_PER_SEC,
        retries: i64::from(hc.retries),
        start_period_ns: i64::from(hc.start_period) * NANOS_PER_SEC,
    }
}

/// Build `extra_hosts` entries, ensuring `host.docker.internal` is always present.
fn build_extra_hosts(def: &ContainerDefinition) -> Vec<String> {
    let mut hosts: Vec<String> = def
        .extra_hosts
        .iter()
        .map(|h| format!("{}:{}", h.hostname, h.ip_address))
        .collect();
    if !hosts.iter().any(|h| h.starts_with("host.docker.internal:")) {
        hosts.push("host.docker.internal:host-gateway".to_string());
    }
    hosts
}

/// Build tmpfs mount entries from Linux parameters.
fn build_tmpfs(def: &ContainerDefinition) -> HashMap<String, String> {
    def.linux_parameters
        .as_ref()
        .map(|lp| {
            lp.tmpfs
                .iter()
                .map(|t| {
                    let size_bytes = t.size * 1024 * 1024;
                    let mut opts = format!("size={size_bytes}");
                    for opt in &t.mount_options {
                        opts.push(',');
                        opts.push_str(opt);
                    }
                    (t.container_path.clone(), opts)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a container configuration from a task definition container.
#[allow(clippy::too_many_arguments)]
pub fn build_container_config(
    family: &str,
    def: &ContainerDefinition,
    network: &str,
    metadata_port: Option<u16>,
    volumes: &[Volume],
    auth_token: Option<&str>,
) -> ContainerConfig {
    ContainerConfig {
        name: format!("{family}-{}", def.name),
        image: def.image.clone(),
        command: def.command.clone(),
        entry_point: def.entry_point.clone(),
        env: build_container_env(def, metadata_port, auth_token),
        port_mappings: def
            .port_mappings
            .iter()
            .map(|p| PortMappingConfig {
                host_port: p.host_port.unwrap_or(p.container_port),
                container_port: p.container_port,
                protocol: p.protocol.clone(),
            })
            .collect(),
        network: network.into(),
        network_aliases: vec![def.name.clone()],
        labels: build_container_labels(family, def),
        extra_hosts: build_extra_hosts(def),
        health_check: def.health_check.as_ref().map(build_health_check_config),
        binds: resolve_binds(&def.mount_points, volumes),
        working_dir: def.working_directory.clone(),
        user: def.user.clone(),
        cpu_units: def.cpu,
        memory_mib: def.memory,
        memory_reservation_mib: def.memory_reservation,
        ulimits: def
            .ulimits
            .iter()
            .map(|u| crate::container::UlimitConfig {
                name: u.name.clone(),
                soft: u.soft_limit,
                hard: u.hard_limit,
            })
            .collect(),
        init: def
            .linux_parameters
            .as_ref()
            .and_then(|lp| lp.init_process_enabled),
        shm_size: def
            .linux_parameters
            .as_ref()
            .and_then(|lp| lp.shared_memory_size)
            .map(|mib| mib * 1024 * 1024),
        tmpfs: build_tmpfs(def),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::container::test_support::MockContainerClient;
    use crate::events::NullEventSink;
    use crate::taskdef::{
        ContainerDefinition, DependencyCondition, DependsOn, Environment, ExtraHost, MountPoint,
        PortMapping, Volume, VolumeHost,
    };

    fn single_container_taskdef() -> TaskDefinition {
        TaskDefinition {
            family: "web".to_string(),
            task_role_arn: None,
            execution_role_arn: None,
            volumes: vec![],
            container_definitions: vec![ContainerDefinition {
                name: "app".to_string(),
                image: "nginx:latest".to_string(),
                ..Default::default()
            }],
        }
    }

    fn two_container_taskdef() -> TaskDefinition {
        TaskDefinition {
            family: "multi".to_string(),
            task_role_arn: None,
            execution_role_arn: None,
            volumes: vec![],
            container_definitions: vec![
                ContainerDefinition {
                    name: "app".to_string(),
                    image: "nginx:latest".to_string(),
                    ..Default::default()
                },
                ContainerDefinition {
                    name: "sidecar".to_string(),
                    image: "redis:latest".to_string(),
                    essential: false,
                    ..Default::default()
                },
            ],
        }
    }

    #[tokio::test]
    async fn run_task_single_container() {
        let mock = MockContainerClient {
            create_network_results: Mutex::new(VecDeque::from([Ok("lecs-web".to_string())])),
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Ok("container-1".to_string())])),
            start_container_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let task_def = single_container_taskdef();
        let (network, containers) = run_task(&mock, &task_def, None, None, &NullEventSink)
            .await
            .expect("should succeed");

        assert_eq!(network, "lecs-web");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].0, "container-1");
        assert_eq!(containers[0].1, "app");
    }

    #[tokio::test]
    async fn run_task_multi_container() {
        let mock = MockContainerClient {
            create_network_results: Mutex::new(VecDeque::from([Ok("lecs-multi".to_string())])),
            pull_image_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([
                Ok("c1".to_string()),
                Ok("c2".to_string()),
            ])),
            start_container_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            ..MockContainerClient::new()
        };

        let task_def = two_container_taskdef();
        let (_, containers) = run_task(&mock, &task_def, None, None, &NullEventSink)
            .await
            .expect("should succeed");

        assert_eq!(containers.len(), 2);
        assert_eq!(containers[0].1, "app");
        assert_eq!(containers[1].1, "sidecar");
    }

    #[tokio::test]
    async fn run_task_network_failure() {
        let mock = MockContainerClient {
            create_network_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            ..MockContainerClient::new()
        };

        let task_def = single_container_taskdef();
        let result = run_task(&mock, &task_def, None, None, &NullEventSink).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_task_container_create_failure() {
        let mock = MockContainerClient {
            create_network_results: Mutex::new(VecDeque::from([Ok("lecs-multi".to_string())])),
            pull_image_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([
                Ok("c1".to_string()),
                Err(crate::container::ContainerError::RuntimeNotRunning),
            ])),
            start_container_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let task_def = two_container_taskdef();
        let result = run_task(&mock, &task_def, None, None, &NullEventSink).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cleanup_success() {
        let mock = MockContainerClient {
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_network_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let containers = vec![("c1".to_string(), "app".to_string())];
        cleanup(&mock, &containers, "lecs-test", &NullEventSink, "test").await;
        // No panic = success (best-effort cleanup)
    }

    #[tokio::test]
    async fn cleanup_tolerates_stop_failure() {
        let mock = MockContainerClient {
            stop_container_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_network_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let containers = vec![("c1".to_string(), "app".to_string())];
        cleanup(&mock, &containers, "lecs-test", &NullEventSink, "test").await;
    }

    #[tokio::test]
    async fn cleanup_tolerates_remove_failure() {
        let mock = MockContainerClient {
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_network_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            ..MockContainerClient::new()
        };

        let containers = vec![("c1".to_string(), "app".to_string())];
        cleanup(&mock, &containers, "lecs-test", &NullEventSink, "test").await;
    }

    #[tokio::test]
    async fn cleanup_emits_cleanup_completed_event() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_network_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let containers = vec![("c1".to_string(), "app".to_string())];
        let sink = CollectingEventSink::new();
        cleanup(&mock, &containers, "lecs-test", &sink, "my-app").await;

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event_type,
            crate::events::EventType::CleanupCompleted
        ));
        assert_eq!(events[0].family, "my-app");
        assert!(events[0].container_name.is_none());
    }

    fn default_config(name: &str) -> crate::container::ContainerConfig {
        crate::container::ContainerConfig {
            name: name.to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn restart_container_replaces_old_with_new() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Ok("new-id".to_string())])),
            start_container_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            Some("old-id"),
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        match outcome {
            RestartOutcome::Replaced { new_id } => assert_eq!(new_id, "new-id"),
            other => panic!("expected Replaced, got {other:?}"),
        }

        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0].event_type,
            crate::events::EventType::Restarting
        ));
        assert!(matches!(
            events[1].event_type,
            crate::events::EventType::Created
        ));
        assert!(matches!(
            events[2].event_type,
            crate::events::EventType::Started
        ));
    }

    #[tokio::test]
    async fn restart_container_tolerates_stop_failure() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            stop_container_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Ok("new-id".to_string())])),
            start_container_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            Some("old-id"),
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        assert!(matches!(outcome, RestartOutcome::Replaced { .. }));
    }

    #[tokio::test]
    async fn restart_container_fails_before_removal_when_remove_errors() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            Some("old-id"),
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        assert!(matches!(outcome, RestartOutcome::FailedBeforeRemoval(_)));
    }

    #[tokio::test]
    async fn restart_container_create_failed_when_create_fails() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            Some("old-id"),
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        assert!(matches!(outcome, RestartOutcome::CreateFailed(_)));
    }

    #[tokio::test]
    async fn restart_container_create_failed_when_start_fails() {
        use crate::events::CollectingEventSink;

        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            stop_container_results: Mutex::new(VecDeque::from([Ok(())])),
            remove_container_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Ok("new-id".to_string())])),
            start_container_results: Mutex::new(VecDeque::from([Err(
                crate::container::ContainerError::RuntimeNotRunning,
            )])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            Some("old-id"),
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        assert!(matches!(outcome, RestartOutcome::CreateFailed(_)));
    }

    #[tokio::test]
    async fn restart_container_create_only_path_when_old_id_none() {
        use crate::events::CollectingEventSink;

        // old_id=None: stop/remove are skipped; only create/start are called.
        let mock = MockContainerClient {
            pull_image_results: Mutex::new(VecDeque::from([Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([Ok("fresh-id".to_string())])),
            start_container_results: Mutex::new(VecDeque::from([Ok(())])),
            ..MockContainerClient::new()
        };

        let config = default_config("my-app-web");
        let sink = CollectingEventSink::new();
        let outcome = restart_container(
            &mock,
            "web",
            None,
            &config,
            &RestartContext {
                metadata_state: None,
                event_sink: &sink,
                family: "my-app",
            },
        )
        .await;

        match outcome {
            RestartOutcome::Replaced { new_id } => assert_eq!(new_id, "fresh-id"),
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn build_container_config_basic() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "nginx:latest".to_string(),
            command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            entry_point: vec!["/docker-entrypoint.sh".into()],
            environment: vec![Environment {
                name: "PORT".to_string(),
                value: "8080".to_string(),
            }],
            port_mappings: vec![PortMapping {
                container_port: 80,
                host_port: Some(8080),
                protocol: "tcp".to_string(),
            }],
            cpu: Some(256),
            memory: Some(512),
            ..Default::default()
        };

        let config = build_container_config("my-app", &def, "lecs-my-app", None, &[], None);

        assert_eq!(config.name, "my-app-app");
        assert_eq!(config.image, "nginx:latest");
        assert_eq!(config.command, vec!["nginx", "-g", "daemon off;"]);
        assert_eq!(config.entry_point, vec!["/docker-entrypoint.sh"]);
        assert_eq!(config.env, vec!["PORT=8080"]);
        assert_eq!(config.port_mappings.len(), 1);
        assert_eq!(config.port_mappings[0].host_port, 8080);
        assert_eq!(config.port_mappings[0].container_port, 80);
        assert_eq!(config.port_mappings[0].protocol, "tcp");
        assert_eq!(config.network, "lecs-my-app");
        assert_eq!(config.network_aliases, vec!["app"]);
        assert_eq!(config.labels.get(crate::labels::MANAGED).unwrap(), "true");
        assert_eq!(config.labels.get(crate::labels::TASK).unwrap(), "my-app");
        assert_eq!(config.labels.get(crate::labels::CONTAINER).unwrap(), "app");
    }

    #[test]
    fn build_container_config_port_default() {
        let def = ContainerDefinition {
            name: "web".to_string(),
            image: "alpine:latest".to_string(),
            port_mappings: vec![PortMapping {
                container_port: 3000,
                host_port: None,
                protocol: "tcp".to_string(),
            }],
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        // host_port defaults to container_port
        assert_eq!(config.port_mappings[0].host_port, 3000);
        assert_eq!(config.port_mappings[0].container_port, 3000);
    }

    #[test]
    fn build_container_config_docker_labels_merged() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            docker_labels: HashMap::from([
                ("com.example.env".into(), "dev".into()),
                (crate::labels::MANAGED.into(), "user-override".into()),
            ]),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        // User labels are included
        assert_eq!(config.labels.get("com.example.env").unwrap(), "dev");
        // Lecs management labels take precedence over user labels
        assert_eq!(config.labels.get(crate::labels::MANAGED).unwrap(), "true");
    }

    #[test]
    fn build_container_config_extra_hosts_merged() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            extra_hosts: vec![ExtraHost {
                hostname: "myhost".to_string(),
                ip_address: "10.0.0.1".to_string(),
            }],
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);
        assert!(config.extra_hosts.contains(&"myhost:10.0.0.1".to_string()));
        // Default host.docker.internal should still be present
        assert!(
            config
                .extra_hosts
                .contains(&"host.docker.internal:host-gateway".to_string())
        );
    }

    #[test]
    fn build_container_config_extra_hosts_user_overrides_default() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            extra_hosts: vec![ExtraHost {
                hostname: "host.docker.internal".to_string(),
                ip_address: "192.168.1.1".to_string(),
            }],
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);
        assert_eq!(config.extra_hosts, vec!["host.docker.internal:192.168.1.1"]);
    }

    #[test]
    fn build_container_config_has_extra_hosts() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);
        assert_eq!(
            config.extra_hosts,
            vec!["host.docker.internal:host-gateway"]
        );
    }

    #[test]
    fn build_container_config_empty_optionals() {
        let def = ContainerDefinition {
            name: "minimal".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        assert!(config.command.is_empty());
        assert!(config.entry_point.is_empty());
        assert!(config.env.is_empty());
        assert!(config.port_mappings.is_empty());
    }

    #[test]
    fn build_container_config_with_metadata_port() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", Some(12345), &[], None);

        assert!(config.env.contains(
            &"ECS_CONTAINER_METADATA_URI_V4=http://host.docker.internal:12345/v4/app".to_string()
        ));
        assert!(
            config.env.contains(
                &"AWS_CONTAINER_CREDENTIALS_FULL_URI=http://host.docker.internal:12345/credentials"
                    .to_string()
            )
        );
    }

    #[test]
    fn build_container_config_with_auth_token() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config(
            "test",
            &def,
            "lecs-test",
            Some(12345),
            &[],
            Some("my-secret-token"),
        );

        assert!(
            config
                .env
                .contains(&"AWS_CONTAINER_AUTHORIZATION_TOKEN=my-secret-token".to_string())
        );
    }

    #[test]
    fn build_container_config_without_auth_token() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", Some(12345), &[], None);

        assert!(
            !config
                .env
                .iter()
                .any(|e| e.starts_with("AWS_CONTAINER_AUTHORIZATION_TOKEN="))
        );
    }

    #[test]
    fn build_container_config_without_metadata_port() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        assert!(
            !config
                .env
                .iter()
                .any(|e| e.starts_with("ECS_CONTAINER_METADATA_URI_V4="))
        );
        assert!(
            !config
                .env
                .iter()
                .any(|e| e.starts_with("AWS_CONTAINER_CREDENTIALS_FULL_URI="))
        );
    }

    #[test]
    fn build_container_config_with_health_check() {
        use crate::taskdef::HealthCheck;

        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            health_check: Some(HealthCheck {
                command: vec!["CMD-SHELL".into(), "curl -f http://localhost/".into()],
                interval: 10,
                timeout: 5,
                retries: 3,
                start_period: 15,
            }),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);
        let hc = config
            .health_check
            .as_ref()
            .expect("should have health check");
        assert_eq!(hc.test, vec!["CMD-SHELL", "curl -f http://localhost/"]);
        assert_eq!(hc.interval_ns, 10_000_000_000);
        assert_eq!(hc.timeout_ns, 5_000_000_000);
        assert_eq!(hc.retries, 3);
        assert_eq!(hc.start_period_ns, 15_000_000_000);
    }

    #[tokio::test]
    async fn run_task_with_dependencies() {
        let mock = MockContainerClient {
            create_network_results: Mutex::new(VecDeque::from([Ok("lecs-test".to_string())])),
            pull_image_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            create_container_results: Mutex::new(VecDeque::from([
                Ok("db-id".to_string()),
                Ok("app-id".to_string()),
            ])),
            start_container_results: Mutex::new(VecDeque::from([Ok(()), Ok(())])),
            ..MockContainerClient::new()
        };

        let task_def = TaskDefinition {
            family: "test".to_string(),
            task_role_arn: None,
            execution_role_arn: None,
            volumes: vec![],
            container_definitions: vec![
                ContainerDefinition {
                    name: "db".to_string(),
                    image: "postgres:16".to_string(),
                    ..Default::default()
                },
                ContainerDefinition {
                    name: "app".to_string(),
                    image: "my-app:latest".to_string(),
                    depends_on: vec![DependsOn {
                        container_name: "db".to_string(),
                        condition: DependencyCondition::Start,
                    }],
                    ..Default::default()
                },
            ],
        };

        let (network, containers) = run_task(&mock, &task_def, None, None, &NullEventSink)
            .await
            .expect("should succeed");

        assert_eq!(network, "lecs-test");
        assert_eq!(containers.len(), 2);
        // DAG ensures db starts before app
        assert_eq!(containers[0].1, "db");
        assert_eq!(containers[1].1, "app");
    }

    #[test]
    fn resolve_binds_single_mount() {
        let volumes = vec![Volume {
            name: "data".to_string(),
            host: Some(VolumeHost {
                source_path: "/host/data".to_string(),
            }),
        }];
        let mount_points = vec![MountPoint {
            source_volume: "data".to_string(),
            container_path: "/app/data".to_string(),
            read_only: false,
        }];

        let binds = resolve_binds(&mount_points, &volumes);
        assert_eq!(binds, vec!["/host/data:/app/data"]);
    }

    #[test]
    fn resolve_binds_read_only() {
        let volumes = vec![Volume {
            name: "config".to_string(),
            host: Some(VolumeHost {
                source_path: "/host/config".to_string(),
            }),
        }];
        let mount_points = vec![MountPoint {
            source_volume: "config".to_string(),
            container_path: "/app/config".to_string(),
            read_only: true,
        }];

        let binds = resolve_binds(&mount_points, &volumes);
        assert_eq!(binds, vec!["/host/config:/app/config:ro"]);
    }

    #[test]
    fn resolve_binds_skips_docker_managed_volume() {
        let volumes = vec![
            Volume {
                name: "data".to_string(),
                host: Some(VolumeHost {
                    source_path: "/host/data".to_string(),
                }),
            },
            Volume {
                name: "cache".to_string(),
                host: None,
            },
        ];
        let mount_points = vec![
            MountPoint {
                source_volume: "data".to_string(),
                container_path: "/app/data".to_string(),
                read_only: false,
            },
            MountPoint {
                source_volume: "cache".to_string(),
                container_path: "/tmp/cache".to_string(),
                read_only: true,
            },
        ];

        let binds = resolve_binds(&mount_points, &volumes);
        assert_eq!(binds, vec!["/host/data:/app/data"]);
    }

    #[test]
    fn build_container_config_with_volumes() {
        let volumes = vec![Volume {
            name: "data".to_string(),
            host: Some(VolumeHost {
                source_path: "/host/data".to_string(),
            }),
        }];
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            mount_points: vec![MountPoint {
                source_volume: "data".to_string(),
                container_path: "/app/data".to_string(),
                read_only: false,
            }],
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &volumes, None);
        assert_eq!(config.binds, vec!["/host/data:/app/data"]);
    }

    #[test]
    fn build_container_config_resource_fields_passthrough() {
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            cpu: Some(256),
            memory: Some(512),
            memory_reservation: Some(256),
            working_directory: Some("/app".to_string()),
            user: Some("1000:1000".to_string()),
            stop_timeout: Some(60),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        assert_eq!(config.cpu_units, Some(256));
        assert_eq!(config.memory_mib, Some(512));
        assert_eq!(config.memory_reservation_mib, Some(256));
        assert_eq!(config.working_dir.as_deref(), Some("/app"));
        assert_eq!(config.user.as_deref(), Some("1000:1000"));
        assert_eq!(
            config.labels.get(crate::labels::STOP_TIMEOUT).unwrap(),
            "60"
        );
    }

    #[test]
    fn build_container_config_linux_parameters() {
        use crate::taskdef::{LinuxParameters, TmpfsMount, Ulimit};
        let def = ContainerDefinition {
            name: "app".to_string(),
            image: "alpine:latest".to_string(),
            ulimits: vec![Ulimit {
                name: "nofile".to_string(),
                soft_limit: 1024,
                hard_limit: 4096,
            }],
            linux_parameters: Some(LinuxParameters {
                init_process_enabled: Some(true),
                shared_memory_size: Some(256),
                tmpfs: vec![TmpfsMount {
                    container_path: "/run".to_string(),
                    size: 64,
                    mount_options: vec!["rw".to_string()],
                }],
            }),
            ..Default::default()
        };

        let config = build_container_config("test", &def, "lecs-test", None, &[], None);

        assert_eq!(config.ulimits.len(), 1);
        assert_eq!(config.ulimits[0].name, "nofile");
        assert_eq!(config.ulimits[0].soft, 1024);
        assert_eq!(config.ulimits[0].hard, 4096);
        assert_eq!(config.init, Some(true));
        assert_eq!(config.shm_size, Some(256 * 1024 * 1024));
        assert!(config.tmpfs.contains_key("/run"));
        let tmpfs_opts = config.tmpfs.get("/run").unwrap();
        assert!(tmpfs_opts.contains("size=67108864"));
        assert!(tmpfs_opts.contains("rw"));
    }
}
