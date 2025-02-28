//! Network reaping tests.
//!
//! These are run serially because all test-related resources are cleaned up after each test.

mod common;

use std::collections::HashMap;

use common::{TEST_LABEL, cleanup, create_network, docker_client, network_exists};
use docker_reaper::{
    Filter, ReapNetworksConfig, RemovalStatus, Resource, ResourceType, reap_networks,
};
use serial_test::serial;
use tokio::time::{Duration, sleep};

/// Test that only networks older than the `min_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn min_age() {
    let old_network_id = create_network(None).await;
    sleep(Duration::from_secs(2)).await;
    let new_network_id = create_network(None).await;
    reap_networks(
        docker_client(),
        &ReapNetworksConfig {
            dry_run: false,
            min_age: Some(Duration::from_secs(2)),
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap networks");
    assert_eq!(network_exists(&old_network_id).await, false);
    assert_eq!(network_exists(&new_network_id).await, true);
    cleanup().await;
}

/// Test that only networks younger than the `max_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn max_age() {
    let old_network_id = create_network(None).await;
    sleep(Duration::from_secs(2)).await;
    let new_network_id = create_network(None).await;
    reap_networks(
        docker_client(),
        &ReapNetworksConfig {
            dry_run: false,
            min_age: None,
            max_age: Some(Duration::from_secs(2)),
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap networks");
    assert_eq!(network_exists(&old_network_id).await, true);
    assert_eq!(network_exists(&new_network_id).await, false);
    cleanup().await;
}

/// Test that only networks matching the specified filters are reaped.
#[tokio::test]
#[serial]
async fn filters() {
    let purple_network_id = create_network(Some(HashMap::from([(
        "color".to_string(),
        "purple".to_string(),
    )])))
    .await;
    let orange_network_id = create_network(Some(HashMap::from([(
        "color".to_string(),
        "orange".to_string(),
    )])))
    .await;
    reap_networks(
        docker_client(),
        &ReapNetworksConfig {
            dry_run: false,
            min_age: None,
            max_age: None,
            filters: &vec![
                Filter::new("label", TEST_LABEL),
                Filter::new("label", "color=orange"),
            ],
        },
    )
    .await
    .expect("failed to reap networks");
    assert_eq!(network_exists(&purple_network_id).await, true);
    assert_eq!(network_exists(&orange_network_id).await, false);
    cleanup().await;
}

/// Test that resources are identified but not removed if `dry_run` is set.
#[tokio::test]
#[serial]
async fn dry_run() {
    let network_id = create_network(None).await;
    let result = reap_networks(
        docker_client(),
        &ReapNetworksConfig {
            dry_run: true,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap networks");
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Network,
        id: network_id.clone(),
        name: String::new(),
        status: RemovalStatus::Eligible
    }));
    assert_eq!(network_exists(&network_id).await, true);
    cleanup().await;
}
