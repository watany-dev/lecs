//! OCI container runtime client integration.

use std::collections::HashMap;

use bollard::Docker;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StatsOptions, StopContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::models::{EndpointSettings, HealthConfig, HostConfig, PortBinding, ResourcesUlimits};
use bollard::network::{CreateNetworkOptions, ListNetworksOptions};
use futures_util::Stream;
use futures_util::StreamExt;
use futures_util::TryStreamExt;

/// Container runtime errors.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("Container runtime is not running. Please start Docker or Podman and try again.")]
    RuntimeNotRunning,

    #[error("Container runtime API error: {0}")]
    Api(#[from] bollard::errors::Error),
}

/// Abstraction over container runtime operations for testability.
pub trait ContainerRuntime: Send + Sync {
    /// Create an Lecs network. Reuses if it already exists.
    async fn create_network(&self, family: &str) -> Result<String, ContainerError>;

    /// Remove a network by name.
    async fn remove_network(&self, name: &str) -> Result<(), ContainerError>;

    /// List Lecs-managed networks, optionally filtered by task family.
    async fn list_networks(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<NetworkInfo>, ContainerError>;

    /// Create a container (does not start it). Returns the container ID.
    async fn create_container(&self, config: &ContainerConfig) -> Result<String, ContainerError>;

    /// Start a container by ID.
    async fn start_container(&self, id: &str) -> Result<(), ContainerError>;

    /// Stop a container by ID with an optional timeout in seconds.
    async fn stop_container(
        &self,
        id: &str,
        timeout_secs: Option<u32>,
    ) -> Result<(), ContainerError>;

    /// Remove a container by ID.
    async fn remove_container(&self, id: &str) -> Result<(), ContainerError>;

    /// List Lecs-managed containers, optionally filtered by task family.
    async fn list_containers(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<ContainerInfo>, ContainerError>;

    /// Inspect a container and return its current state.
    async fn inspect_container(&self, id: &str) -> Result<ContainerInspection, ContainerError>;

    /// Wait for a container to exit. Returns the exit status code.
    async fn wait_container(&self, id: &str) -> Result<WaitResult, ContainerError>;

    /// Get a single snapshot of resource usage statistics for a container.
    async fn stats_container(&self, id: &str) -> Result<ContainerStats, ContainerError>;

    /// Execute a command inside a running container.
    async fn exec_container(&self, id: &str, cmd: &[String]) -> Result<ExecResult, ContainerError>;

    /// Pull a container image from a registry.
    ///
    /// The `image` parameter is the full image reference (e.g., `nginx:latest`,
    /// `registry.example.com/app:v1.0`). The implementation should handle
    /// splitting the reference into repository and tag components.
    async fn pull_image(&self, image: &str) -> Result<(), ContainerError>;
}

/// Result of executing a command inside a container.
pub struct ExecResult {
    /// Exit code from the executed command (None if not available).
    pub exit_code: Option<i64>,
}

/// Lecs container runtime client wrapping bollard.
pub struct ContainerClient {
    docker: Docker,
}

/// Container creation configuration.
#[derive(Default)]
pub struct ContainerConfig {
    pub name: String,
    pub image: String,
    pub command: Vec<String>,
    pub entry_point: Vec<String>,
    pub env: Vec<String>,
    pub port_mappings: Vec<PortMappingConfig>,
    pub network: String,
    pub network_aliases: Vec<String>,
    pub labels: HashMap<String, String>,
    /// Extra host-to-IP mappings (e.g., `host.docker.internal:host-gateway`).
    pub extra_hosts: Vec<String>,
    /// Docker HEALTHCHECK configuration.
    pub health_check: Option<HealthCheckConfig>,
    /// Bind mount volumes (format: `host_path:container_path` or `host_path:container_path:ro`).
    pub binds: Vec<String>,
    /// Working directory inside the container.
    pub working_dir: Option<String>,
    /// User to run the container as.
    pub user: Option<String>,
    /// CPU units (1024 = 1 vCPU).
    pub cpu_units: Option<u32>,
    /// Hard memory limit (MiB).
    pub memory_mib: Option<u32>,
    /// Soft memory limit (MiB).
    pub memory_reservation_mib: Option<u32>,
    /// Resource limits (ulimits) for the container.
    pub ulimits: Vec<UlimitConfig>,
    /// Run an init process inside the container.
    pub init: Option<bool>,
    /// Size of `/dev/shm` in bytes.
    pub shm_size: Option<i64>,
    /// Tmpfs mounts (path → mount options string).
    pub tmpfs: HashMap<String, String>,
}

/// Resource limit (ulimit) configuration for a container.
pub struct UlimitConfig {
    /// Ulimit name (e.g., "nofile", "memlock").
    pub name: String,
    /// Soft limit.
    pub soft: i64,
    /// Hard limit.
    pub hard: i64,
}

/// Docker HEALTHCHECK configuration (nanosecond units).
pub struct HealthCheckConfig {
    /// Health check command (e.g. `["CMD-SHELL", "curl -f http://localhost/"]`).
    pub test: Vec<String>,
    /// Interval between health checks in nanoseconds.
    pub interval_ns: i64,
    /// Timeout for each check in nanoseconds.
    pub timeout_ns: i64,
    /// Number of consecutive failures before marking unhealthy.
    pub retries: i64,
    /// Grace period before health checks start in nanoseconds.
    pub start_period_ns: i64,
}

/// Port mapping configuration.
pub struct PortMappingConfig {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: String,
}

/// Port information for a running container.
#[derive(Debug, Clone)]
pub struct PortInfo {
    pub host_port: Option<u16>,
    pub container_port: u16,
    pub protocol: String,
}

/// Resource usage statistics for a container.
pub struct ContainerStats {
    pub cpu_percent: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
}

/// Information about an Lecs-managed container.
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    /// Container image name.
    pub image: String,
    pub family: String,
    pub state: String,
    /// Health check status (e.g., "healthy", "unhealthy", "starting").
    pub health_status: Option<String>,
    /// Port mappings for the container.
    pub ports: Vec<PortInfo>,
    /// ISO 8601 timestamp when the container was started.
    pub started_at: Option<String>,
}

/// Information about an Lecs-managed network.
pub struct NetworkInfo {
    #[allow(dead_code)] // Populated by Docker API; retained for API completeness
    pub id: String,
    pub name: String,
}

/// Result of inspecting a container.
pub struct ContainerInspection {
    #[allow(dead_code)] // Populated by Docker API; retained for API completeness
    pub id: String,
    pub state: ContainerState,
    /// Container image name.
    pub image: String,
    /// Environment variables set on the container.
    pub env: Vec<String>,
    /// Network name the container is attached to.
    pub network_name: Option<String>,
    /// Port mappings for the container.
    pub ports: Vec<PortInfo>,
    /// ISO 8601 timestamp when the container was started.
    pub started_at: Option<String>,
    /// Container labels.
    pub labels: HashMap<String, String>,
}

/// Container state from inspection.
pub struct ContainerState {
    /// Status string (e.g., "running", "exited").
    pub status: String,
    /// Whether the container is running.
    #[allow(dead_code)] // Populated by Docker API; retained for API completeness
    pub running: bool,
    /// Exit code (if exited).
    #[allow(dead_code)] // Populated by Docker API; retained for API completeness
    pub exit_code: Option<i64>,
    /// Health check status (e.g., "healthy", "unhealthy", "starting").
    pub health_status: Option<String>,
}

/// Result of waiting for a container to exit.
pub struct WaitResult {
    pub status_code: i64,
}

/// Host URL scheme classification.
#[derive(Debug, PartialEq, Eq)]
enum HostScheme {
    Unix,
    Tcp,
}

/// Parse a host URL into its scheme and address.
///
/// - `unix:///path` → `(Unix, "/path")`
/// - `tcp://host:port` → `(Tcp, "host:port")`
/// - `/path` (bare path) → `(Unix, "/path")`
fn parse_host_url(url: &str) -> (HostScheme, &str) {
    url.strip_prefix("unix://").map_or_else(
        || {
            url.strip_prefix("tcp://")
                .map_or((HostScheme::Unix, url), |addr| (HostScheme::Tcp, addr))
        },
        |path| (HostScheme::Unix, path),
    )
}

/// Parse an image reference into (repository, tag) components.
///
/// Handles the following formats:
/// - `nginx` → `("nginx", "latest")`
/// - `nginx:1.25` → `("nginx", "1.25")`
/// - `registry.example.com:5000/app` → `("registry.example.com:5000/app", "latest")`
/// - `registry.example.com:5000/app:v1` → `("registry.example.com:5000/app", "v1")`
/// - `nginx@sha256:abc...` → `("nginx@sha256:abc...", "")` (digest reference, no tag)
fn parse_image_reference(image: &str) -> (&str, &str) {
    // Digest reference — pass through as-is with empty tag
    if image.contains('@') {
        return (image, "");
    }

    // Find the last colon that is part of the tag (not a port number).
    // A colon in a tag comes after the last `/` (if any).
    let after_last_slash = image.rfind('/').map_or(0, |i| i + 1);
    image[after_last_slash..]
        .rfind(':')
        .map_or((image, "latest"), |offset| {
            let colon_pos = after_last_slash + offset;
            (&image[..colon_pos], &image[colon_pos + 1..])
        })
}

/// Return candidate socket paths for Podman runtime connection.
///
/// Docker default paths are handled by bollard's `connect_with_local_defaults`.
fn podman_socket_candidates() -> Vec<String> {
    let mut candidates = Vec::new();

    // Rootless Podman (XDG Base Directory Specification)
    if let Ok(xdg_dir) = std::env::var("XDG_RUNTIME_DIR") {
        candidates.push(format!("{xdg_dir}/podman/podman.sock"));
    } else if let Some(uid) = current_uid() {
        // Fallback: derive rootless socket path from UID.
        // Covers sudo, cron, non-login shells, or separate terminal sessions
        // where XDG_RUNTIME_DIR is not propagated.
        candidates.push(format!("/run/user/{uid}/podman/podman.sock"));
    }

    // Rootful Podman
    candidates.push("/run/podman/podman.sock".to_string());

    candidates
}

/// Retrieve the current process owner's UID from `/proc/self` metadata.
///
/// Returns `None` on non-Linux platforms or if `/proc` is unavailable.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Stub for non-Unix platforms where rootless Podman is not applicable.
#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

#[cfg(not(tarpaulin_include))]
impl ContainerClient {
    /// Connect to the container runtime.
    ///
    /// Priority:
    /// 1. Explicit host (`--host` flag or `CONTAINER_HOST` env)
    /// 2. `DOCKER_HOST` env / Docker default sockets (via bollard)
    /// 3. Podman socket auto-detection (rootless → rootful)
    pub async fn connect(host: Option<&str>) -> Result<Self, ContainerError> {
        // 1. Explicit host specified
        if let Some(url) = host {
            return Self::connect_to_host(url).await;
        }

        // 2. Bollard defaults (DOCKER_HOST env, Docker standard sockets)
        if let Ok(docker) = Docker::connect_with_local_defaults()
            && docker.ping().await.is_ok()
        {
            return Ok(Self { docker });
        }

        // 3. Podman socket auto-detection
        for candidate in podman_socket_candidates() {
            let path = std::path::Path::new(&candidate);
            if path.exists()
                && let Ok(docker) =
                    Docker::connect_with_unix(&candidate, 120, bollard::API_DEFAULT_VERSION)
                && docker.ping().await.is_ok()
            {
                tracing::info!(socket = %candidate, "Connected to container runtime via Podman socket");
                return Ok(Self { docker });
            }
        }

        Err(ContainerError::RuntimeNotRunning)
    }

    /// Connect to a specific host URL.
    async fn connect_to_host(url: &str) -> Result<Self, ContainerError> {
        let docker = match parse_host_url(url) {
            (HostScheme::Unix, path) => {
                Docker::connect_with_unix(path, 120, bollard::API_DEFAULT_VERSION)
                    .map_err(|_| ContainerError::RuntimeNotRunning)?
            }
            (HostScheme::Tcp, addr) => {
                tracing::warn!(
                    addr,
                    "Connecting to Docker daemon over unencrypted HTTP — \
                     credentials and container data may be exposed on the network"
                );
                let http_url = format!("http://{addr}");
                Docker::connect_with_http(&http_url, 120, bollard::API_DEFAULT_VERSION)
                    .map_err(|_| ContainerError::RuntimeNotRunning)?
            }
        };
        docker
            .ping()
            .await
            .map_err(|_| ContainerError::RuntimeNotRunning)?;
        Ok(Self { docker })
    }

    /// Stream container logs.
    ///
    /// When `follow` is `true` the stream stays open (like `docker logs -f`).
    /// When `false` it returns existing logs and ends.
    pub fn stream_logs(
        &self,
        id: &str,
        follow: bool,
    ) -> impl Stream<Item = Result<String, ContainerError>> + '_ {
        self.docker
            .logs(
                id,
                Some(LogsOptions::<String> {
                    follow,
                    stdout: true,
                    stderr: true,
                    ..Default::default()
                }),
            )
            .map(|result| {
                result
                    .map(|output| output.to_string())
                    .map_err(ContainerError::from)
            })
    }
}

#[cfg(not(tarpaulin_include))]
impl ContainerRuntime for ContainerClient {
    async fn create_network(&self, family: &str) -> Result<String, ContainerError> {
        let name = format!("lecs-{family}");

        let labels = HashMap::from([
            (crate::labels::MANAGED, "true"),
            (crate::labels::TASK, family),
        ]);

        // Check if network already exists
        let existing = self
            .docker
            .list_networks(Some(ListNetworksOptions {
                filters: HashMap::from([("name".to_string(), vec![name.clone()])]),
            }))
            .await?;

        for net in &existing {
            if net.name.as_deref() == Some(&name) {
                return Ok(name);
            }
        }

        self.docker
            .create_network(CreateNetworkOptions {
                name: name.as_str(),
                driver: "bridge",
                labels,
                ..Default::default()
            })
            .await?;

        Ok(name)
    }

    async fn remove_network(&self, name: &str) -> Result<(), ContainerError> {
        self.docker.remove_network(name).await?;
        Ok(())
    }

    async fn list_networks(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<NetworkInfo>, ContainerError> {
        let mut label_filters = vec![format!("{}=true", crate::labels::MANAGED)];
        if let Some(family) = task_filter {
            label_filters.push(format!("{}={family}", crate::labels::TASK));
        }

        let networks = self
            .docker
            .list_networks(Some(ListNetworksOptions {
                filters: HashMap::from([("label".to_string(), label_filters)]),
            }))
            .await?;

        Ok(networks
            .into_iter()
            .filter_map(|n| {
                Some(NetworkInfo {
                    id: n.id?,
                    name: n.name?,
                })
            })
            .collect())
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String, ContainerError> {
        let container_config = build_bollard_config(config);

        let response = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: config.name.as_str(),
                    platform: None,
                }),
                container_config,
            )
            .await?;

        Ok(response.id)
    }

    async fn start_container(&self, id: &str) -> Result<(), ContainerError> {
        self.docker.start_container::<String>(id, None).await?;
        Ok(())
    }

    async fn stop_container(
        &self,
        id: &str,
        timeout_secs: Option<u32>,
    ) -> Result<(), ContainerError> {
        let t = timeout_secs.map_or(30, i64::from);
        self.docker
            .stop_container(id, Some(StopContainerOptions { t }))
            .await?;
        Ok(())
    }

    async fn remove_container(&self, id: &str) -> Result<(), ContainerError> {
        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }

    async fn list_containers(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<ContainerInfo>, ContainerError> {
        let mut label_filters = vec![format!("{}=true", crate::labels::MANAGED)];
        if let Some(family) = task_filter {
            label_filters.push(format!("{}={family}", crate::labels::TASK));
        }

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: HashMap::from([("label".to_string(), label_filters)]),
                ..Default::default()
            }))
            .await?;

        Ok(containers
            .into_iter()
            .filter_map(|c| {
                let labels = c.labels.as_ref()?;
                let ports = c
                    .ports
                    .as_ref()
                    .map(|ports| {
                        ports
                            .iter()
                            .map(|p| PortInfo {
                                host_port: p.public_port,
                                container_port: p.private_port,
                                protocol: p.typ.as_ref().map_or_else(
                                    || "tcp".to_string(),
                                    |t| format!("{t:?}").to_lowercase(),
                                ),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(ContainerInfo {
                    id: c.id?,
                    name: c
                        .names
                        .as_ref()
                        .and_then(|n| n.first())
                        .map(|n| n.trim_start_matches('/').to_string())
                        .unwrap_or_default(),
                    image: c.image.unwrap_or_default(),
                    family: labels.get(crate::labels::TASK).cloned().unwrap_or_default(),
                    state: c.state.unwrap_or_default(),
                    health_status: None, // populated via inspect when needed
                    ports,
                    started_at: None, // populated via inspect when needed
                })
            })
            .collect())
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerInspection, ContainerError> {
        let resp = self.docker.inspect_container(id, None).await?;
        let state = resp.state.as_ref();
        let config = resp.config.as_ref();

        let ports = extract_inspect_ports(&resp);
        let started_at = state
            .and_then(|s| s.started_at.clone())
            .filter(|s| !s.is_empty() && s != "0001-01-01T00:00:00Z");
        let network_name = resp
            .network_settings
            .as_ref()
            .and_then(|ns| ns.networks.as_ref())
            .and_then(|nets| nets.keys().next().cloned());

        Ok(ContainerInspection {
            id: resp.id.unwrap_or_default(),
            state: ContainerState {
                status: state
                    .and_then(|s| s.status.as_ref())
                    .map(|s| format!("{s:?}").to_lowercase())
                    .unwrap_or_default(),
                running: state.and_then(|s| s.running).unwrap_or(false),
                exit_code: state.and_then(|s| s.exit_code),
                health_status: state
                    .and_then(|s| s.health.as_ref())
                    .and_then(|h| h.status.as_ref())
                    .map(|s| format!("{s:?}").to_lowercase()),
            },
            image: config.and_then(|c| c.image.clone()).unwrap_or_default(),
            env: config.and_then(|c| c.env.clone()).unwrap_or_default(),
            network_name,
            ports,
            started_at,
            labels: config.and_then(|c| c.labels.clone()).unwrap_or_default(),
        })
    }

    async fn wait_container(&self, id: &str) -> Result<WaitResult, ContainerError> {
        let response = self
            .docker
            .wait_container::<String>(id, None)
            .next()
            .await
            .ok_or(ContainerError::RuntimeNotRunning)?
            .map_err(ContainerError::from)?;
        Ok(WaitResult {
            status_code: response.status_code,
        })
    }

    async fn stats_container(&self, id: &str) -> Result<ContainerStats, ContainerError> {
        let stats = self
            .docker
            .stats(
                id,
                Some(StatsOptions {
                    stream: false,
                    one_shot: true,
                }),
            )
            .next()
            .await
            .ok_or(ContainerError::RuntimeNotRunning)?
            .map_err(ContainerError::from)?;

        let cpu_percent = calculate_cpu_percent(&stats);
        let memory_usage = stats.memory_stats.usage.unwrap_or(0);
        let memory_limit = stats.memory_stats.limit.unwrap_or(0);

        let (net_rx, net_tx) = stats.networks.as_ref().map_or((0, 0), |nets| {
            nets.values().fold((0u64, 0u64), |(rx, tx), n| {
                (rx + n.rx_bytes, tx + n.tx_bytes)
            })
        });

        let (block_read, block_write) = stats
            .blkio_stats
            .io_service_bytes_recursive
            .as_ref()
            .map_or((0, 0), |entries| {
                entries
                    .iter()
                    .fold((0u64, 0u64), |(r, w), e| match e.op.as_str() {
                        "read" | "Read" => (r + e.value, w),
                        "write" | "Write" => (r, w + e.value),
                        _ => (r, w),
                    })
            });

        Ok(ContainerStats {
            cpu_percent,
            memory_usage,
            memory_limit,
            net_rx_bytes: net_rx,
            net_tx_bytes: net_tx,
            block_read_bytes: block_read,
            block_write_bytes: block_write,
        })
    }

    #[cfg(not(tarpaulin_include))]
    #[allow(clippy::print_stdout, clippy::print_stderr)]
    async fn exec_container(&self, id: &str, cmd: &[String]) -> Result<ExecResult, ContainerError> {
        let exec = self
            .docker
            .create_exec(
                id,
                CreateExecOptions::<String> {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(cmd.to_vec()),
                    ..Default::default()
                },
            )
            .await?;

        let start_result = self.docker.start_exec(&exec.id, None).await?;

        match start_result {
            StartExecResults::Attached { output, .. } => {
                output
                    .try_for_each(|log| async move {
                        match log {
                            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                                print!("{}", String::from_utf8_lossy(&message));
                            }
                            LogOutput::StdErr { message } => {
                                eprint!("{}", String::from_utf8_lossy(&message));
                            }
                            LogOutput::StdIn { .. } => {}
                        }
                        Ok(())
                    })
                    .await?;
            }
            StartExecResults::Detached => {}
        }

        let inspect = self.docker.inspect_exec(&exec.id).await?;
        Ok(ExecResult {
            exit_code: inspect.exit_code,
        })
    }

    async fn pull_image(&self, image: &str) -> Result<(), ContainerError> {
        let (from_image, tag) = parse_image_reference(image);

        let options = CreateImageOptions {
            from_image,
            tag,
            ..Default::default()
        };

        self.docker
            .create_image(Some(options), None, None)
            .try_collect::<Vec<_>>()
            .await?;

        Ok(())
    }
}

/// Calculate CPU usage percentage from a Docker stats snapshot.
#[cfg(not(tarpaulin_include))]
#[allow(clippy::cast_precision_loss, dead_code)]
fn calculate_cpu_percent(stats: &bollard::container::Stats) -> f64 {
    let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as f64
        - stats.precpu_stats.cpu_usage.total_usage as f64;
    let system_delta = stats.cpu_stats.system_cpu_usage.unwrap_or(0) as f64
        - stats.precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
    let num_cpus = stats.cpu_stats.online_cpus.unwrap_or(1) as f64;

    if system_delta > 0.0 && cpu_delta >= 0.0 {
        (cpu_delta / system_delta) * num_cpus * 100.0
    } else {
        0.0
    }
}

/// Extract port mappings from a container inspection response.
#[cfg(not(tarpaulin_include))]
fn extract_inspect_ports(resp: &bollard::models::ContainerInspectResponse) -> Vec<PortInfo> {
    resp.network_settings
        .as_ref()
        .and_then(|ns| ns.ports.as_ref())
        .map(|ports| {
            ports
                .iter()
                .filter_map(|(key, bindings)| {
                    let parts: Vec<&str> = key.split('/').collect();
                    let container_port = parts.first()?.parse::<u16>().ok()?;
                    let protocol = parts.get(1).unwrap_or(&"tcp").to_string();
                    let host_port = bindings
                        .as_ref()
                        .and_then(|b| b.first())
                        .and_then(|b| b.host_port.as_ref())
                        .and_then(|p| p.parse::<u16>().ok());
                    Some(PortInfo {
                        host_port,
                        container_port,
                        protocol,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build the `HostConfig` portion of a bollard container config.
fn build_host_config(
    config: &ContainerConfig,
    port_bindings: HashMap<String, Option<Vec<PortBinding>>>,
) -> HostConfig {
    let extra_hosts = if config.extra_hosts.is_empty() {
        None
    } else {
        Some(config.extra_hosts.clone())
    };

    let binds = if config.binds.is_empty() {
        None
    } else {
        Some(config.binds.clone())
    };

    HostConfig {
        port_bindings: Some(port_bindings),
        extra_hosts,
        binds,
        nano_cpus: config
            .cpu_units
            .map(|cpu| i64::from(cpu) * 1_000_000_000 / 1024),
        memory: config.memory_mib.map(|m| i64::from(m) * 1024 * 1024),
        memory_reservation: config
            .memory_reservation_mib
            .map(|m| i64::from(m) * 1024 * 1024),
        ulimits: if config.ulimits.is_empty() {
            None
        } else {
            Some(
                config
                    .ulimits
                    .iter()
                    .map(|u| ResourcesUlimits {
                        name: Some(u.name.clone()),
                        soft: Some(u.soft),
                        hard: Some(u.hard),
                    })
                    .collect(),
            )
        },
        init: config.init,
        shm_size: config.shm_size,
        tmpfs: if config.tmpfs.is_empty() {
            None
        } else {
            Some(config.tmpfs.clone())
        },
        ..Default::default()
    }
}

/// Build a bollard container `Config` from an Lecs `ContainerConfig`.
///
/// Pure function — no container runtime interaction.
#[allow(clippy::zero_sized_map_values)] // Container API requires HashMap for exposed_ports
pub fn build_bollard_config(config: &ContainerConfig) -> Config<String> {
    let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();

    for pm in &config.port_mappings {
        let container_key = format!("{}/{}", pm.container_port, pm.protocol);
        exposed_ports.insert(container_key.clone(), HashMap::default());
        port_bindings.insert(
            container_key,
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some(pm.host_port.to_string()),
            }]),
        );
    }

    let endpoint_settings = EndpointSettings {
        aliases: Some(config.network_aliases.clone()),
        ..Default::default()
    };

    let networking_config = bollard::container::NetworkingConfig {
        endpoints_config: HashMap::from([(config.network.clone(), endpoint_settings)]),
    };

    let host_config = build_host_config(config, port_bindings);

    let cmd = if config.command.is_empty() {
        None
    } else {
        Some(config.command.clone())
    };

    let entrypoint = if config.entry_point.is_empty() {
        None
    } else {
        Some(config.entry_point.clone())
    };

    let env = if config.env.is_empty() {
        None
    } else {
        Some(config.env.clone())
    };

    let healthcheck = config.health_check.as_ref().map(|hc| HealthConfig {
        test: Some(hc.test.clone()),
        interval: Some(hc.interval_ns),
        timeout: Some(hc.timeout_ns),
        retries: Some(hc.retries),
        start_period: Some(hc.start_period_ns),
        start_interval: None,
    });

    Config {
        image: Some(config.image.clone()),
        cmd,
        entrypoint,
        env,
        exposed_ports: Some(exposed_ports),
        host_config: Some(host_config),
        networking_config: Some(networking_config),
        labels: Some(config.labels.clone()),
        healthcheck,
        working_dir: config.working_dir.clone(),
        user: config.user.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::struct_field_names)]
pub mod test_support {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Mock container runtime client for testing.
    pub struct MockContainerClient {
        pub create_network_results: Mutex<VecDeque<Result<String, ContainerError>>>,
        pub create_container_results: Mutex<VecDeque<Result<String, ContainerError>>>,
        pub start_container_results: Mutex<VecDeque<Result<(), ContainerError>>>,
        pub stop_container_results: Mutex<VecDeque<Result<(), ContainerError>>>,
        pub remove_container_results: Mutex<VecDeque<Result<(), ContainerError>>>,
        pub remove_network_results: Mutex<VecDeque<Result<(), ContainerError>>>,
        pub list_containers_results: Mutex<VecDeque<Result<Vec<ContainerInfo>, ContainerError>>>,
        pub list_networks_results: Mutex<VecDeque<Result<Vec<NetworkInfo>, ContainerError>>>,
        pub inspect_container_results: Mutex<VecDeque<Result<ContainerInspection, ContainerError>>>,
        pub wait_container_results: Mutex<VecDeque<Result<WaitResult, ContainerError>>>,
        pub stats_container_results: Mutex<VecDeque<Result<ContainerStats, ContainerError>>>,
        pub exec_container_results: Mutex<VecDeque<Result<ExecResult, ContainerError>>>,
        pub pull_image_results: Mutex<VecDeque<Result<(), ContainerError>>>,
    }

    impl MockContainerClient {
        pub fn new() -> Self {
            Self {
                create_network_results: Mutex::new(VecDeque::new()),
                create_container_results: Mutex::new(VecDeque::new()),
                start_container_results: Mutex::new(VecDeque::new()),
                stop_container_results: Mutex::new(VecDeque::new()),
                remove_container_results: Mutex::new(VecDeque::new()),
                remove_network_results: Mutex::new(VecDeque::new()),
                list_containers_results: Mutex::new(VecDeque::new()),
                list_networks_results: Mutex::new(VecDeque::new()),
                inspect_container_results: Mutex::new(VecDeque::new()),
                wait_container_results: Mutex::new(VecDeque::new()),
                stats_container_results: Mutex::new(VecDeque::new()),
                exec_container_results: Mutex::new(VecDeque::new()),
                pull_image_results: Mutex::new(VecDeque::new()),
            }
        }
    }

    /// Pop the next result from a mock queue,
    /// returning `ContainerError::RuntimeNotRunning` if the queue is empty.
    fn pop_result<T>(
        queue: &Mutex<VecDeque<Result<T, ContainerError>>>,
    ) -> Result<T, ContainerError> {
        queue
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
            .unwrap_or(Err(ContainerError::RuntimeNotRunning))
    }

    impl ContainerRuntime for MockContainerClient {
        async fn create_network(&self, _family: &str) -> Result<String, ContainerError> {
            pop_result(&self.create_network_results)
        }

        async fn remove_network(&self, _name: &str) -> Result<(), ContainerError> {
            pop_result(&self.remove_network_results)
        }

        async fn list_networks(
            &self,
            _task_filter: Option<&str>,
        ) -> Result<Vec<NetworkInfo>, ContainerError> {
            pop_result(&self.list_networks_results)
        }

        async fn create_container(
            &self,
            _config: &ContainerConfig,
        ) -> Result<String, ContainerError> {
            pop_result(&self.create_container_results)
        }

        async fn start_container(&self, _id: &str) -> Result<(), ContainerError> {
            pop_result(&self.start_container_results)
        }

        async fn stop_container(
            &self,
            _id: &str,
            _timeout_secs: Option<u32>,
        ) -> Result<(), ContainerError> {
            pop_result(&self.stop_container_results)
        }

        async fn remove_container(&self, _id: &str) -> Result<(), ContainerError> {
            pop_result(&self.remove_container_results)
        }

        async fn list_containers(
            &self,
            _task_filter: Option<&str>,
        ) -> Result<Vec<ContainerInfo>, ContainerError> {
            pop_result(&self.list_containers_results)
        }

        async fn inspect_container(
            &self,
            _id: &str,
        ) -> Result<ContainerInspection, ContainerError> {
            pop_result(&self.inspect_container_results)
        }

        async fn wait_container(&self, _id: &str) -> Result<WaitResult, ContainerError> {
            pop_result(&self.wait_container_results)
        }

        async fn stats_container(&self, _id: &str) -> Result<ContainerStats, ContainerError> {
            pop_result(&self.stats_container_results)
        }

        async fn exec_container(
            &self,
            _id: &str,
            _cmd: &[String],
        ) -> Result<ExecResult, ContainerError> {
            pop_result(&self.exec_container_results)
        }

        async fn pull_image(&self, _image: &str) -> Result<(), ContainerError> {
            pop_result(&self.pull_image_results)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_config() -> ContainerConfig {
        ContainerConfig {
            name: "test-app".to_string(),
            image: "nginx:latest".to_string(),
            command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            entry_point: vec!["/entrypoint.sh".into()],
            env: vec!["PORT=8080".to_string()],
            port_mappings: vec![PortMappingConfig {
                host_port: 8080,
                container_port: 80,
                protocol: "tcp".to_string(),
            }],
            network: "lecs-test".to_string(),
            network_aliases: vec!["app".to_string()],
            labels: HashMap::from([(crate::labels::MANAGED.into(), "true".into())]),
            ..Default::default()
        }
    }

    #[test]
    fn build_bollard_config_with_ports() {
        let config = sample_config();
        let result = build_bollard_config(&config);

        // Verify exposed ports key format
        let exposed = result.exposed_ports.as_ref().expect("exposed_ports");
        assert!(exposed.contains_key("80/tcp"));

        // Verify port bindings
        let host_config = result.host_config.as_ref().expect("host_config");
        let bindings = host_config.port_bindings.as_ref().expect("port_bindings");
        let binding = bindings
            .get("80/tcp")
            .expect("80/tcp binding")
            .as_ref()
            .expect("binding vec");
        assert_eq!(binding.len(), 1);
        assert_eq!(binding[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding[0].host_port.as_deref(), Some("8080"));
    }

    #[test]
    fn build_bollard_config_extra_hosts() {
        let mut config = sample_config();
        config.extra_hosts = vec!["host.docker.internal:host-gateway".to_string()];
        let result = build_bollard_config(&config);

        let host_config = result.host_config.as_ref().expect("host_config");
        let extra = host_config.extra_hosts.as_ref().expect("extra_hosts");
        assert_eq!(extra, &["host.docker.internal:host-gateway"]);
    }

    #[test]
    fn build_bollard_config_empty_extra_hosts() {
        let config = sample_config();
        let result = build_bollard_config(&config);

        let host_config = result.host_config.as_ref().expect("host_config");
        assert!(host_config.extra_hosts.is_none());
    }

    #[test]
    fn build_bollard_config_empty_cmd_and_env() {
        let config = ContainerConfig {
            name: "min".to_string(),
            image: "alpine".to_string(),
            network: "net".to_string(),
            ..Default::default()
        };
        let result = build_bollard_config(&config);

        assert!(result.cmd.is_none());
        assert!(result.entrypoint.is_none());
        assert!(result.env.is_none());
    }

    #[test]
    fn build_bollard_config_with_working_dir() {
        let mut config = sample_config();
        config.working_dir = Some("/app".to_string());
        let result = build_bollard_config(&config);
        assert_eq!(result.working_dir.as_deref(), Some("/app"));
    }

    #[test]
    fn build_bollard_config_without_working_dir() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        assert!(result.working_dir.is_none());
    }

    #[test]
    fn build_bollard_config_with_user() {
        let mut config = sample_config();
        config.user = Some("1000:1000".to_string());
        let result = build_bollard_config(&config);
        assert_eq!(result.user.as_deref(), Some("1000:1000"));
    }

    #[test]
    fn build_bollard_config_without_user() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        assert!(result.user.is_none());
    }

    #[test]
    fn build_bollard_config_cpu_conversion() {
        let mut config = sample_config();
        config.cpu_units = Some(256);
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        // 256 * 1_000_000_000 / 1024 = 250_000_000
        assert_eq!(hc.nano_cpus, Some(250_000_000));
    }

    #[test]
    fn build_bollard_config_memory_conversion() {
        let mut config = sample_config();
        config.memory_mib = Some(512);
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        // 512 * 1024 * 1024 = 536_870_912
        assert_eq!(hc.memory, Some(536_870_912));
    }

    #[test]
    fn build_bollard_config_memory_reservation_conversion() {
        let mut config = sample_config();
        config.memory_reservation_mib = Some(256);
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        // 256 * 1024 * 1024 = 268_435_456
        assert_eq!(hc.memory_reservation, Some(268_435_456));
    }

    #[test]
    fn build_bollard_config_no_resource_limits() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        assert!(hc.nano_cpus.is_none());
        assert!(hc.memory.is_none());
        assert!(hc.memory_reservation.is_none());
    }

    #[test]
    fn build_bollard_config_ulimits() {
        let mut config = sample_config();
        config.ulimits = vec![
            UlimitConfig {
                name: "nofile".to_string(),
                soft: 1024,
                hard: 4096,
            },
            UlimitConfig {
                name: "memlock".to_string(),
                soft: -1,
                hard: -1,
            },
        ];
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        let ulimits = hc.ulimits.as_ref().expect("ulimits");
        assert_eq!(ulimits.len(), 2);
        assert_eq!(ulimits[0].name.as_deref(), Some("nofile"));
        assert_eq!(ulimits[0].soft, Some(1024));
        assert_eq!(ulimits[0].hard, Some(4096));
        assert_eq!(ulimits[1].name.as_deref(), Some("memlock"));
        assert_eq!(ulimits[1].soft, Some(-1));
    }

    #[test]
    fn build_bollard_config_empty_ulimits() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        assert!(hc.ulimits.is_none());
    }

    #[test]
    fn build_bollard_config_init_process() {
        let mut config = sample_config();
        config.init = Some(true);
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        assert_eq!(hc.init, Some(true));
    }

    #[test]
    fn build_bollard_config_shm_size() {
        let mut config = sample_config();
        config.shm_size = Some(268_435_456); // 256 MiB in bytes
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        assert_eq!(hc.shm_size, Some(268_435_456));
    }

    #[test]
    fn build_bollard_config_tmpfs() {
        let mut config = sample_config();
        config
            .tmpfs
            .insert("/run".to_string(), "size=67108864,rw,noexec".to_string());
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        let tmpfs = hc.tmpfs.as_ref().expect("tmpfs");
        assert_eq!(
            tmpfs.get("/run").map(String::as_str),
            Some("size=67108864,rw,noexec")
        );
    }

    #[test]
    fn build_bollard_config_empty_tmpfs() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        let hc = result.host_config.as_ref().expect("host_config");
        assert!(hc.tmpfs.is_none());
    }

    #[test]
    fn build_bollard_config_with_cmd_and_entrypoint() {
        let config = sample_config();
        let result = build_bollard_config(&config);

        let cmd = result.cmd.expect("cmd should be Some");
        assert_eq!(cmd, vec!["nginx", "-g", "daemon off;"]);

        let ep = result.entrypoint.expect("entrypoint should be Some");
        assert_eq!(ep, vec!["/entrypoint.sh"]);

        let env = result.env.expect("env should be Some");
        assert_eq!(env, vec!["PORT=8080"]);
    }

    #[test]
    fn build_bollard_config_networking() {
        let config = sample_config();
        let result = build_bollard_config(&config);

        let net_config = result
            .networking_config
            .as_ref()
            .expect("networking_config");
        let endpoint = net_config
            .endpoints_config
            .get("lecs-test")
            .expect("endpoint for lecs-test");
        assert_eq!(
            endpoint.aliases.as_deref(),
            Some(["app".to_string()].as_slice())
        );
    }

    #[test]
    fn container_error_display() {
        let err = ContainerError::RuntimeNotRunning;
        assert_eq!(
            err.to_string(),
            "Container runtime is not running. Please start Docker or Podman and try again."
        );
    }

    #[test]
    fn podman_socket_candidates_includes_rootful() {
        let candidates = podman_socket_candidates();
        assert!(candidates.iter().any(|c| c == "/run/podman/podman.sock"));
    }

    #[test]
    fn podman_socket_candidates_rootful_is_last() {
        let candidates = podman_socket_candidates();
        assert_eq!(
            candidates.last().map(String::as_str),
            Some("/run/podman/podman.sock")
        );
    }

    #[test]
    fn podman_socket_candidates_has_rootless_candidate() {
        // Whether via XDG_RUNTIME_DIR or UID fallback, on a standard Linux
        // system there should be at least one rootless candidate before rootful.
        let candidates = podman_socket_candidates();
        if std::env::var("XDG_RUNTIME_DIR").is_ok() || current_uid().is_some() {
            assert!(
                candidates.len() >= 2,
                "expected rootless + rootful candidates, got: {candidates:?}"
            );
            // The first candidate should be a rootless socket path
            let first = &candidates[0];
            assert!(
                first.contains("/podman/podman.sock") && first != "/run/podman/podman.sock",
                "first candidate should be a rootless socket path, got: {first}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn current_uid_returns_valid_value() {
        // On Linux with /proc, current_uid should return Some with a valid UID
        let uid = current_uid();
        if std::path::Path::new("/proc/self").exists() {
            assert!(uid.is_some(), "current_uid() should return Some on Linux");
        }
    }

    #[test]
    fn parse_host_url_unix() {
        let (scheme, path) = parse_host_url("unix:///run/podman/podman.sock");
        assert_eq!(scheme, HostScheme::Unix);
        assert_eq!(path, "/run/podman/podman.sock");
    }

    #[test]
    fn parse_host_url_tcp() {
        let (scheme, addr) = parse_host_url("tcp://localhost:2375");
        assert_eq!(scheme, HostScheme::Tcp);
        assert_eq!(addr, "localhost:2375");
    }

    #[test]
    fn parse_host_url_bare_path() {
        let (scheme, path) = parse_host_url("/run/podman/podman.sock");
        assert_eq!(scheme, HostScheme::Unix);
        assert_eq!(path, "/run/podman/podman.sock");
    }

    #[test]
    fn build_bollard_config_with_healthcheck() {
        let mut config = sample_config();
        config.health_check = Some(HealthCheckConfig {
            test: vec!["CMD-SHELL".into(), "curl -f http://localhost/".into()],
            interval_ns: 10_000_000_000,
            timeout_ns: 5_000_000_000,
            retries: 3,
            start_period_ns: 15_000_000_000,
        });

        let result = build_bollard_config(&config);
        let hc = result.healthcheck.as_ref().expect("healthcheck");
        assert_eq!(
            hc.test.as_deref(),
            Some(
                [
                    "CMD-SHELL".to_string(),
                    "curl -f http://localhost/".to_string()
                ]
                .as_slice()
            )
        );
        assert_eq!(hc.interval, Some(10_000_000_000));
        assert_eq!(hc.timeout, Some(5_000_000_000));
        assert_eq!(hc.retries, Some(3));
        assert_eq!(hc.start_period, Some(15_000_000_000));
    }

    #[test]
    fn build_bollard_config_without_healthcheck() {
        let config = sample_config();
        let result = build_bollard_config(&config);
        assert!(result.healthcheck.is_none());
    }

    #[test]
    fn build_bollard_config_with_binds() {
        let mut config = sample_config();
        config.binds = vec![
            "/host/data:/container/data".to_string(),
            "/host/cache:/container/cache:ro".to_string(),
        ];
        let result = build_bollard_config(&config);

        let host_config = result.host_config.as_ref().expect("host_config");
        let binds = host_config.binds.as_ref().expect("binds");
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0], "/host/data:/container/data");
        assert_eq!(binds[1], "/host/cache:/container/cache:ro");
    }

    #[test]
    fn build_bollard_config_empty_binds() {
        let config = sample_config();
        let result = build_bollard_config(&config);

        let host_config = result.host_config.as_ref().expect("host_config");
        assert!(host_config.binds.is_none());
    }

    #[test]
    fn parse_image_reference_simple_name() {
        assert_eq!(parse_image_reference("nginx"), ("nginx", "latest"));
    }

    #[test]
    fn parse_image_reference_with_tag() {
        assert_eq!(parse_image_reference("nginx:1.25"), ("nginx", "1.25"));
    }

    #[test]
    fn parse_image_reference_registry_with_port() {
        assert_eq!(
            parse_image_reference("registry.example.com:5000/app"),
            ("registry.example.com:5000/app", "latest")
        );
    }

    #[test]
    fn parse_image_reference_registry_with_port_and_tag() {
        assert_eq!(
            parse_image_reference("registry.example.com:5000/app:v1"),
            ("registry.example.com:5000/app", "v1")
        );
    }

    #[test]
    fn parse_image_reference_digest() {
        let img = "nginx@sha256:abc123";
        assert_eq!(parse_image_reference(img), (img, ""));
    }

    #[test]
    fn parse_image_reference_namespaced() {
        assert_eq!(
            parse_image_reference("library/nginx:alpine"),
            ("library/nginx", "alpine")
        );
    }
}
