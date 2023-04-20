use bollard::container::{ListContainersOptions, RemoveContainerOptions};
use bollard::network::ListNetworksOptions;
use bollard::Docker;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use tabled::Tabled;
use thiserror::Error;
use tokio::time::Duration;
use tracing::{debug, warn};

#[derive(Debug)]
pub struct ReapContainersConfig<'a> {
    /// Return results without actually removing containers or networks.
    pub dry_run: bool,
    /// Only containers older than this duration will be eligible for reaping.
    pub min_age: Option<Duration>,
    /// Only containers younger than this duration will be eligible for reaping.
    pub max_age: Option<Duration>,
    /// Additional Docker Engine-supported [container filters](https://docs.docker.com/engine/reference/commandline/ps/#filter).
    pub filters: &'a Vec<Filter>,
    /// Also attempt to remove the networks associated with reaped containers.
    pub reap_networks: bool,
}

#[derive(Debug)]
enum RemovalStatus {
    /// Used in dry-run mode to indicate that a resource is eligible for removal.
    Eligible,
    /// Resource was successfully removed.
    Success,
    /// Removal was already in progress.
    InProgress,
    /// An error occurred when attempting to remove this resource.
    Error(RemovalError),
}

impl fmt::Display for RemovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eligible => write!(f, "Eligible for removal"),
            Self::Success => write!(f, "Removed"),
            &Self::InProgress => write!(f, "Removal in progress"),
            Self::Error(e) => write!(f, "Error: {}", e),
        }
    }
}

#[derive(Clone, Debug)]
/// A Docker Engine filter (see https://docs.docker.com/engine/reference/commandline/ps/#filter)
pub struct Filter {
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
    pub fn new(name: &str, value: &str) -> Self {
        Self {
            name: String::from(name),
            value: String::from(value),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ResourceType {
    Container,
    Network,
    #[allow(dead_code)]
    Volume,
}

impl fmt::Display for ResourceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Container => {
                write!(f, "Container")
            }
            Self::Network => {
                write!(f, "Network")
            }
            Self::Volume => {
                write!(f, "Volume")
            }
        }
    }
}

#[derive(Debug, Tabled)]
#[tabled(rename_all = "PascalCase")]
pub struct Resource {
    #[tabled(rename = "Resource Type")]
    resource_type: ResourceType,
    #[tabled(skip)]
    #[allow(dead_code)]
    id: String,
    name: String,
    status: RemovalStatus,
}

impl Resource {
    /// Attempts to remove this resource.
    /// After competion, the resource's `status` will be either `RemovalStatus::Success` or
    /// `RemovalStatus::Error`.
    async fn remove(&mut self, docker: &Docker) {
        debug!("Removing {} {}", self.resource_type, self.name);
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
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 409,
                        ..
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
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => {
                        // Mark as successful if already removed (404)
                        self.status = RemovalStatus::Success;
                    }
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 409,
                        ..
                    }) => {
                        self.status = RemovalStatus::InProgress;
                    }
                    Err(e) => self.status = RemovalStatus::Error(RemovalError::Docker(e)),
                };
            }
            ResourceType::Volume => todo!(),
        }
    }
}

/// Error encountered while removing a resource.
#[derive(Error, Debug)]
pub enum RemovalError {
    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),
}

/// Unrecoverable error encountered during a reap iteration.
#[derive(Error, Debug)]
pub enum ReapError {
    #[error(transparent)]
    Docker(#[from] bollard::errors::Error),
    #[error(transparent)]
    InvalidSystemTime(#[from] std::time::SystemTimeError),
    #[error(transparent)]
    TaskFailure(#[from] tokio::task::JoinError),
    #[error("min_age must be less than max_age")]
    InvalidAgeBound,
}

pub async fn reap_containers(
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
            filters: config.filters.to_bollard_filters(),
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
                warn!("Skipped container ({}): missing creation timestamp", id);
                return false
            };
            let creation_secs: u64 = match creation_secs.try_into() {
                Ok(secs) => secs,
                Err(_) => {
                    warn!("Skipped container ({}): negative creation timestamp", id);
                    return false;
                }
            };
            let Some(age) = now.checked_sub(Duration::from_secs(creation_secs)) else {
                warn!("Skipped container {}: creation timestamp after system time", id);
                return false
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
            continue
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
            status: RemovalStatus::Eligible,
        });
        if config.reap_networks {
            if let Some(network_settings) = container.network_settings {
                if let Some(networks) = network_settings.networks {
                    // Docker has network IDs, but also requires each network to have a unique
                    // name. We just use the name as an ID since it's easier to retrieve.
                    eligible_network_names.extend(networks.keys().cloned().inspect(|name| {
                        debug!("Added network {} from container {} ", name, id);
                    }))
                }
            }
        }
    }
    for network_name in eligible_network_names {
        eligible_resources.push(Resource {
            resource_type: ResourceType::Network,
            id: network_name.clone(),
            name: network_name.clone(),
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

}
