//! Container reaping tests.
//!
//! These are run serially because all test-related resources are cleaned up after each test.

use std::collections::HashMap;

use super::common::{
    RunContainerResult, TEST_LABEL, cleanup, container_exists, docker_client, network_exists,
    run_container,
};
use crate::reaper::{
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
            record_image_use: None,
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
            record_image_use: None,
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
            record_image_use: None,
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
            record_image_use: None,
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
            record_image_use: None,
        },
    )
    .await
    .expect("failed to reap containers");
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Container,
        id: container_id.clone(),
        name: String::new(),
        details: String::new(),
        status: RemovalStatus::Eligible,
    }));
    assert!(result.contains(&Resource {
        resource_type: ResourceType::Network,
        id: network_id.clone().expect("network ID not present"),
        name: String::new(),
        details: String::new(),
        status: RemovalStatus::Eligible,
    }));
    assert_eq!(
        network_exists(&network_id.expect("network ID not present")).await,
        true
    );
    assert_eq!(container_exists(&container_id).await, true);
    cleanup().await;
}

/// Test that `record_image_use` stamps the image of every matching container, including one
/// too young to reap; that the run creating the record starts it with every image already on
/// the host at 0, and a later run neither does that again nor drops entries; and that a dry
/// run leaves the record alone.
#[tokio::test]
#[serial]
async fn record_image_use() {
    let RunContainerResult { container_id, .. } = run_container(false, None).await;
    let image_id = docker_client()
        .inspect_container(&container_id, None)
        .await
        .expect("failed to inspect container")
        .image
        .expect("container has no image id");
    let dir = std::env::temp_dir().join(format!(
        "docker-reaper-record-image-use-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("image-use");
    let filters = vec![Filter::new("label", TEST_LABEL)];
    let config = |dry_run| ReapContainersConfig {
        dry_run,
        min_age: Some(Duration::from_secs(3600)),
        max_age: None,
        filters: &filters,
        reap_networks: false,
        record_image_use: Some(path.clone()),
    };

    reap_containers(docker_client(), &config(true))
        .await
        .expect("failed to reap containers");
    assert!(!path.exists(), "a dry run wrote the record");

    let before = crate::usage::now_secs();
    reap_containers(docker_client(), &config(false))
        .await
        .expect("failed to reap containers");
    let record = crate::usage::load(&path).expect("failed to read the record");
    let stamp = *record
        .get(&image_id)
        .expect("the container's image was not recorded");
    assert!(
        stamp >= before,
        "stamp {stamp} predates the run at {before}"
    );
    assert!(
        container_exists(&container_id).await,
        "the container was too young to reap"
    );
    // Other containers may be using other images, but none carries TEST_LABEL.
    let on_host = docker_client()
        .list_images(None::<bollard::query_parameters::ListImagesOptions>)
        .await
        .expect("failed to list images");
    for image in on_host.iter().filter(|image| image.id != image_id) {
        assert_eq!(
            record.get(&image.id),
            Some(&0),
            "{} was on the host when the record started",
            image.id
        );
    }

    let mut edited = record.clone();
    edited.insert("sha256:gone".to_string(), 5);
    crate::usage::save(&path, &edited).unwrap();
    reap_containers(docker_client(), &config(false))
        .await
        .expect("failed to reap containers");
    let record = crate::usage::load(&path).expect("failed to read the record");
    assert_eq!(
        record.get("sha256:gone"),
        Some(&5),
        "a later run must leave other entries alone"
    );
    assert!(record[&image_id] >= stamp);

    std::fs::remove_dir_all(&dir).ok();
    cleanup().await;
}
