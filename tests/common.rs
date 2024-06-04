//! Common utility functions for integration tests.
#![allow(dead_code)]

use bollard::container::{Config, NetworkingConfig};
use bollard::image::CreateImageOptions;
use bollard::network::CreateNetworkOptions;
use bollard::secret::{ContainerCreateResponse, EndpointSettings};
use bollard::Docker;
use chrono::Utc;
use docker_reaper::{
    reap_containers, reap_networks, reap_volumes, Filter, ReapContainersConfig, ReapNetworksConfig,
    ReapVolumesConfig,
};
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio_stream::StreamExt;

/// A label set on all test-created Docker resources.
pub(crate) const TEST_LABEL: &str = "docker-reaper-test";

/// Obtain a client for the local Docker daemon.
pub(crate) fn docker_client() -> &'static Docker {
    static CLIENT: OnceLock<Docker> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Docker::connect_with_local_defaults().expect("failed to connect to Docker daemon")
    })
}

/// Return type for [run_container] calls.
/// A network will not be created unless the `with_network` argument was `true`.
pub(crate) struct RunContainerResult {
    pub(crate) container_id: String,
    pub(crate) network_id: Option<String>,
}

/// Run a container on the local Docker daemon.
/// The label [TEST_LABEL] will always be set. Additional labels may also be specified.
pub(crate) async fn run_container(
    with_network: bool,
    extra_labels: Option<HashMap<String, String>>,
) -> RunContainerResult {
    static TEST_IMAGE: &'static str = "busybox:latest";

    let client = docker_client();
    let mut labels = HashMap::from([(TEST_LABEL.to_string(), "true".to_string())]);
    if let Some(extra_labels) = extra_labels {
        labels.extend(extra_labels.into_iter())
    }
    let mut network_id = None;
    if with_network {
        let name = Utc::now().timestamp_millis().to_string(); // network names must be unique
        client
            .create_network(CreateNetworkOptions {
                name: name.clone(),
                labels: labels.clone(),
                ..Default::default()
            })
            .await
            .expect("failed to create network");
        // We use names rather than actual IDs to uniquely identify networks in docker-reaper
        // because they are more meaningful in the user-facing output. Docker's handling of network
        // names vs. IDs is strange - they can effectively be used interchangably.
        network_id = Some(name);
    }

    // Ensure test image is present on host
    if client.inspect_image(&TEST_IMAGE).await.is_err() {
        let mut pull_results_stream = client.create_image(
            Some(CreateImageOptions {
                from_image: TEST_IMAGE,
                ..Default::default()
            }),
            None,
            None,
        );
        while let Some(result) = pull_results_stream.next().await {
            result.expect("failed to pull test image");
        }
    }

    let ContainerCreateResponse {
        id: container_id, ..
    } = client
        .create_container::<String, String>(
            None,
            Config {
                tty: Some(true),
                cmd: None,
                image: Some(TEST_IMAGE.to_string()),
                labels: Some(labels),
                networking_config: {
                    if with_network {
                        Some(NetworkingConfig {
                            endpoints_config: HashMap::from([(
                                "docker-reaper-test-network".to_string(),
                                EndpointSettings {
                                    network_id: network_id.clone(),
                                    ..Default::default()
                                },
                            )]),
                        })
                    } else {
                        None
                    }
                },
                ..Default::default()
            },
        )
        .await
        .expect("failed to create container");
    client
        .start_container::<&str>(&container_id, None)
        .await
        .expect(&format!("failed to start container {container_id}"));
    RunContainerResult {
        container_id,
        network_id,
    }
}

/// Check whether a container with the given ID exists.
pub(crate) async fn container_exists(id: &str) -> bool {
    let client = docker_client();
    match client.inspect_container(id, None).await {
        Ok(_) => return true,
        Err(err) => match err {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => return false,
            _ => panic!("unexpected error: {err}"),
        },
    }
}

/// Check whether a network with the given ID exists.
pub(crate) async fn network_exists(id: &str) -> bool {
    let client = docker_client();
    match client.inspect_network::<&str>(id, None).await {
        Ok(_) => return true,
        Err(err) => match err {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => return false,
            _ => panic!("unexpected error: {err}"),
        },
    }
}

/// Clean up all remaining test resources.
pub(crate) async fn cleanup() {
    let client = docker_client();
    reap_containers(
        client,
        &ReapContainersConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
            reap_networks: true,
        },
    )
    .await
    .expect("failed to clean up containers");

    reap_networks(
        client,
        &ReapNetworksConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to clean up networks");

    reap_volumes(
        client,
        &ReapVolumesConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to clean up volumes");
}
