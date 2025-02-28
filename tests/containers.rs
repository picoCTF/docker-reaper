//! Container reaping tests.
//!
//! These are run serially because all test-related resources are cleaned up after each test.

mod common;

use std::collections::HashMap;

use common::{
    RunContainerResult, TEST_LABEL, cleanup, container_exists, docker_client, network_exists,
    run_container,
};
use docker_reaper::{
    Filter, ReapContainersConfig, RemovalStatus, Resource, ResourceType, reap_containers,
};
use serial_test::serial;
use tokio::time::{Duration, sleep};

/// Test that only containers older than the `min_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn min_age() {
    let RunContainerResult {
        container_id: ref old_container_id,
        ..
    } = run_container(false, None).await;
    sleep(Duration::from_secs(2)).await;
    let RunContainerResult {
        container_id: ref new_container_id,
        ..
    } = run_container(false, None).await;
    reap_containers(
        docker_client(),
        &ReapContainersConfig {
            dry_run: false,
            min_age: Some(Duration::from_secs(2)),
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
            reap_networks: false,
        },
    )
    .await
    .expect("failed to reap containers");
    assert_eq!(container_exists(old_container_id).await, false);
    assert_eq!(container_exists(new_container_id).await, true);
    cleanup().await;
}

/// Test that only containers younger than the `max_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn max_age() {
    let RunContainerResult {
        container_id: ref old_container_id,
        ..
    } = run_container(false, None).await;
    sleep(Duration::from_secs(2)).await;
    let RunContainerResult {
        container_id: ref new_container_id,
        ..
    } = run_container(false, None).await;
    reap_containers(
        docker_client(),
        &ReapContainersConfig {
            dry_run: false,
            min_age: None,
            max_age: Some(Duration::from_secs(2)),
            filters: &vec![Filter::new("label", TEST_LABEL)],
            reap_networks: false,
        },
    )
    .await
    .expect("failed to reap containers");
    assert_eq!(container_exists(old_container_id).await, true);
    assert_eq!(container_exists(new_container_id).await, false);
    cleanup().await;
}

/// Test that only containers matching the specified filters are reaped.
#[tokio::test]
#[serial]
async fn filters() {
    let RunContainerResult {
        container_id: ref purple_container_id,
        ..
    } = run_container(
        false,
        Some(HashMap::from([("color".to_string(), "purple".to_string())])),
    )
    .await;
    let RunContainerResult {
        container_id: ref orange_container_id,
        ..
    } = run_container(
        false,
        Some(HashMap::from([("color".to_string(), "orange".to_string())])),
    )
    .await;
    reap_containers(
        docker_client(),
        &ReapContainersConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![
                Filter::new("label", TEST_LABEL),
                Filter::new("label", "color=orange"),
            ],
            reap_networks: false,
        },
    )
    .await
    .expect("failed to reap containers");
    assert_eq!(container_exists(purple_container_id).await, true);
    assert_eq!(container_exists(orange_container_id).await, false);
    cleanup().await;
}

/// Test that container-associated networks are also removed if `reap_networks` is set.
#[tokio::test]
#[serial]
async fn reap_networks() {
    let RunContainerResult {
        container_id,
        network_id,
    } = run_container(true, None).await;
    reap_containers(
        docker_client(),
        &ReapContainersConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
            reap_networks: true,
        },
    )
    .await
    .expect("failed to reap containers");
    assert_eq!(
        network_exists(&network_id.expect("network ID not present")).await,
        false
    );
    assert_eq!(container_exists(&container_id).await, false);
    cleanup().await;
}

/// Test that resources are identified but not removed if `dry_run` is set.
#[tokio::test]
#[serial]
async fn dry_run() {
    let RunContainerResult {
        container_id,
        network_id,
    } = run_container(true, None).await;
    let result = reap_containers(
        docker_client(),
        &ReapContainersConfig {
            dry_run: true,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
            reap_networks: true,
        },
    )
    .await
    .expect("failed to reap containers");
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Container,
        id: container_id.clone(),
        name: String::new(),
        status: RemovalStatus::Eligible,
    }));
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Network,
        id: network_id.clone().expect("network ID not present"),
        name: String::new(),
        status: RemovalStatus::Eligible,
    }));
    assert_eq!(
        network_exists(&network_id.expect("network ID not present")).await,
        true
    );
    assert_eq!(container_exists(&container_id).await, true);
    cleanup().await;
}
