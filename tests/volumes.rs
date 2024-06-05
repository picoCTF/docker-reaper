//! Volume reaping tests.
//!
//! These are run serially because all test-related resources are cleaned up after each test.

mod common;

use std::collections::HashMap;

use common::{cleanup, create_volume, docker_client, volume_exists, TEST_LABEL};
use docker_reaper::{
    reap_volumes, Filter, ReapVolumesConfig, RemovalStatus, Resource, ResourceType,
};
use serial_test::serial;
use tokio::time::{sleep, Duration};

/// Test that only volumes older than the `min_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn min_age() {
    let old_volume_id = create_volume(None).await;
    sleep(Duration::from_secs(2)).await;
    let new_volume_id = create_volume(None).await;
    reap_volumes(
        docker_client(),
        &ReapVolumesConfig {
            dry_run: false,
            min_age: Some(Duration::from_secs(2)),
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap volumes");
    assert_eq!(volume_exists(&old_volume_id).await, false);
    assert_eq!(volume_exists(&new_volume_id).await, true);
    cleanup().await;
}

/// Test that only volumes younger than the `max_age` threshold are reaped.
#[tokio::test]
#[serial]
async fn max_age() {
    let old_volume_id = create_volume(None).await;
    sleep(Duration::from_secs(2)).await;
    let new_volume_id = create_volume(None).await;
    reap_volumes(
        docker_client(),
        &ReapVolumesConfig {
            dry_run: false,
            min_age: None,
            max_age: Some(Duration::from_secs(2)),
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap volumes");
    assert_eq!(volume_exists(&old_volume_id).await, true);
    assert_eq!(volume_exists(&new_volume_id).await, false);
    cleanup().await;
}

/// Test that only volumes matching the specified filters are reaped.
#[tokio::test]
#[serial]
async fn filters() {
    let purple_volume_id = create_volume(Some(HashMap::from([(
        "color".to_string(),
        "purple".to_string(),
    )])))
    .await;
    let orange_volume_id = create_volume(Some(HashMap::from([(
        "color".to_string(),
        "orange".to_string(),
    )])))
    .await;
    reap_volumes(
        docker_client(),
        &ReapVolumesConfig {
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
    .expect("failed to reap volumes");
    assert_eq!(volume_exists(&purple_volume_id).await, true);
    assert_eq!(volume_exists(&orange_volume_id).await, false);
    cleanup().await;
}

/// Test that resources are identified but not removed if `dry_run` is set.
#[tokio::test]
#[serial]
async fn dry_run() {
    let volume_id = create_volume(None).await;
    let result = reap_volumes(
        docker_client(),
        &ReapVolumesConfig {
            dry_run: true,
            min_age: None,
            max_age: None,
            filters: &vec![Filter::new("label", TEST_LABEL)],
        },
    )
    .await
    .expect("failed to reap volumes");
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Volume,
        id: volume_id.clone(),
        name: String::new(),
        status: RemovalStatus::Eligible
    }));
    assert_eq!(volume_exists(&volume_id).await, true);
    cleanup().await;
}
