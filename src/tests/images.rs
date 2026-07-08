//! Unit tests for image eviction planning.
//!
//! Unlike the other resource types, these do not run against a live Docker
//! daemon: an integration test would have to delete the host's real images
//! (eligibility depends on global disk usage, which a test cannot safely
//! manufacture). The docker-facing plumbing reuses the same list/remove
//! patterns as the other subcommands; the eviction policy is pure and tested
//! here.

use crate::reaper::{ImageCandidate, plan_image_evictions};
use bollard::models::ImageSummary;
use std::collections::HashSet;

fn image(id: &str, tag: Option<&str>, size: i64, shared_size: i64) -> ImageSummary {
    ImageSummary {
        id: id.to_string(),
        repo_tags: tag.map(|t| vec![t.to_string()]).unwrap_or_default(),
        size,
        shared_size,
        ..Default::default()
    }
}

#[test]
fn images_in_use_are_protected() {
    let images = vec![
        image("sha256:aaa", Some("challenge-a:1"), 500, 0),
        image("sha256:bbb", Some("challenge-b:1"), 100, 0),
    ];
    let in_use = HashSet::from(["sha256:aaa".to_string()]);
    let plan = plan_image_evictions(&images, &in_use);
    assert_eq!(
        plan,
        vec![ImageCandidate {
            id: "sha256:bbb".to_string(),
            name: "challenge-b:1".to_string(),
            unique_size: 100,
        }]
    );
}

#[test]
fn largest_unique_size_evicted_first() {
    // bbb is the largest by total size, but most of it is shared layers;
    // aaa reclaims the most when removed and must come first.
    let images = vec![
        image("sha256:ccc", Some("c:1"), 50, 0),
        image("sha256:bbb", Some("b:1"), 1000, 940),
        image("sha256:aaa", Some("a:1"), 400, 100),
    ];
    let plan = plan_image_evictions(&images, &HashSet::new());
    let order: Vec<&str> = plan.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(order, vec!["sha256:aaa", "sha256:bbb", "sha256:ccc"]);
    assert_eq!(plan[0].unique_size, 300);
    assert_eq!(plan[1].unique_size, 60);
}

#[test]
fn unknown_shared_size_treated_as_fully_unique() {
    // The API reports -1 when shared size was not computed.
    let images = vec![image("sha256:aaa", Some("a:1"), 400, -1)];
    let plan = plan_image_evictions(&images, &HashSet::new());
    assert_eq!(plan[0].unique_size, 400);
}

#[test]
fn untagged_images_fall_back_to_id() {
    let images = vec![image("sha256:aaa", None, 400, 0)];
    let plan = plan_image_evictions(&images, &HashSet::new());
    assert_eq!(plan[0].name, "sha256:aaa");
}
