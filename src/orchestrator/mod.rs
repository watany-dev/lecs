//! Container lifecycle orchestration and `dependsOn` DAG.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use serde::Serialize;

use crate::container::ContainerRuntime;
use crate::events::{EventSink, EventType, LifecycleEvent};
use crate::taskdef::{DependencyCondition, DependsOn, HealthCheck};

/// Orchestrator errors.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("cyclic dependency detected: {0}")]
    CyclicDependency(String),

    #[error("container runtime error: {0}")]
    Runtime(#[from] crate::container::ContainerError),

    #[error("condition not met for container '{0}': {1}")]
    ConditionNotMet(String, String),

    #[error("health check timed out for container '{0}'")]
    HealthCheckTimeout(String),

    #[error("container '{0}' exceeded maximum restart count ({1})")]
    MaxRestartsExceeded(String, u32),
}

/// Maximum backoff duration between container restart attempts (5 minutes).
const MAX_BACKOFF_SECS: u64 = 300;

/// Container restart policy for service mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    /// Do not restart (default, task-runner behavior).
    #[default]
    None,
    /// Restart only on non-zero exit code.
    OnFailure,
    /// Always restart regardless of exit code.
    Always,
}

/// Tracks restart state for a single container.
#[derive(Debug)]
pub struct RestartTracker {
    policy: RestartPolicy,
    restart_count: u32,
    max_restarts: u32,
}

impl RestartTracker {
    /// Create a new restart tracker with the given policy and maximum restart count.
    #[must_use]
    pub const fn new(policy: RestartPolicy, max_restarts: u32) -> Self {
        Self {
            policy,
            restart_count: 0,
            max_restarts,
        }
    }

    /// Return `true` if the container should be restarted given `exit_code`.
    ///
    /// Takes the policy and current restart count into account. Returns `false`
    /// when `restart_count` has reached `max_restarts`.
    #[must_use]
    pub const fn should_restart(&self, exit_code: i64) -> bool {
        if self.restart_count >= self.max_restarts {
            return false;
        }
        match self.policy {
            RestartPolicy::None => false,
            RestartPolicy::OnFailure => exit_code != 0,
            RestartPolicy::Always => true,
        }
    }

    /// Compute the next backoff duration based on current restart count.
    ///
    /// Follows exponential backoff: `min(2^restart_count, MAX_BACKOFF_SECS)` seconds.
    #[must_use]
    pub const fn next_backoff(&self) -> Duration {
        let secs = match 1u64.checked_shl(self.restart_count) {
            Some(v) if v < MAX_BACKOFF_SECS => v,
            _ => MAX_BACKOFF_SECS,
        };
        Duration::from_secs(secs)
    }

    /// Increment the restart counter.
    pub const fn record_restart(&mut self) {
        self.restart_count = self.restart_count.saturating_add(1);
    }

    /// Return the current restart count.
    #[must_use]
    pub const fn restart_count(&self) -> u32 {
        self.restart_count
    }

    /// Return the configured maximum restart count.
    #[must_use]
    pub const fn max_restarts(&self) -> u32 {
        self.max_restarts
    }
}

/// Lightweight dependency information for DAG resolution.
#[derive(Debug, Clone)]
pub struct DependencyInfo {
    pub name: String,
    pub depends_on: Vec<DependsOn>,
}

/// Resolve the startup order of containers using Kahn's algorithm.
///
/// Returns layers of container names that can be started concurrently within each layer.
/// Layer N must complete (according to its dependsOn conditions) before layer N+1 starts.
pub fn resolve_start_order(deps: &[DependencyInfo]) -> Result<Vec<Vec<String>>, OrchestratorError> {
    if deps.is_empty() {
        return Ok(vec![]);
    }

    // Build adjacency and in-degree map
    let names: HashSet<&str> = deps.iter().map(|d| d.name.as_str()).collect();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();

    for dep in deps {
        in_degree.entry(dep.name.as_str()).or_insert(0);
        for d in &dep.depends_on {
            if names.contains(d.container_name.as_str()) {
                *in_degree.entry(dep.name.as_str()).or_insert(0) += 1;
                dependents
                    .entry(d.container_name.as_str())
                    .or_default()
                    .push(dep.name.as_str());
            }
        }
    }

    // Kahn's algorithm with layers
    let mut queue: VecDeque<&str> = VecDeque::new();
    let mut initial: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(name, _)| *name)
        .collect();
    initial.sort_unstable();
    queue.extend(initial);

    let mut layers: Vec<Vec<String>> = Vec::new();
    let mut processed = 0usize;

    while !queue.is_empty() {
        let layer: Vec<&str> = std::mem::take(&mut queue).into_iter().collect();
        processed += layer.len();

        // Update in-degrees and collect next layer
        let mut next: Vec<&str> = Vec::new();
        for &name in &layer {
            if let Some(children) = dependents.get(name) {
                for &child in children {
                    if let Some(deg) = in_degree.get_mut(child) {
                        *deg -= 1;
                        if *deg == 0 {
                            next.push(child);
                        }
                    }
                }
            }
        }
        next.sort_unstable();
        queue.extend(next);

        layers.push(layer.into_iter().map(String::from).collect());
    }

    if processed != deps.len() {
        let cycle_path = find_cycle(deps);
        return Err(OrchestratorError::CyclicDependency(cycle_path));
    }

    Ok(layers)
}

/// Find a cycle in the dependency graph using DFS and return a descriptive path string.
fn find_cycle(deps: &[DependencyInfo]) -> String {
    let adj: HashMap<&str, Vec<&str>> = deps
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.depends_on
                    .iter()
                    .map(|dep| dep.container_name.as_str())
                    .collect(),
            )
        })
        .collect();

    let mut visited: HashSet<&str> = HashSet::new();
    let mut in_stack: HashSet<&str> = HashSet::new();
    let mut path: Vec<&str> = Vec::new();

    for dep in deps {
        if !visited.contains(dep.name.as_str())
            && dfs_cycle(
                dep.name.as_str(),
                &adj,
                &mut visited,
                &mut in_stack,
                &mut path,
            )
        {
            return format_cycle_path(&path);
        }
    }

    "unknown cycle".to_string()
}

fn dfs_cycle<'a>(
    node: &'a str,
    adj: &HashMap<&str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
    in_stack: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> bool {
    visited.insert(node);
    in_stack.insert(node);
    path.push(node);

    if let Some(neighbors) = adj.get(node) {
        for &neighbor in neighbors {
            if !visited.contains(neighbor) {
                if dfs_cycle(neighbor, adj, visited, in_stack, path) {
                    return true;
                }
            } else if in_stack.contains(neighbor) {
                path.push(neighbor);
                return true;
            }
        }
    }

    path.pop();
    in_stack.remove(node);
    false
}

fn format_cycle_path(path: &[&str]) -> String {
    if path.len() < 2 {
        return "unknown cycle".to_string();
    }

    let cycle_start = path[path.len() - 1];
    path[..path.len() - 1]
        .iter()
        .position(|&n| n == cycle_start)
        .map_or_else(
            || path.join(" -> "),
            |start_idx| path[start_idx..].join(" -> "),
        )
}

/// Context for emitting lifecycle events during orchestration.
pub struct EventContext<'a> {
    pub event_sink: &'a dyn EventSink,
    pub family: &'a str,
}

/// Container specification for orchestrated startup.
pub struct ContainerSpec {
    pub name: String,
    pub config: crate::container::ContainerConfig,
    pub depends_on: Vec<DependsOn>,
    pub health_check: Option<HealthCheck>,
    #[allow(dead_code)] // Part of ECS task definition contract
    pub essential: bool,
}

/// Result of orchestrated startup.
#[derive(Debug)]
pub struct StartupResult {
    /// Containers that were started: (id, name).
    pub started: Vec<(String, String)>,
}

/// Create and start a single container, emitting lifecycle events.
///
/// Automatically pulls the container image before creation.
async fn create_and_start_container(
    client: &(impl ContainerRuntime + ?Sized),
    spec: &ContainerSpec,
    name: &str,
    event_sink: &dyn EventSink,
    started: &[(String, String)],
) -> Result<String, (StartupResult, OrchestratorError)> {
    let family = spec
        .config
        .labels
        .get(crate::labels::TASK)
        .cloned()
        .unwrap_or_default();

    // Pull the image before creating the container
    let image = &spec.config.image;
    tracing::info!(container = %name, image = %image, "Pulling image");
    client.pull_image(image).await.map_err(|e| {
        (
            StartupResult {
                started: started.to_vec(),
            },
            OrchestratorError::from(e),
        )
    })?;
    event_sink.emit(&LifecycleEvent::new(
        EventType::ImagePulled,
        &family,
        Some(name),
        Some(image),
    ));
    tracing::info!(container = %name, image = %image, "Pulled image");

    let id = client.create_container(&spec.config).await.map_err(|e| {
        (
            StartupResult {
                started: started.to_vec(),
            },
            OrchestratorError::from(e),
        )
    })?;
    event_sink.emit(&LifecycleEvent::new(
        EventType::Created,
        &family,
        Some(name),
        None,
    ));
    client.start_container(&id).await.map_err(|e| {
        (
            StartupResult {
                started: started.to_vec(),
            },
            OrchestratorError::from(e),
        )
    })?;
    event_sink.emit(&LifecycleEvent::new(
        EventType::Started,
        &family,
        Some(name),
        None,
    ));
    tracing::info!(container = %name, "Started container");
    Ok(id)
}

/// Orchestrate container startup following the dependsOn DAG.
///
/// 1. Resolve startup order using topological sort
/// 2. For each layer: create and start containers
/// 3. Between layers: wait for dependency conditions
///
/// On error, returns the partially started containers for cleanup by the caller.
pub async fn orchestrate_startup(
    client: &(impl ContainerRuntime + ?Sized),
    specs: Vec<ContainerSpec>,
    event_sink: &dyn EventSink,
) -> Result<StartupResult, (StartupResult, OrchestratorError)> {
    // Build dependency info for DAG resolution
    let dep_infos: Vec<DependencyInfo> = specs
        .iter()
        .map(|s| DependencyInfo {
            name: s.name.clone(),
            depends_on: s.depends_on.clone(),
        })
        .collect();

    let layers = resolve_start_order(&dep_infos).map_err(|e| {
        (
            StartupResult {
                started: Vec::new(),
            },
            e,
        )
    })?;

    // Index specs by name
    let specs_by_name: HashMap<String, &ContainerSpec> =
        specs.iter().map(|s| (s.name.clone(), s)).collect();

    let mut started: Vec<(String, String)> = Vec::new();
    let mut id_by_name: HashMap<String, String> = HashMap::new();

    for (layer_idx, layer) in layers.iter().enumerate() {
        // Start all containers in this layer
        for name in layer {
            let spec = specs_by_name[name];
            let id = create_and_start_container(client, spec, name, event_sink, &started).await?;
            started.push((id.clone(), name.clone()));
            id_by_name.insert(name.clone(), id);
        }

        // Wait for conditions needed by the next layer
        if let Some(next_layer) = layers.get(layer_idx + 1) {
            let mut waited: HashSet<(String, DependencyCondition)> = HashSet::new();
            for next_name in next_layer {
                let next_spec = specs_by_name[next_name.as_str()];
                for dep in &next_spec.depends_on {
                    if let Some(dep_id) = id_by_name.get(&dep.container_name)
                        && waited.insert((dep.container_name.clone(), dep.condition))
                    {
                        let dep_spec = specs_by_name[dep.container_name.as_str()];
                        let dep_family = dep_spec
                            .config
                            .labels
                            .get(crate::labels::TASK)
                            .map(String::as_str)
                            .unwrap_or_default();
                        let dep_health_check = dep_spec.health_check.as_ref();
                        let ctx = EventContext {
                            event_sink,
                            family: dep_family,
                        };
                        wait_for_condition(
                            client,
                            dep_id,
                            &dep.container_name,
                            dep.condition,
                            dep_health_check,
                            &ctx,
                        )
                        .await
                        .map_err(|e| {
                            (
                                StartupResult {
                                    started: started.clone(),
                                },
                                e,
                            )
                        })?;
                    }
                }
            }
        }
    }

    Ok(StartupResult { started })
}

/// Wait for a dependency condition to be met.
#[allow(clippy::too_many_arguments)]
pub async fn wait_for_condition(
    client: &(impl ContainerRuntime + ?Sized),
    id: &str,
    name: &str,
    condition: DependencyCondition,
    health_check: Option<&HealthCheck>,
    ctx: &EventContext<'_>,
) -> Result<(), OrchestratorError> {
    match condition {
        DependencyCondition::Start => {
            // Container was already started, condition is met immediately.
            Ok(())
        }
        DependencyCondition::Complete => {
            let result = client.wait_container(id).await?;
            tracing::info!(container = %name, exit_code = result.status_code, "Container completed");
            ctx.event_sink.emit(&LifecycleEvent::new(
                EventType::Exited,
                ctx.family,
                Some(name),
                Some(&format!("exit code: {}", result.status_code)),
            ));
            Ok(())
        }
        DependencyCondition::Success => {
            let result = client.wait_container(id).await?;
            ctx.event_sink.emit(&LifecycleEvent::new(
                EventType::Exited,
                ctx.family,
                Some(name),
                Some(&format!("exit code: {}", result.status_code)),
            ));
            if result.status_code == 0 {
                tracing::info!(container = %name, "Container completed successfully");
                Ok(())
            } else {
                Err(OrchestratorError::ConditionNotMet(
                    name.to_string(),
                    format!("expected exit code 0, got {}", result.status_code),
                ))
            }
        }
        DependencyCondition::Healthy => {
            let hc = health_check.ok_or_else(|| {
                OrchestratorError::ConditionNotMet(
                    name.to_string(),
                    "HEALTHY condition requires a healthCheck".to_string(),
                )
            })?;
            wait_for_healthy(client, id, name, hc, ctx).await
        }
    }
}

/// Result of an essential container exiting.
pub struct EssentialExit {
    pub container_name: String,
    pub exit_code: i64,
}

/// Watch for an essential container to exit. Returns when the container stops.
///
/// Intended to be spawned via `tokio::spawn` and combined with `tokio::select!`
/// alongside Ctrl+C signal handling.
pub async fn watch_essential_exit(
    client: &(impl ContainerRuntime + ?Sized),
    id: &str,
    name: &str,
) -> EssentialExit {
    match client.wait_container(id).await {
        Ok(result) => EssentialExit {
            container_name: name.to_string(),
            exit_code: result.status_code,
        },
        Err(e) => {
            tracing::warn!(container = %name, error = %e, "Error watching essential container");
            EssentialExit {
                container_name: name.to_string(),
                exit_code: -1,
            }
        }
    }
}

/// Poll `inspect_container` until health status becomes "healthy" or timeout.
async fn wait_for_healthy(
    client: &(impl ContainerRuntime + ?Sized),
    id: &str,
    name: &str,
    health_check: &HealthCheck,
    ctx: &EventContext<'_>,
) -> Result<(), OrchestratorError> {
    let timeout_secs = u64::from(health_check.start_period)
        + u64::from(health_check.interval) * (u64::from(health_check.retries) + 1)
        + 30;
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_secs(u64::from(health_check.interval).max(1));

    let poll_future = async {
        loop {
            let inspection = client.inspect_container(id).await?;
            match inspection.state.health_status.as_deref() {
                Some("healthy") => {
                    tracing::info!(container = %name, "Container is healthy");
                    ctx.event_sink.emit(&LifecycleEvent::new(
                        EventType::HealthCheckPassed,
                        ctx.family,
                        Some(name),
                        None,
                    ));
                    return Ok(());
                }
                Some("unhealthy") => {
                    ctx.event_sink.emit(&LifecycleEvent::new(
                        EventType::HealthCheckFailed,
                        ctx.family,
                        Some(name),
                        Some("status: unhealthy"),
                    ));
                    return Err(OrchestratorError::HealthCheckTimeout(name.to_string()));
                }
                _ => {
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    };

    tokio::time::timeout(timeout, poll_future)
        .await
        .map_err(|_| {
            ctx.event_sink.emit(&LifecycleEvent::new(
                EventType::HealthCheckFailed,
                ctx.family,
                Some(name),
                Some("timed out"),
            ));
            OrchestratorError::HealthCheckTimeout(name.to_string())
        })?
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::events::{CollectingEventSink, NullEventSink};
    use crate::taskdef::DependencyCondition;

    // --- RestartPolicy / RestartTracker tests ---

    #[test]
    fn restart_policy_default_is_none() {
        assert_eq!(RestartPolicy::default(), RestartPolicy::None);
    }

    #[test]
    fn restart_policy_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&RestartPolicy::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&RestartPolicy::OnFailure).unwrap(),
            "\"on_failure\""
        );
        assert_eq!(
            serde_json::to_string(&RestartPolicy::Always).unwrap(),
            "\"always\""
        );
    }

    #[test]
    fn should_restart_none_never_restarts() {
        let tracker = RestartTracker::new(RestartPolicy::None, 10);
        assert!(!tracker.should_restart(0));
        assert!(!tracker.should_restart(1));
        assert!(!tracker.should_restart(137));
    }

    #[test]
    fn should_restart_on_failure_only_nonzero() {
        let tracker = RestartTracker::new(RestartPolicy::OnFailure, 10);
        assert!(!tracker.should_restart(0));
        assert!(tracker.should_restart(1));
        assert!(tracker.should_restart(137));
    }

    #[test]
    fn should_restart_always_restarts_any_code() {
        let tracker = RestartTracker::new(RestartPolicy::Always, 10);
        assert!(tracker.should_restart(0));
        assert!(tracker.should_restart(1));
    }

    #[test]
    fn should_restart_respects_max_restarts() {
        let mut tracker = RestartTracker::new(RestartPolicy::Always, 3);
        for _ in 0..3 {
            assert!(tracker.should_restart(0));
            tracker.record_restart();
        }
        assert!(!tracker.should_restart(0));
        assert!(!tracker.should_restart(1));
    }

    #[test]
    fn should_restart_respects_max_restarts_on_failure() {
        let mut tracker = RestartTracker::new(RestartPolicy::OnFailure, 2);
        tracker.record_restart();
        tracker.record_restart();
        assert!(!tracker.should_restart(1));
    }

    #[test]
    fn next_backoff_exponential_progression() {
        let mut tracker = RestartTracker::new(RestartPolicy::Always, 20);
        assert_eq!(tracker.next_backoff(), Duration::from_secs(1));
        tracker.record_restart();
        assert_eq!(tracker.next_backoff(), Duration::from_secs(2));
        tracker.record_restart();
        assert_eq!(tracker.next_backoff(), Duration::from_secs(4));
        tracker.record_restart();
        assert_eq!(tracker.next_backoff(), Duration::from_secs(8));
    }

    #[test]
    fn next_backoff_caps_at_max() {
        let mut tracker = RestartTracker::new(RestartPolicy::Always, 20);
        // After 8 restarts, 2^8 = 256 < 300
        for _ in 0..8 {
            tracker.record_restart();
        }
        assert_eq!(tracker.next_backoff(), Duration::from_secs(256));
        // After 9 restarts, 2^9 = 512 should be capped to 300
        tracker.record_restart();
        assert_eq!(tracker.next_backoff(), Duration::from_secs(300));
        // And remain capped at 15 restarts
        for _ in 0..6 {
            tracker.record_restart();
        }
        assert_eq!(tracker.next_backoff(), Duration::from_secs(300));
    }

    #[test]
    fn record_restart_increments_counter() {
        let mut tracker = RestartTracker::new(RestartPolicy::Always, 10);
        assert_eq!(tracker.restart_count(), 0);
        tracker.record_restart();
        assert_eq!(tracker.restart_count(), 1);
        tracker.record_restart();
        assert_eq!(tracker.restart_count(), 2);
    }

    #[test]
    fn tracker_exposes_max() {
        let tracker = RestartTracker::new(RestartPolicy::OnFailure, 5);
        assert_eq!(tracker.max_restarts(), 5);
    }

    #[test]
    fn max_restarts_exceeded_error_display() {
        let err = OrchestratorError::MaxRestartsExceeded("web".into(), 10);
        assert_eq!(
            err.to_string(),
            "container 'web' exceeded maximum restart count (10)"
        );
    }

    fn dep(name: &str, depends: &[(&str, DependencyCondition)]) -> DependencyInfo {
        DependencyInfo {
            name: name.to_string(),
            depends_on: depends
                .iter()
                .map(|(n, c)| DependsOn {
                    container_name: n.to_string(),
                    condition: *c,
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_no_dependencies() {
        let deps = vec![dep("a", &[]), dep("b", &[]), dep("c", &[])];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0], vec!["a", "b", "c"]);
    }

    #[test]
    fn resolve_linear_chain() {
        let deps = vec![
            dep("a", &[]),
            dep("b", &[("a", DependencyCondition::Start)]),
            dep("c", &[("b", DependencyCondition::Healthy)]),
        ];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec!["a"]);
        assert_eq!(layers[1], vec!["b"]);
        assert_eq!(layers[2], vec!["c"]);
    }

    #[test]
    fn resolve_diamond_dependency() {
        let deps = vec![
            dep("a", &[]),
            dep("b", &[("a", DependencyCondition::Start)]),
            dep("c", &[("a", DependencyCondition::Start)]),
            dep(
                "d",
                &[
                    ("b", DependencyCondition::Complete),
                    ("c", DependencyCondition::Complete),
                ],
            ),
        ];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec!["a"]);
        assert_eq!(layers[1], vec!["b", "c"]);
        assert_eq!(layers[2], vec!["d"]);
    }

    #[test]
    fn resolve_single_container() {
        let deps = vec![dep("only", &[])];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0], vec!["only"]);
    }

    #[test]
    fn resolve_circular_two_nodes() {
        let deps = vec![
            dep("a", &[("b", DependencyCondition::Start)]),
            dep("b", &[("a", DependencyCondition::Start)]),
        ];
        let err = resolve_start_order(&deps).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::CyclicDependency(ref msg) if msg.contains('a') && msg.contains('b')),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_circular_three_nodes() {
        let deps = vec![
            dep("a", &[("c", DependencyCondition::Start)]),
            dep("b", &[("a", DependencyCondition::Start)]),
            dep("c", &[("b", DependencyCondition::Start)]),
        ];
        let err = resolve_start_order(&deps).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::CyclicDependency(_)),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_partial_dependencies() {
        let deps = vec![
            dep("a", &[]),
            dep("b", &[("a", DependencyCondition::Success)]),
            dep("c", &[]),
        ];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0], vec!["a", "c"]);
        assert_eq!(layers[1], vec!["b"]);
    }

    #[test]
    fn resolve_multiple_roots() {
        let deps = vec![
            dep("a", &[]),
            dep("b", &[]),
            dep(
                "c",
                &[
                    ("a", DependencyCondition::Start),
                    ("b", DependencyCondition::Start),
                ],
            ),
        ];
        let layers = resolve_start_order(&deps).expect("should resolve");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0], vec!["a", "b"]);
        assert_eq!(layers[1], vec!["c"]);
    }

    #[test]
    fn resolve_empty_input() {
        let layers = resolve_start_order(&[]).expect("should resolve");
        assert!(layers.is_empty());
    }

    // --- Condition waiting tests ---

    use crate::container::test_support::MockContainerClient;
    use crate::container::{ContainerInspection, ContainerState, WaitResult};
    use crate::taskdef::HealthCheck;

    fn null_ctx() -> EventContext<'static> {
        EventContext {
            event_sink: &NullEventSink,
            family: "test",
        }
    }

    #[tokio::test]
    async fn wait_for_condition_start_returns_immediately() {
        let mock = MockContainerClient::new();
        let result = wait_for_condition(
            &mock,
            "id1",
            "app",
            DependencyCondition::Start,
            None,
            &null_ctx(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_condition_complete_waits_for_exit() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 1 }));

        let result = wait_for_condition(
            &mock,
            "id1",
            "app",
            DependencyCondition::Complete,
            None,
            &null_ctx(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_condition_success_exits_zero() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 0 }));

        let result = wait_for_condition(
            &mock,
            "id1",
            "app",
            DependencyCondition::Success,
            None,
            &null_ctx(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_condition_success_exits_nonzero_fails() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 1 }));

        let result = wait_for_condition(
            &mock,
            "id1",
            "app",
            DependencyCondition::Success,
            None,
            &null_ctx(),
        )
        .await;
        assert!(
            matches!(result, Err(OrchestratorError::ConditionNotMet(ref name, _)) if name == "app"),
            "unexpected: {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_healthy_becomes_healthy() {
        let mock = MockContainerClient::new();
        // First poll: starting, second poll: healthy
        {
            let mut q = mock.inspect_container_results.lock().unwrap();
            q.push_back(Ok(make_inspection("id1", Some("starting"))));
            q.push_back(Ok(make_inspection("id1", Some("healthy"))));
        }

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "true".into()],
            interval: 1,
            timeout: 1,
            retries: 3,
            start_period: 0,
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &null_ctx(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn wait_for_healthy_becomes_unhealthy_fails() {
        let mock = MockContainerClient::new();
        mock.inspect_container_results
            .lock()
            .unwrap()
            .push_back(Ok(make_inspection("id1", Some("unhealthy"))));

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "false".into()],
            interval: 1,
            timeout: 1,
            retries: 1,
            start_period: 0,
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &null_ctx(),
        )
        .await;
        assert!(
            matches!(result, Err(OrchestratorError::HealthCheckTimeout(ref name)) if name == "db"),
            "unexpected: {result:?}"
        );
    }

    #[tokio::test]
    async fn wait_for_healthy_timeout_fails() {
        let mock = MockContainerClient::new();
        // Return "starting" indefinitely — each call pops from queue, then RuntimeNotRunning
        for _ in 0..100 {
            mock.inspect_container_results
                .lock()
                .unwrap()
                .push_back(Ok(make_inspection("id1", Some("starting"))));
        }

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "true".into()],
            interval: 1,
            timeout: 1,
            retries: 1,
            start_period: 0,
        };

        // Use tokio::time::pause for deterministic time control
        tokio::time::pause();
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &null_ctx(),
        )
        .await;
        assert!(
            matches!(result, Err(OrchestratorError::HealthCheckTimeout(ref name)) if name == "db"),
            "unexpected: {result:?}"
        );
    }

    // --- Essential container monitoring tests ---

    #[tokio::test]
    async fn watch_essential_exit_returns_exit_code() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 0 }));

        let result = watch_essential_exit(&mock, "id1", "app").await;
        assert_eq!(result.container_name, "app");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn watch_essential_exit_with_nonzero_code() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 137 }));

        let result = watch_essential_exit(&mock, "id1", "web").await;
        assert_eq!(result.container_name, "web");
        assert_eq!(result.exit_code, 137);
    }

    #[tokio::test]
    async fn wait_for_condition_healthy_without_health_check() {
        let mock = MockContainerClient::new();
        let result = wait_for_condition(
            &mock,
            "id1",
            "app",
            DependencyCondition::Healthy,
            None,
            &null_ctx(),
        )
        .await;
        assert!(
            matches!(result, Err(OrchestratorError::ConditionNotMet(ref name, _)) if name == "app"),
            "unexpected: {result:?}"
        );
    }

    #[tokio::test]
    async fn watch_essential_exit_handles_error() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Err(crate::container::ContainerError::RuntimeNotRunning));

        let result = watch_essential_exit(&mock, "id1", "app").await;
        assert_eq!(result.container_name, "app");
        assert_eq!(result.exit_code, -1);
    }

    #[test]
    fn orchestrator_error_display() {
        let err = OrchestratorError::CyclicDependency("a -> b -> a".to_string());
        assert_eq!(err.to_string(), "cyclic dependency detected: a -> b -> a");

        let err = OrchestratorError::ConditionNotMet("app".to_string(), "timeout".to_string());
        assert!(err.to_string().contains("app"));

        let err = OrchestratorError::HealthCheckTimeout("db".to_string());
        assert!(err.to_string().contains("db"));
    }

    #[test]
    fn format_cycle_path_short_input() {
        assert_eq!(format_cycle_path(&[]), "unknown cycle");
        assert_eq!(format_cycle_path(&["a"]), "unknown cycle");
    }

    // --- Property-based tests for DAG resolution ---

    mod pbt {
        use super::*;
        use proptest::prelude::*;
        use std::collections::{HashMap, HashSet};

        /// Generate a list of unique container names (`1..=max_nodes`).
        fn arb_container_names(max_nodes: usize) -> impl Strategy<Value = Vec<String>> {
            (1..=max_nodes).prop_flat_map(|n| {
                proptest::collection::vec("[a-z][a-z0-9]{0,7}", n..=n).prop_map(|names| {
                    // Deduplicate
                    let mut seen = HashSet::new();
                    let mut unique = Vec::new();
                    for (i, name) in names.into_iter().enumerate() {
                        let candidate = if seen.contains(&name) {
                            format!("{name}{i}")
                        } else {
                            name
                        };
                        seen.insert(candidate.clone());
                        unique.push(candidate);
                    }
                    unique
                })
            })
        }

        /// Generate a valid DAG (no cycles): for each node, only depend on nodes
        /// with a smaller index (guarantees topological ordering).
        fn arb_dag(max_nodes: usize) -> impl Strategy<Value = Vec<DependencyInfo>> {
            arb_container_names(max_nodes).prop_flat_map(|names| {
                let n = names.len();
                let deps_strategies: Vec<_> = (0..n)
                    .map(|i| {
                        if i == 0 {
                            // First node cannot depend on anything
                            Just(vec![]).boxed()
                        } else {
                            // Depend on a subset of earlier nodes (0 to min(i, 3) deps)
                            let max_deps = i.min(3);
                            proptest::collection::vec(0..i, 0..=max_deps)
                                .prop_map(|indices| {
                                    let mut seen = HashSet::new();
                                    indices
                                        .into_iter()
                                        .filter(|idx| seen.insert(*idx))
                                        .collect::<Vec<_>>()
                                })
                                .boxed()
                        }
                    })
                    .collect();

                (Just(names), deps_strategies).prop_map(|(names, dep_indices_vec)| {
                    names
                        .iter()
                        .enumerate()
                        .map(|(i, name)| DependencyInfo {
                            name: name.clone(),
                            depends_on: dep_indices_vec[i]
                                .iter()
                                .map(|&idx| DependsOn {
                                    container_name: names[idx].clone(),
                                    condition: DependencyCondition::Start,
                                })
                                .collect(),
                        })
                        .collect()
                })
            })
        }

        /// Generate a dependency graph that contains at least one cycle.
        fn arb_cyclic_graph() -> impl Strategy<Value = Vec<DependencyInfo>> {
            arb_container_names(6).prop_flat_map(|names| {
                let n = names.len();
                if n < 2 {
                    // Need at least 2 nodes for a cycle
                    return Just(vec![
                        DependencyInfo {
                            name: "cyc_a".into(),
                            depends_on: vec![DependsOn {
                                container_name: "cyc_b".into(),
                                condition: DependencyCondition::Start,
                            }],
                        },
                        DependencyInfo {
                            name: "cyc_b".into(),
                            depends_on: vec![DependsOn {
                                container_name: "cyc_a".into(),
                                condition: DependencyCondition::Start,
                            }],
                        },
                    ])
                    .boxed();
                }

                // Pick a cycle length (2..=n), then create a cycle among
                // the first `cycle_len` nodes and leave the rest independent.
                (2..=n)
                    .prop_flat_map(move |cycle_len| {
                        let names = names.clone();
                        Just(
                            names
                                .iter()
                                .enumerate()
                                .map(|(i, name)| {
                                    if i < cycle_len {
                                        let dep_idx = (i + 1) % cycle_len;
                                        DependencyInfo {
                                            name: name.clone(),
                                            depends_on: vec![DependsOn {
                                                container_name: names[dep_idx].clone(),
                                                condition: DependencyCondition::Start,
                                            }],
                                        }
                                    } else {
                                        DependencyInfo {
                                            name: name.clone(),
                                            depends_on: vec![],
                                        }
                                    }
                                })
                                .collect(),
                        )
                    })
                    .boxed()
            })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            /// Property: A valid DAG always produces a successful result.
            #[test]
            fn dag_always_resolves(deps in arb_dag(8)) {
                let result = resolve_start_order(&deps);
                prop_assert!(result.is_ok(), "DAG should resolve but got: {:?}", result.err());
            }

            /// Property: All nodes appear in exactly one layer.
            #[test]
            fn all_nodes_in_exactly_one_layer(deps in arb_dag(8)) {
                let layers = resolve_start_order(&deps).expect("should resolve");
                let all_names: Vec<&str> = layers.iter().flat_map(|l| l.iter().map(String::as_str)).collect();
                let unique: HashSet<&str> = all_names.iter().copied().collect();

                // No duplicates
                prop_assert_eq!(all_names.len(), unique.len(), "duplicate nodes in layers");

                // All input nodes present
                for d in &deps {
                    prop_assert!(unique.contains(d.name.as_str()), "missing node: {}", d.name);
                }
            }

            /// Property: Dependencies are satisfied — every node appears in a
            /// layer after all its dependencies.
            #[test]
            fn dependencies_precede_dependents(deps in arb_dag(8)) {
                let layers = resolve_start_order(&deps).expect("should resolve");

                // Build name -> layer index map
                let mut layer_of: HashMap<&str, usize> = HashMap::new();
                for (li, layer) in layers.iter().enumerate() {
                    for name in layer {
                        layer_of.insert(name.as_str(), li);
                    }
                }

                for d in &deps {
                    let my_layer = layer_of[d.name.as_str()];
                    for dep in &d.depends_on {
                        if let Some(&dep_layer) = layer_of.get(dep.container_name.as_str()) {
                            prop_assert!(
                                dep_layer < my_layer,
                                "container '{}' (layer {}) depends on '{}' (layer {}), but dependency is not in an earlier layer",
                                d.name, my_layer, dep.container_name, dep_layer
                            );
                        }
                    }
                }
            }

            /// Property: Nodes within the same layer have no mutual dependencies.
            #[test]
            fn same_layer_nodes_are_independent(deps in arb_dag(8)) {
                let layers = resolve_start_order(&deps).expect("should resolve");
                let dep_map: HashMap<&str, HashSet<&str>> = deps.iter().map(|d| {
                    (d.name.as_str(), d.depends_on.iter().map(|dep| dep.container_name.as_str()).collect())
                }).collect();

                for layer in &layers {
                    let layer_set: HashSet<&str> = layer.iter().map(String::as_str).collect();
                    for name in layer {
                        if let Some(my_deps) = dep_map.get(name.as_str()) {
                            for dep_name in my_deps {
                                prop_assert!(
                                    !layer_set.contains(dep_name),
                                    "'{}' and its dependency '{}' are in the same layer",
                                    name, dep_name
                                );
                            }
                        }
                    }
                }
            }

            /// Property: Each layer is sorted (deterministic output).
            #[test]
            fn layers_are_sorted(deps in arb_dag(8)) {
                let layers = resolve_start_order(&deps).expect("should resolve");
                for layer in &layers {
                    let mut sorted = layer.clone();
                    sorted.sort();
                    prop_assert_eq!(layer, &sorted, "layer should be sorted");
                }
            }

            /// Property: A graph with a cycle always returns CyclicDependency error.
            #[test]
            fn cyclic_graph_always_fails(deps in arb_cyclic_graph()) {
                let result = resolve_start_order(&deps);
                prop_assert!(
                    matches!(result, Err(OrchestratorError::CyclicDependency(_))),
                    "cyclic graph should fail with CyclicDependency, got: {:?}",
                    result
                );
            }

            /// Property: Empty input produces empty output.
            #[test]
            fn empty_input_empty_output(_seed in 0u32..100u32) {
                let result = resolve_start_order(&[]).expect("empty should resolve");
                prop_assert!(result.is_empty());
            }
        }
    }

    fn make_inspection(id: &str, health_status: Option<&str>) -> ContainerInspection {
        ContainerInspection {
            id: id.into(),
            state: ContainerState {
                status: "running".into(),
                running: true,
                exit_code: None,
                health_status: health_status.map(String::from),
            },
            image: String::new(),
            env: vec![],
            network_name: None,
            ports: vec![],
            started_at: None,
            labels: HashMap::new(),
        }
    }

    // --- orchestrate_startup tests ---

    use crate::container::ContainerConfig;

    fn make_spec(name: &str, depends: &[(&str, DependencyCondition)]) -> ContainerSpec {
        ContainerSpec {
            name: name.to_string(),
            config: ContainerConfig {
                name: format!("test-{name}"),
                image: "alpine:latest".to_string(),
                network: "lecs-test".to_string(),
                network_aliases: vec![name.to_string()],
                ..Default::default()
            },
            depends_on: depends
                .iter()
                .map(|(n, c)| DependsOn {
                    container_name: n.to_string(),
                    condition: *c,
                })
                .collect(),
            health_check: None,
            essential: true,
        }
    }

    #[tokio::test]
    async fn orchestrate_startup_no_dependencies() {
        let mock = MockContainerClient::new();
        // Two containers, no deps — both in layer 0
        for name in &["c1", "c2"] {
            mock.pull_image_results.lock().unwrap().push_back(Ok(()));
            mock.create_container_results
                .lock()
                .unwrap()
                .push_back(Ok(format!("id-{name}")));
            mock.start_container_results
                .lock()
                .unwrap()
                .push_back(Ok(()));
        }

        let specs = vec![make_spec("a", &[]), make_spec("b", &[])];
        let result = orchestrate_startup(&mock, specs, &NullEventSink)
            .await
            .unwrap();
        assert_eq!(result.started.len(), 2);
    }

    #[tokio::test]
    async fn orchestrate_startup_linear_chain() {
        let mock = MockContainerClient::new();
        // a -> b (START condition)
        for name in &["a", "b"] {
            mock.pull_image_results.lock().unwrap().push_back(Ok(()));
            mock.create_container_results
                .lock()
                .unwrap()
                .push_back(Ok(format!("id-{name}")));
            mock.start_container_results
                .lock()
                .unwrap()
                .push_back(Ok(()));
        }

        let specs = vec![
            make_spec("a", &[]),
            make_spec("b", &[("a", DependencyCondition::Start)]),
        ];
        let result = orchestrate_startup(&mock, specs, &NullEventSink)
            .await
            .unwrap();
        assert_eq!(result.started.len(), 2);
        assert_eq!(result.started[0].1, "a");
        assert_eq!(result.started[1].1, "b");
    }

    #[tokio::test]
    async fn orchestrate_startup_healthy_condition() {
        let mock = MockContainerClient::new();
        // db (with healthcheck) -> app (HEALTHY)
        for name in &["db", "app"] {
            mock.pull_image_results.lock().unwrap().push_back(Ok(()));
            mock.create_container_results
                .lock()
                .unwrap()
                .push_back(Ok(format!("id-{name}")));
            mock.start_container_results
                .lock()
                .unwrap()
                .push_back(Ok(()));
        }
        // Health check poll: healthy immediately
        mock.inspect_container_results
            .lock()
            .unwrap()
            .push_back(Ok(make_inspection("id-db", Some("healthy"))));

        let mut db_spec = make_spec("db", &[]);
        db_spec.health_check = Some(HealthCheck {
            command: vec!["CMD-SHELL".into(), "pg_isready".into()],
            interval: 1,
            timeout: 1,
            retries: 3,
            start_period: 0,
        });

        let app_spec = make_spec("app", &[("db", DependencyCondition::Healthy)]);

        tokio::time::pause();
        let result = orchestrate_startup(&mock, vec![db_spec, app_spec], &NullEventSink)
            .await
            .unwrap();
        assert_eq!(result.started.len(), 2);
        assert_eq!(result.started[0].1, "db");
        assert_eq!(result.started[1].1, "app");
    }

    #[tokio::test]
    async fn orchestrate_startup_cycle_detected() {
        let specs = vec![
            make_spec("a", &[("b", DependencyCondition::Start)]),
            make_spec("b", &[("a", DependencyCondition::Start)]),
        ];
        let mock = MockContainerClient::new();
        let err = orchestrate_startup(&mock, specs, &NullEventSink)
            .await
            .unwrap_err();
        assert!(
            matches!(err.1, OrchestratorError::CyclicDependency(_)),
            "unexpected: {:?}",
            err.1
        );
        assert!(err.0.started.is_empty());
    }

    #[tokio::test]
    async fn orchestrate_startup_image_pull_failure() {
        let mock = MockContainerClient::new();
        mock.pull_image_results
            .lock()
            .unwrap()
            .push_back(Err(crate::container::ContainerError::RuntimeNotRunning));

        let specs = vec![make_spec("a", &[])];
        let err = orchestrate_startup(&mock, specs, &NullEventSink)
            .await
            .unwrap_err();
        assert!(
            matches!(err.1, OrchestratorError::Runtime(_)),
            "unexpected: {:?}",
            err.1
        );
        assert!(err.0.started.is_empty());
    }

    // --- Event emission tests ---

    #[tokio::test]
    async fn wait_for_condition_complete_emits_exited_event() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 42 }));

        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "worker",
            DependencyCondition::Complete,
            None,
            &ctx,
        )
        .await;
        assert!(result.is_ok());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::Exited));
        assert_eq!(events[0].container_name.as_deref(), Some("worker"));
        assert_eq!(events[0].family, "my-app");
        assert_eq!(events[0].details.as_deref(), Some("exit code: 42"));
    }

    #[tokio::test]
    async fn wait_for_condition_success_emits_exited_event() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 0 }));

        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "init",
            DependencyCondition::Success,
            None,
            &ctx,
        )
        .await;
        assert!(result.is_ok());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::Exited));
        assert_eq!(events[0].details.as_deref(), Some("exit code: 0"));
    }

    #[tokio::test]
    async fn wait_for_condition_success_nonzero_emits_exited_event() {
        let mock = MockContainerClient::new();
        mock.wait_container_results
            .lock()
            .unwrap()
            .push_back(Ok(WaitResult { status_code: 1 }));

        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "init",
            DependencyCondition::Success,
            None,
            &ctx,
        )
        .await;
        assert!(result.is_err());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::Exited));
        assert_eq!(events[0].details.as_deref(), Some("exit code: 1"));
    }

    #[tokio::test]
    async fn wait_for_healthy_emits_health_check_passed() {
        let mock = MockContainerClient::new();
        mock.inspect_container_results
            .lock()
            .unwrap()
            .push_back(Ok(make_inspection("id1", Some("healthy"))));

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "true".into()],
            interval: 1,
            timeout: 1,
            retries: 3,
            start_period: 0,
        };
        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &ctx,
        )
        .await;
        assert!(result.is_ok());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::HealthCheckPassed));
        assert_eq!(events[0].container_name.as_deref(), Some("db"));
        assert_eq!(events[0].family, "my-app");
    }

    #[tokio::test]
    async fn wait_for_unhealthy_emits_health_check_failed() {
        let mock = MockContainerClient::new();
        mock.inspect_container_results
            .lock()
            .unwrap()
            .push_back(Ok(make_inspection("id1", Some("unhealthy"))));

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "false".into()],
            interval: 1,
            timeout: 1,
            retries: 1,
            start_period: 0,
        };
        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &ctx,
        )
        .await;
        assert!(result.is_err());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::HealthCheckFailed));
        assert_eq!(events[0].container_name.as_deref(), Some("db"));
        assert_eq!(events[0].details.as_deref(), Some("status: unhealthy"));
    }

    #[tokio::test]
    async fn wait_for_healthy_timeout_emits_health_check_failed() {
        let mock = MockContainerClient::new();
        for _ in 0..100 {
            mock.inspect_container_results
                .lock()
                .unwrap()
                .push_back(Ok(make_inspection("id1", Some("starting"))));
        }

        let hc = HealthCheck {
            command: vec!["CMD-SHELL".into(), "true".into()],
            interval: 1,
            timeout: 1,
            retries: 1,
            start_period: 0,
        };

        tokio::time::pause();
        let sink = CollectingEventSink::new();
        let ctx = EventContext {
            event_sink: &sink,
            family: "my-app",
        };
        let result = wait_for_condition(
            &mock,
            "id1",
            "db",
            DependencyCondition::Healthy,
            Some(&hc),
            &ctx,
        )
        .await;
        assert!(result.is_err());

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].event_type, EventType::HealthCheckFailed));
        assert_eq!(events[0].details.as_deref(), Some("timed out"));
    }

    #[tokio::test]
    async fn orchestrate_startup_emits_created_and_started_events() {
        let mock = MockContainerClient::new();
        mock.pull_image_results.lock().unwrap().push_back(Ok(()));
        mock.create_container_results
            .lock()
            .unwrap()
            .push_back(Ok("id-a".to_string()));
        mock.start_container_results
            .lock()
            .unwrap()
            .push_back(Ok(()));

        let specs = vec![make_spec("a", &[])];
        let sink = CollectingEventSink::new();
        let result = orchestrate_startup(&mock, specs, &sink).await.unwrap();
        assert_eq!(result.started.len(), 1);

        let events = sink.events();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0].event_type, EventType::ImagePulled));
        assert!(matches!(events[1].event_type, EventType::Created));
        assert!(matches!(events[2].event_type, EventType::Started));
    }
}
