use bollard::container::RemoveContainerOptions;
use bollard::{container::ListContainersOptions, Docker};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};
use tabled::Tabled;
use thiserror::Error;
use tokio::time::Duration;
use tracing::{debug, info};

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
    /// Removal of the resource is in progress. Typically not shown in results unless a timeout
    /// occurs.
    InProgress,
    /// Resource was successfully removed.
    Success,
    /// An error occurred when attempting to remove this resource.
    Error(RemovalError),
}

impl fmt::Display for RemovalStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eligible => write!(f, "Eligible for removal"),
            Self::InProgress => write!(f, "Removal in progress"),
            Self::Success => write!(f, "Removed"),
            Self::Error(e) => write!(f, "Error: {}", e.to_string()),
        }
    }
}

#[derive(Clone, Debug)]
/// A Docker Engine filter (see https://docs.docker.com/engine/reference/commandline/ps/#filter)
pub struct Filter {
    name: String,
    value: String,
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
pub struct RemovedResource {
    #[tabled(rename = "Resource Type")]
    resource_type: ResourceType,
    #[tabled(skip)]
    #[allow(dead_code)]
    id: String,
    name: String,
    status: RemovalStatus,
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
    #[error("Current system time is before UNIX epoch")]
    InvalidSystemTime,
}

pub async fn reap_containers(
    docker: &Docker,
    config: &ReapContainersConfig<'_>,
) -> Result<Vec<RemovedResource>, ReapError> {
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return Err(ReapError::InvalidSystemTime),
    };

    let eligible_containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: {
                // Flatten any filter values with the same key into vecs to match
                // bollard::container::ListContainersOptions format ([a=b, a=c] => [a=[b, c]])
                config.filters.iter().fold(HashMap::new(), |mut acc, f| {
                    acc.entry(f.name.clone()).or_default().push(f.value.clone());
                    acc
                })
            },
            ..Default::default()
        }))
        .await?
        .into_iter()
        .filter(|c| {
            (config.max_age.is_none() && config.min_age.is_none()) || {
                if let Some(creation_secs) = c.created {
                    let creation_secs: u64 = match creation_secs.try_into() {
                        Ok(s) => s,
                        Err(_) => return false,
                    };
                    let age = now - Duration::from_secs(creation_secs);
                    return age > config.min_age.unwrap_or(Duration::ZERO)
                        && age < config.max_age.unwrap_or(Duration::MAX);
                } else {
                    false
                }
            }
        });

    let mut eligible_networks = HashSet::new();
    let mut resources: Vec<RemovedResource> = Vec::new();
    for container in eligible_containers {
        if let Some(id) = container.id {
            if config.reap_networks {
                if let Some(network_settings) = container.network_settings {
                    if let Some(networks) = network_settings.networks {
                        // Docker has network IDs, but also requires each network to have a unique
                        // name. We just use names as IDs since they're easier to retrieve.
                        eligible_networks.extend(networks.keys().cloned())
                    }
                }
            }
            resources.push(RemovedResource {
                resource_type: ResourceType::Container,
                id: id.clone(),
                name: container
                    .names
                    .unwrap_or_default()
                    .first()
                    .unwrap_or_else(|| &id)
                    .clone(),
                status: RemovalStatus::Eligible,
            });
        }
    }
    for network in eligible_networks {
        resources.push(RemovedResource {
            resource_type: ResourceType::Network,
            id: network.clone(),
            name: network.clone(),
            status: RemovalStatus::Eligible,
        })
    }
    if config.dry_run {
        return Ok(resources);
    }
    unimplemented!()
}
