use crate::shims::{self, DEFAULT_NAMESPACE, ShimProcess};
use bollard::Docker;
use bollard::models::{ImageSummary, VolumeListResponse};
use bollard::query_parameters::{
    ListContainersOptions, ListImagesOptions, ListNetworksOptions, ListVolumesOptions,
    RemoveContainerOptions, RemoveImageOptions,
};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tabled::Tabled;
use thiserror::Error;
use tokio::time::{Duration, sleep};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub(crate) struct ReapContainersConfig<'a> {
    /// Return results without actually removing containers or networks.
    pub(crate) dry_run: bool,
    /// Only containers older than this duration will be eligible for reaping.
    pub(crate) min_age: Option<Duration>,
    /// Only containers younger than this duration will be eligible for reaping.
    pub(crate) max_age: Option<Duration>,
    /// Additional Docker Engine-supported [container filters](https://docs.docker.com/engine/reference/commandline/ps/#filter).
    pub(crate) filters: &'a Vec<Filter>,
    /// Also attempt to remove the networks associated with reaped containers.
    pub(crate) reap_networks: bool,
}

#[derive(Debug)]
pub(crate) struct ReapShimsConfig {
    /// Report orphans without signalling them.
    pub(crate) dry_run: bool,
    /// Only shims that have been running at least this long are eligible.
    pub(crate) min_age: Duration,
    /// containerd namespace to restrict the sweep to. Docker uses "moby".
    ///
    /// Orphanhood is decided against the connected daemon's container list, which is only
    /// meaningful for the namespace that daemon owns. Selecting another namespace makes
    /// that check vacuous -- the daemon will never report those ids -- so a non-default
    /// value is warned about. It also excludes other *namespaces*, not other clients of
    /// the same one: a second daemon, a DinD inner daemon or `ctr -n moby` are all
    /// invisible to the queried daemon and visible in /proc. Out of scope by assumption.
    pub(crate) namespace: String,
    /// How long to wait before re-confirming a candidate. Closes the window where a
    /// container has just been removed from the daemon but its shim has not yet exited.
    pub(crate) settle: Duration,
    /// How long a shim gets to exit after SIGTERM before it is sent SIGKILL.
    pub(crate) grace: Duration,
    /// Root of the proc filesystem. Overridable for testing.
    pub(crate) proc_root: PathBuf,
    /// Root of containerd's v2 runtime task state, used only to enrich reporting.
    pub(crate) runtime_root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ReapNetworksConfig<'a> {
    /// Return results without actually removing networks.
    pub(crate) dry_run: bool,
    /// Only networks older than this duration will be eligible for reaping.
    pub(crate) min_age: Option<Duration>,
    /// Only networks younger than this duration will be eligible for reaping.
    pub(crate) max_age: Option<Duration>,
    /// Additional Docker Engine-supported [network filters](https://docs.docker.com/engine/reference/commandline/network_ls/#filter).
    pub(crate) filters: &'a Vec<Filter>,
}

#[derive(Debug)]
pub(crate) struct ReapVolumesConfig<'a> {
    /// Return results without actually removing volumes.
    pub(crate) dry_run: bool,
    /// Only volumes older than this duration will be eligible for reaping.
    pub(crate) min_age: Option<Duration>,
    /// Only volumes younger than this duration will be eligible for reaping.
    pub(crate) max_age: Option<Duration>,
    /// Additional Docker Engine-supported [volume filters](https://docs.docker.com/engine/reference/commandline/volume_ls/#filter).
    pub(crate) filters: &'a Vec<Filter>,
}

#[derive(Debug)]
pub(crate) struct ReapImagesConfig<'a> {
    /// Return results without actually removing images.
    pub(crate) dry_run: bool,
    /// Reap only when the measured filesystem is at least this full (percent).
    pub(crate) threshold: u8,
    /// Remove images (largest unique size first) until usage falls below this (percent).
    pub(crate) target: u8,
    /// Filesystem to measure; defaults to the daemon's root directory, which
    /// is only correct when the daemon is local.
    pub(crate) disk_path: Option<PathBuf>,
    /// Additional Docker Engine-supported [image filters](https://docs.docker.com/engine/reference/commandline/image_ls/#filter).
    pub(crate) filters: &'a Vec<Filter>,
}

#[derive(Debug)]
pub(crate) enum RemovalStatus {
    /// Used in dry-run mode to indicate that a resource is eligible for removal.
    Eligible,
    /// Resource was successfully removed.
    Success,
    /// Removal was already in progress.
    InProgress,
    /// Resource was in use at removal time and skipped (e.g. a container was
    /// created from an image between listing and removal).
    InUse,
    /// Resource was not needed to reach the disk usage target.
    NotNeeded,
    /// An error occurred when attempting to remove this resource.
    Error(RemovalError),
}

impl fmt::Display for RemovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eligible => write!(f, "Eligible for removal"),
            Self::Success => write!(f, "Removed"),
            &Self::InProgress => write!(f, "Removal in progress"),
            Self::InUse => write!(f, "Skipped: in use"),
            Self::NotNeeded => write!(f, "Skipped: target reached"),
            Self::Error(e) => write!(f, "Error: {}", e),
        }
    }
}

#[derive(Clone, Debug)]
/// A Docker Engine filter (see <https://docs.docker.com/engine/reference/commandline/ps/#filter>)
pub(crate) struct Filter {
    name: String,
    value: String,
}

trait BollardConversionExt {
    /// Converts the iterator into the format expected by `bollard` filter arguments.
    ///
    /// The values of all items sharing the same key are combined into a single Vec.
    fn to_bollard_filters(&self) -> HashMap<String, Vec<String>>
    where
        Self: IntoIterator;
}

impl BollardConversionExt for Vec<Filter> {
    fn to_bollard_filters(&self) -> HashMap<String, Vec<String>> {
        self.iter().fold(HashMap::new(), |mut acc, f| {
            acc.entry(f.name.clone()).or_default().push(f.value.clone());
            acc
        })
    }
}

impl Filter {
    pub(crate) fn new(name: &str, value: &str) -> Self {
        Self {
            name: String::from(name),
            value: String::from(value),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResourceType {
    Container,
    /// An orphaned containerd shim. Not a Docker resource: it is found by reading the
    /// local process table and removed with a signal, not through the Engine API.
    Shim,
    Network,
    Volume,
    Image,
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Container => {
                write!(f, "Container")
            }
            Self::Shim => {
                write!(f, "Shim")
            }
            Self::Network => {
                write!(f, "Network")
            }
            Self::Volume => {
                write!(f, "Volume")
            }
            Self::Image => {
                write!(f, "Image")
            }
        }
    }
}

#[derive(Debug, Tabled)]
#[tabled(rename_all = "PascalCase")]
pub(crate) struct Resource {
    #[tabled(rename = "Resource Type")]
    pub(crate) resource_type: ResourceType,
    #[tabled(skip)]
    pub(crate) id: String,
    pub(crate) name: String,
    /// Extra per-resource context (e.g. reclaimable size for images).
    pub(crate) details: String,
    pub(crate) status: RemovalStatus,
}

impl PartialEq for Resource {
    fn eq(&self, other: &Self) -> bool {
        self.resource_type == other.resource_type && self.id == other.id
    }
}

impl Resource {
    /// Attempts to remove this resource.
    /// After completion, the resource's `status` will be either `RemovalStatus::Success` or
    /// `RemovalStatus::Error`.
    async fn remove(&mut self, docker: &Docker) {
        debug!("Removing {} {}", self.resource_type, self.name);
        use bollard::errors::Error::DockerResponseServerError;
        match self.resource_type {
            ResourceType::Container => {
                let options = RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                };
                match docker.remove_container(&self.id, Some(options)).await {
                    Ok(_) => {
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 409, ..
                    }) => {
                        self.status = RemovalStatus::InProgress;
                    }
                    Err(e) => self.status = RemovalStatus::Error(RemovalError::Docker(e)),
                };
            }
            ResourceType::Network => {
                match docker.remove_network(&self.id).await {
                    Ok(_) => {
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 409, ..
                    }) => {
                        self.status = RemovalStatus::InProgress;
                    }
                    Err(e) => self.status = RemovalStatus::Error(RemovalError::Docker(e)),
                };
            }
            ResourceType::Volume => {
                match docker
                    .remove_volume(
                        &self.id,
                        None::<bollard::query_parameters::RemoveVolumeOptions>,
                    )
                    .await
                {
                    Ok(_) => {
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 409, ..
                    }) => {
                        self.status = RemovalStatus::InProgress;
                    }
                    Err(e) => self.status = RemovalStatus::Error(RemovalError::Docker(e)),
                }
            }
            ResourceType::Shim => {
                // Unreachable: shims are signalled by reap_shims, which never routes
                // them through the Engine API. Reported rather than silently skipped.
                self.status = RemovalStatus::Error(RemovalError::Shim(String::from(
                    "shims cannot be removed through the Docker API",
                )));
            }
            ResourceType::Image => {
                // No force: an image that gained a container reference since
                // listing produces a 409 and is skipped rather than orphaning
                // a running container's image.
                let options = RemoveImageOptions {
                    force: false,
                    noprune: false,
                    ..Default::default()
                };
                match docker.remove_image(&self.id, Some(options), None).await {
                    Ok(_) => {
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 404, ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(DockerResponseServerError {
                        status_code: 409, ..
                    }) => {
                        self.status = RemovalStatus::InUse;
                    }
                    Err(e) => self.status = RemovalStatus::Error(RemovalError::Docker(e)),
                }
            }
        }
    }
}

/// Error encountered while removing a resource.
#[derive(Error, Debug)]
pub(crate) enum RemovalError {
    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),
    #[error("{0}")]
    Shim(String),
}

/// Unrecoverable error encountered during a reap iteration.
#[derive(Error, Debug)]
pub(crate) enum ReapError {
    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),
    #[error(transparent)]
    InvalidSystemTime(#[from] std::time::SystemTimeError),
    #[error(transparent)]
    TaskFailure(#[from] tokio::task::JoinError),
    #[error("min_age must be less than max_age")]
    InvalidAgeBound,
    #[error("target must be less than threshold")]
    InvalidDiskBounds,
    #[error("failed to measure disk usage: {0}")]
    DiskMeasurement(#[from] std::io::Error),
    #[error("failed to read the process table at {path}: {source}")]
    ProcRead {
        path: String,
        source: std::io::Error,
    },
    #[error("docker daemon did not report its root directory; pass --disk-path explicitly")]
    UnknownDockerRoot,
}

pub(crate) async fn reap_containers(
    docker: &Docker,
    config: &ReapContainersConfig<'_>,
) -> Result<Vec<Resource>, ReapError> {
    if config.min_age.unwrap_or(Duration::ZERO) >= config.max_age.unwrap_or(Duration::MAX) {
        return Err(ReapError::InvalidAgeBound);
    }

    // Collect eligible containers. Since there's no way to ask the Docker API for containers
    // matching a certain age range directly, we first obtain the full list based only on the
    // provided filter values (if any).
    let mut eligible_containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(config.filters.to_bollard_filters()),
            ..Default::default()
        }))
        .await?;

    // Reduce the eligible containers to only those within the specified age range (if applicable).
    if config.max_age.is_some() || config.min_age.is_some() {
        let now: Duration = SystemTime::now().duration_since(UNIX_EPOCH)?;
        eligible_containers.retain(|container| {
            let id = container.id.as_deref().unwrap_or("unknown ID");
            // The creation time for containers is returned as a signed UNIX timestamp, but we need
            // to convert it to an unsigned value to use `Duration::from_secs()`. If, for some
            // reason, the returned creation time is missing or negative, skip the container.
            let Some(creation_secs) = container.created else {
                warn!("Skipped container {}: missing creation timestamp", id);
                return false;
            };
            let creation_secs: u64 = match creation_secs.try_into() {
                Ok(secs) => secs,
                Err(_) => {
                    warn!("Skipped container {}: negative creation timestamp", id);
                    return false;
                }
            };
            let Some(age) = now.checked_sub(Duration::from_secs(creation_secs)) else {
                warn!(
                    "Skipped container {}: creation timestamp after system time",
                    id
                );
                return false;
            };
            let within_age_range = age > config.min_age.unwrap_or(Duration::ZERO)
                && age < config.max_age.unwrap_or(Duration::MAX);
            if !within_age_range {
                debug!("Skipped container {}: age outside of specified range", id);
            }
            within_age_range
        });
    }

    let mut eligible_network_names = HashSet::new();
    let mut eligible_resources: Vec<Resource> = Vec::new();
    for container in eligible_containers {
        let Some(id) = container.id else {
            warn!("Skipped container (unknown ID): missing ID value");
            continue;
        };
        eligible_resources.push(Resource {
            resource_type: ResourceType::Container,
            id: id.clone(),
            name: container
                .names
                .unwrap_or_default()
                .first()
                .unwrap_or(&id)
                .clone(),
            details: String::new(),
            status: RemovalStatus::Eligible,
        });
        if config.reap_networks
            && let Some(network_settings) = container.network_settings
            && let Some(networks) = network_settings.networks
        {
            // Docker has network IDs, but also requires each network to have a unique
            // name. We just use the name as an ID since it's easier to retrieve.
            eligible_network_names.extend(networks.keys().cloned().inspect(|name| {
                debug!("Added network {} from container {} ", name, id);
            }))
        }
    }
    for network_name in eligible_network_names {
        eligible_resources.push(Resource {
            resource_type: ResourceType::Network,
            id: network_name.clone(),
            name: network_name.clone(),
            details: String::new(),
            status: RemovalStatus::Eligible,
        })
    }
    if config.dry_run {
        return Ok(eligible_resources);
    }
    // Remove containers before networks, as otherwise there will be active endpoints
    let mut container_futures = Vec::new();
    let mut network_futures = Vec::new();
    for mut resource in eligible_resources {
        match resource.resource_type {
            ResourceType::Container => container_futures.push(async move {
                resource.remove(docker).await;
                resource
            }),
            ResourceType::Network => network_futures.push(async move {
                resource.remove(docker).await;
                resource
            }),
            _ => {}
        }
    }
    let mut removed_resources = futures::future::join_all(container_futures).await;
    removed_resources.extend(futures::future::join_all(network_futures).await);
    Ok(removed_resources)
}

pub(crate) async fn reap_networks(
    docker: &Docker,
    config: &ReapNetworksConfig<'_>,
) -> Result<Vec<Resource>, ReapError> {
    if config.min_age.unwrap_or(Duration::ZERO) >= config.max_age.unwrap_or(Duration::MAX) {
        return Err(ReapError::InvalidAgeBound);
    }

    let mut eligible_networks = docker
        .list_networks(Some(ListNetworksOptions {
            filters: Some(config.filters.to_bollard_filters()),
        }))
        .await?;

    if config.max_age.is_some() || config.min_age.is_some() {
        let now = chrono::Utc::now();
        eligible_networks.retain(|network| {
            let Some(ref name) = network.name else {
                warn!("Skipped network (unknown name): missing name value");
                return false;
            };
            let Some(ref creation_timestamp) = network.created else {
                warn!("Skipped network {}: missing creation timestamp", name);
                return false;
            };
            let Ok(creation_time) = chrono::DateTime::parse_from_rfc3339(creation_timestamp) else {
                warn!(
                    "Skipped network {}: failed to parse creation timestamp as RFC3339",
                    name
                );
                return false;
            };
            let Ok(age) = now.signed_duration_since(creation_time).to_std() else {
                warn!(
                    "Skipped network {}: creation timestamp after system time",
                    name
                );
                return false;
            };
            let within_age_range = age > config.min_age.unwrap_or(Duration::ZERO)
                && age < config.max_age.unwrap_or(Duration::MAX);
            if !within_age_range {
                debug!("Skipped network {}: age outside of specified range", name);
            }
            within_age_range
        });
    }
    let eligible_networks: Vec<Resource> = eligible_networks
        .into_iter()
        .filter_map(|network| {
            let Some(name) = network.name else {
                warn!("Skipped network (unknown name): missing name value");
                return None;
            };
            Some(Resource {
                resource_type: ResourceType::Network,
                id: name.clone(),
                name,
                details: String::new(),
                status: RemovalStatus::Eligible,
            })
        })
        .collect();
    if config.dry_run {
        return Ok(eligible_networks);
    }
    let network_futures = eligible_networks.into_iter().map(|mut network| async move {
        network.remove(docker).await;
        network
    });
    let removed_networks = futures::future::join_all(network_futures).await;
    Ok(removed_networks)
}

pub(crate) async fn reap_volumes(
    docker: &Docker,
    config: &ReapVolumesConfig<'_>,
) -> Result<Vec<Resource>, ReapError> {
    if config.min_age.unwrap_or(Duration::ZERO) >= config.max_age.unwrap_or(Duration::MAX) {
        return Err(ReapError::InvalidAgeBound);
    }

    let VolumeListResponse {
        volumes: eligible_volumes,
        warnings,
    } = docker
        .list_volumes(Some(ListVolumesOptions {
            filters: Some(config.filters.to_bollard_filters()),
        }))
        .await?;
    if let Some(warnings) = warnings {
        for warning in warnings {
            warn!("Encountered warning when listing volumes: {}", warning);
        }
    }
    let Some(mut eligible_volumes) = eligible_volumes else {
        debug!("No volumes returned");
        return Ok(Vec::new());
    };

    if config.max_age.is_some() || config.min_age.is_some() {
        let now = chrono::Utc::now();
        eligible_volumes.retain(|volume| {
            let Some(ref creation_timestamp) = volume.created_at else {
                warn!("Skipped volume {}: missing creation timestamp", volume.name);
                return false;
            };
            let Ok(creation_time) = chrono::DateTime::parse_from_rfc3339(creation_timestamp) else {
                warn!(
                    "Skipped volume {}: failed to parse creation timestamp as RFC3339",
                    volume.name
                );
                return false;
            };
            let Ok(age) = now.signed_duration_since(creation_time).to_std() else {
                warn!(
                    "Skipped volume {}: creation timestamp after system time",
                    volume.name
                );
                return false;
            };
            let within_age_range = age > config.min_age.unwrap_or(Duration::ZERO)
                && age < config.max_age.unwrap_or(Duration::MAX);
            if !within_age_range {
                debug!(
                    "Skipped volume {}: age outside of specified range",
                    volume.name
                );
            }
            within_age_range
        })
    }
    let eligible_volumes: Vec<Resource> = eligible_volumes
        .into_iter()
        .map(|volume| Resource {
            resource_type: ResourceType::Volume,
            id: volume.name.clone(),
            name: volume.name,
            details: String::new(),
            status: RemovalStatus::Eligible,
        })
        .collect();
    if config.dry_run {
        return Ok(eligible_volumes);
    }
    let volume_futures = eligible_volumes.into_iter().map(|mut volume| async move {
        volume.remove(docker).await;
        volume
    });
    let removed_volumes = futures::future::join_all(volume_futures).await;
    Ok(removed_volumes)
}

/// An image eligible for eviction: referenced by no container, sized by the
/// bytes uniquely attributable to it (i.e. not shared with other images).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ImageCandidate {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) unique_size: u64,
}

/// Selects and orders eviction candidates: images not referenced by any
/// container, largest unique size first. Unique size (total minus layers
/// shared with other images) is what a removal actually reclaims; an unknown
/// shared size (-1) is treated as fully unique.
pub(crate) fn plan_image_evictions(
    images: &[ImageSummary],
    in_use: &HashSet<String>,
) -> Vec<ImageCandidate> {
    let mut candidates: Vec<ImageCandidate> = images
        .iter()
        .filter(|image| !in_use.contains(&image.id))
        .map(|image| ImageCandidate {
            id: image.id.clone(),
            name: image
                .repo_tags
                .first()
                .cloned()
                .unwrap_or_else(|| image.id.clone()),
            unique_size: u64::try_from(image.size - image.shared_size.max(0)).unwrap_or(0),
        })
        .collect();
    candidates.sort_by_key(|c| std::cmp::Reverse(c.unique_size));
    candidates
}

/// Used and total capacity (in bytes) of the filesystem containing `path`,
/// computed like df(1): capacity is used space plus space available to
/// unprivileged processes, so the root reserve never counts as free.
fn disk_usage(path: &Path) -> Result<(u64, u64), ReapError> {
    let vfs = rustix::fs::statvfs(path).map_err(std::io::Error::from)?;
    let used = vfs.f_blocks.saturating_sub(vfs.f_bfree) * vfs.f_frsize;
    let capacity = used + vfs.f_bavail * vfs.f_frsize;
    if capacity == 0 {
        return Err(ReapError::DiskMeasurement(std::io::Error::other(
            "filesystem reports zero capacity",
        )));
    }
    Ok((used, capacity))
}

fn used_percent(used: u64, capacity: u64) -> f64 {
    100.0 * used as f64 / capacity as f64
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", size, UNITS[unit])
}

pub(crate) async fn reap_images(
    docker: &Docker,
    config: &ReapImagesConfig<'_>,
) -> Result<Vec<Resource>, ReapError> {
    if config.target >= config.threshold {
        return Err(ReapError::InvalidDiskBounds);
    }

    // Resolve the filesystem to measure: an explicit path, or the daemon's
    // image store. The latter is only meaningful when the daemon is local —
    // main.rs refuses the remote+default combination up front.
    let disk_path = match &config.disk_path {
        Some(path) => path.clone(),
        None => PathBuf::from(
            docker
                .info()
                .await?
                .docker_root_dir
                .ok_or(ReapError::UnknownDockerRoot)?,
        ),
    };

    let (used, capacity) = disk_usage(&disk_path)?;
    let usage = used_percent(used, capacity);
    if usage < config.threshold as f64 {
        info!(
            "Disk usage of {} is {:.1}%, below the {}% threshold; nothing to do",
            disk_path.display(),
            usage,
            config.threshold
        );
        return Ok(Vec::new());
    }
    warn!(
        "Disk usage of {} is {:.1}%, at or above the {}% threshold; evicting images to reach {}%",
        disk_path.display(),
        usage,
        config.threshold,
        config.target
    );

    // An image referenced by any container — running or stopped — must not be
    // removed, so collect the referenced image IDs first.
    let in_use: HashSet<String> = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?
        .into_iter()
        .filter_map(|container| container.image_id)
        .collect();

    // shared_size asks the engine to compute per-image shared layer bytes,
    // which list_images otherwise reports as -1.
    let images = docker
        .list_images(Some(ListImagesOptions {
            shared_size: true,
            filters: Some(config.filters.to_bollard_filters()),
            ..Default::default()
        }))
        .await?;

    let candidates = plan_image_evictions(&images, &in_use);
    let target_bytes = (config.target as u64) * (capacity / 100);

    if config.dry_run {
        // Estimate using unique sizes; actual reclaim is re-measured from the
        // filesystem in a real run.
        let mut projected_used = used;
        return Ok(candidates
            .into_iter()
            .map(|candidate| {
                let status = if projected_used > target_bytes {
                    projected_used = projected_used.saturating_sub(candidate.unique_size);
                    RemovalStatus::Eligible
                } else {
                    RemovalStatus::NotNeeded
                };
                Resource {
                    resource_type: ResourceType::Image,
                    id: candidate.id,
                    name: candidate.name,
                    details: format_size(candidate.unique_size),
                    status,
                }
            })
            .collect());
    }

    // Remove sequentially, largest first, re-measuring the filesystem after
    // each successful removal so we stop as soon as the target is reached
    // rather than trusting the shared-size estimates.
    let mut results = Vec::new();
    for candidate in candidates {
        let (used, capacity) = disk_usage(&disk_path)?;
        if used <= target_bytes {
            info!(
                "Disk usage now {:.1}%, at or below the {}% target",
                used_percent(used, capacity),
                config.target
            );
            break;
        }
        let mut resource = Resource {
            resource_type: ResourceType::Image,
            id: candidate.id,
            name: candidate.name,
            details: format_size(candidate.unique_size),
            status: RemovalStatus::Eligible,
        };
        resource.remove(docker).await;
        results.push(resource);
    }

    let (used, capacity) = disk_usage(&disk_path)?;
    if used > target_bytes {
        warn!(
            "Out of removable images with disk usage still at {:.1}% (target {}%)",
            used_percent(used, capacity),
            config.target
        );
    }
    Ok(results)
}

/// Reaps `containerd-shim-runc-v2` processes whose container no longer exists.
///
/// These cannot be reached through the Engine API — the container is already gone from
/// the daemon, so `remove_container` returns 404 — and each one holds roughly 5 MiB
/// until the host reboots. Discovery therefore reads the local process table, and this
/// only makes sense against a local daemon.
///
/// A shim is only signalled once it has passed three checks:
///
/// 1. Its container id is absent from `list_containers(all)`, so neither a running nor a
///    stopped container claims it.
/// 2. It has been alive for at least `min_age`, sparing anything mid-creation.
/// 3. Both of the above still hold after `settle`, and its argv still names the same
///    container. The re-read closes the window where a container has just been removed
///    but its shim has not yet exited, and guards against the pid being reused.
pub(crate) async fn reap_shims(
    docker: &Docker,
    config: &ReapShimsConfig,
) -> Result<Vec<Resource>, ReapError> {
    let proc_err = |source| ReapError::ProcRead {
        path: config.proc_root.display().to_string(),
        source,
    };
    // Every selection gate resolves under proc_root, but the signal goes to the real
    // process table, so a directory of fixture data could otherwise nominate arbitrary
    // pids for SIGTERM/SIGKILL. Confirm the tree really is this process's own procfs
    // before signalling. Dry runs never signal, so they keep working against fixtures.
    if !config.dry_run {
        let names_us = std::fs::read_link(config.proc_root.join("self"))
            .ok()
            .and_then(|link| link.to_str().and_then(|pid| pid.parse::<u32>().ok()))
            == Some(std::process::id());
        if !names_us {
            return Err(proc_err(std::io::Error::other(
                "not this process's own procfs; refusing to signal pids read from it",
            )));
        }
    }

    let uptime = shims::uptime(&config.proc_root).map_err(proc_err)?;
    let clock_ticks = rustix::param::clock_ticks_per_second();
    let all_shims = shims::list_shims(&config.proc_root, uptime, clock_ticks).map_err(proc_err)?;
    debug!("Found {} shim process(es)", all_shims.len());

    if config.namespace != DEFAULT_NAMESPACE {
        warn!(
            "Sweeping containerd namespace {:?} rather than {:?}: the check that a shim's \
             container is absent from the daemon is only meaningful for the namespace \
             that daemon owns, so it does not protect live containers here",
            config.namespace, DEFAULT_NAMESPACE
        );
    }

    let live_containers = list_container_ids(docker).await?;
    let candidates: Vec<ShimProcess> = all_shims
        .into_iter()
        .filter(|shim| {
            if shim.namespace != config.namespace {
                debug!(
                    "Skipped shim {}: namespace {} is out of scope",
                    shim.pid, shim.namespace
                );
                return false;
            }
            if live_containers.contains(&shim.container_id) {
                return false;
            }
            if shim.age < config.min_age {
                debug!(
                    "Skipped shim {}: only {:?} old, below the minimum age",
                    shim.pid, shim.age
                );
                return false;
            }
            // Everything above comes from /proc/<pid>/cmdline, which is argv the process
            // chose for itself. On a host running untrusted containers any of them can
            // present itself as an orphaned shim and, by ignoring SIGTERM, make this sweep
            // spend its grace period on a decoy. Namespace membership cannot be forged
            // from inside a container, and a genuine orphan is a host process however
            // containerd left its bookkeeping -- so this cannot spare a real orphan.
            if !shims::shares_pid_namespace(&config.proc_root, shim.pid) {
                debug!(
                    "Skipped shim {}: not in this process's PID namespace, so not a host shim",
                    shim.pid
                );
                return false;
            }
            true
        })
        .collect();

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let describe = |shim: &ShimProcess| {
        let task_dir =
            shims::task_state_dir(&config.runtime_root, &shim.namespace, &shim.container_id);
        format!(
            "pid {}, age {}s, task state {}",
            shim.pid,
            shim.age.as_secs(),
            if task_dir.exists() {
                "present"
            } else {
                "absent"
            }
        )
    };

    if config.dry_run {
        return Ok(candidates
            .iter()
            .map(|shim| Resource {
                resource_type: ResourceType::Shim,
                id: shim.container_id.clone(),
                name: format!("shim for {}", short_id(&shim.container_id)),
                details: describe(shim),
                status: RemovalStatus::Eligible,
            })
            .collect());
    }

    info!(
        "Found {} candidate orphaned shim(s); re-confirming after {:?}",
        candidates.len(),
        config.settle
    );
    sleep(config.settle).await;
    let live_containers = list_container_ids(docker).await?;

    // Re-confirm, then terminate as one batch. Doing it per shim would cost
    // candidates x grace, because a shim wedged in a cancelled runtime call does not act
    // on SIGTERM: clearing a large backlog would hold this oneshot unit for minutes and
    // starve the container and image sweeps that share it.
    let mut confirmed = Vec::new();
    let mut spared = 0usize;
    for shim in candidates {
        // Anything that changed during the settle window means this was a container
        // being torn down normally, or a reused pid. Either way, leave it alone.
        if live_containers.contains(&shim.container_id) {
            debug!(
                "Skipped shim {}: its container reappeared during the settle window",
                shim.pid
            );
            spared += 1;
            continue;
        }
        if !shims::still_same_shim(&config.proc_root, &shim) {
            debug!(
                "Skipped shim {}: no longer the same process for {}",
                shim.pid, shim.container_id
            );
            spared += 1;
            continue;
        }
        confirmed.push(shim);
    }
    if spared > 0 {
        info!(
            "Spared {} candidate(s) that changed during the settle window",
            spared
        );
    }
    let statuses = terminate_shims(&confirmed, &config.proc_root, config.grace).await;
    Ok(confirmed
        .into_iter()
        .zip(statuses)
        .map(|(shim, status)| Resource {
            resource_type: ResourceType::Shim,
            id: shim.container_id.clone(),
            name: format!("shim for {}", short_id(&shim.container_id)),
            details: describe(&shim),
            status,
        })
        .collect())
}

/// Signals a batch of shims: SIGTERM to all, one shared grace window, then SIGKILL to
/// whatever is still running.
///
/// Batched rather than per shim because a shim wedged in a cancelled runtime call does not
/// act on SIGTERM, so the grace period is the expected cost of every candidate rather than
/// a rare tail. Sequentially that would be `candidates x grace` -- minutes for a large
/// backlog, during which this oneshot unit blocks the container and image sweeps it shares.
///
/// Returns one status per input shim, in order.
async fn terminate_shims(
    shims: &[ShimProcess],
    proc_root: &Path,
    grace: Duration,
) -> Vec<RemovalStatus> {
    let poll = Duration::from_millis(100);
    // How long to wait for a SIGKILLed process to actually leave the process table. The
    // signal is only queued when kill() returns; the target still has to be scheduled and
    // reaped. Without this every successful kill would be reported as still in progress.
    let reap_window = Duration::from_millis(500);

    let mut statuses: Vec<Option<RemovalStatus>> = (0..shims.len()).map(|_| None).collect();

    for (i, shim) in shims.iter().enumerate() {
        match signal_shim(shim, rustix::process::Signal::TERM) {
            Ok(()) => {}
            // Already gone between the re-check and here.
            Err(SignalError::Gone) => statuses[i] = Some(RemovalStatus::Success),
            Err(SignalError::Failed(e)) => {
                statuses[i] = Some(RemovalStatus::Error(RemovalError::Shim(format!(
                    "SIGTERM failed: {e}"
                ))))
            }
        }
    }

    let outstanding = |statuses: &[Option<RemovalStatus>]| {
        shims
            .iter()
            .enumerate()
            .any(|(i, s)| statuses[i].is_none() && shims::is_alive(proc_root, s.pid))
    };

    // Cap the final step: sleeping a whole poll interval each time would round any
    // grace shorter than the interval up to it, so --grace 1ms would wait 100ms.
    let mut waited = Duration::ZERO;
    while waited < grace && outstanding(&statuses) {
        let step = poll.min(grace.saturating_sub(waited));
        sleep(step).await;
        waited += step;
    }

    for (i, shim) in shims.iter().enumerate() {
        if statuses[i].is_some() {
            continue;
        }
        if !shims::is_alive(proc_root, shim.pid) {
            statuses[i] = Some(RemovalStatus::Success);
            continue;
        }
        // The grace window is long enough for a shim that honoured SIGTERM to exit and
        // have its pid recycled, and is_alive only tests the number. Re-confirm identity
        // the way reap_shims does before the SIGTERM -- argv plus PID namespace, so a
        // container cannot steer the escalation with argv it chose for itself.
        if !shims::still_same_shim(proc_root, shim)
            || !shims::shares_pid_namespace(proc_root, shim.pid)
        {
            warn!(
                "Shim {} pid was reused during the grace window; not escalating",
                shim.pid
            );
            statuses[i] = Some(RemovalStatus::InUse);
            continue;
        }
        warn!(
            "Shim {} did not exit within {:?} of SIGTERM; sending SIGKILL",
            shim.pid, grace
        );
        if let Err(SignalError::Failed(e)) = signal_shim(shim, rustix::process::Signal::KILL) {
            statuses[i] = Some(RemovalStatus::Error(RemovalError::Shim(format!(
                "SIGKILL failed: {e}"
            ))));
        }
    }

    let mut waited = Duration::ZERO;
    while waited < reap_window && outstanding(&statuses) {
        let step = poll.min(reap_window.saturating_sub(waited));
        sleep(step).await;
        waited += step;
    }

    shims
        .iter()
        .enumerate()
        .map(|(i, shim)| {
            statuses[i].take().unwrap_or_else(|| {
                if shims::is_alive(proc_root, shim.pid) {
                    // Uninterruptible sleep, most likely. Distinct from the Docker-409
                    // sense InProgress carries elsewhere in this crate, so report it as an
                    // error rather than implying someone else is cleaning it up.
                    RemovalStatus::Error(RemovalError::Shim(String::from(
                        "still running after SIGKILL",
                    )))
                } else {
                    RemovalStatus::Success
                }
            })
        })
        .collect()
}

enum SignalError {
    /// The process no longer exists, which for our purposes is a success.
    Gone,
    Failed(rustix::io::Errno),
}

fn signal_shim(shim: &ShimProcess, sig: rustix::process::Signal) -> Result<(), SignalError> {
    // list_shims already rejects non-positive pids; re-check here so the invariant is
    // local rather than resting on rustix's Pid::from_raw, whose non-negative guard is a
    // debug_assert and is compiled out of the release profile. kill() with a negative pid
    // signals a process group, and -1 means every process the caller may signal.
    if shim.pid <= 0 {
        return Err(SignalError::Failed(rustix::io::Errno::INVAL));
    }
    let Some(pid) = rustix::process::Pid::from_raw(shim.pid) else {
        return Err(SignalError::Failed(rustix::io::Errno::INVAL));
    };
    match rustix::process::kill_process(pid, sig) {
        Ok(()) => Ok(()),
        Err(e) if e == rustix::io::Errno::SRCH => Err(SignalError::Gone),
        Err(e) => Err(SignalError::Failed(e)),
    }
}

/// Every container id known to the daemon, running or not.
async fn list_container_ids(docker: &Docker) -> Result<HashSet<String>, ReapError> {
    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;
    Ok(containers.into_iter().filter_map(|c| c.id).collect())
}

/// Docker's conventional 12-character short form, for readable output.
fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}
