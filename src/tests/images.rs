//! Unit tests for image eviction planning.
//!
//! Unlike the other resource types, these do not run against a live Docker
//! daemon: an integration test would have to delete the host's real images
//! (eligibility depends on global disk usage, which a test cannot safely
//! manufacture). The docker-facing plumbing reuses the same list/remove
//! patterns as the other subcommands; the eviction policy is pure and tested
//! here.

use crate::reaper::{
    ImageCandidate, describe_image, order_least_recently_used, plan_image_evictions, settle_record,
    stamp_for_eviction,
};
use crate::usage::LastUsed;
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

fn ids(plan: &[ImageCandidate]) -> Vec<&str> {
    plan.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn least_recently_used_evicted_first_whatever_its_size() {
    let images = vec![
        image("sha256:big", Some("big:1"), 5000, 0),
        image("sha256:mid", Some("mid:1"), 800, 0),
        image("sha256:small", Some("small:1"), 10, 0),
    ];
    let mut plan = plan_image_evictions(&images, &HashSet::new());
    let last_used = LastUsed::from([
        ("sha256:big".to_string(), 300),
        ("sha256:mid".to_string(), 200),
        ("sha256:small".to_string(), 100),
    ]);
    order_least_recently_used(&mut plan, &last_used);
    assert_eq!(ids(&plan), vec!["sha256:small", "sha256:mid", "sha256:big"]);
}

#[test]
fn images_last_used_together_stay_largest_first() {
    let images = vec![
        image("sha256:small", Some("small:1"), 10, 0),
        image("sha256:big", Some("big:1"), 5000, 0),
        image("sha256:recent", Some("recent:1"), 9000, 0),
    ];
    let mut plan = plan_image_evictions(&images, &HashSet::new());
    let last_used = LastUsed::from([
        ("sha256:small".to_string(), 100),
        ("sha256:big".to_string(), 100),
        ("sha256:recent".to_string(), 900),
    ]);
    order_least_recently_used(&mut plan, &last_used);
    assert_eq!(
        ids(&plan),
        vec!["sha256:big", "sha256:small", "sha256:recent"]
    );
}

#[test]
fn an_image_missing_from_the_record_goes_last() {
    // reap_images stamps unseen images before ordering; this is the ordering's own fallback.
    let images = vec![
        image("sha256:unseen", Some("unseen:1"), 5000, 0),
        image("sha256:old", Some("old:1"), 10, 0),
    ];
    let mut plan = plan_image_evictions(&images, &HashSet::new());
    order_least_recently_used(
        &mut plan,
        &LastUsed::from([("sha256:old".to_string(), 100)]),
    );
    assert_eq!(ids(&plan), vec!["sha256:old", "sha256:unseen"]);
}

#[test]
fn describes_size_and_last_use() {
    let candidate = |id: &str| ImageCandidate {
        id: id.to_string(),
        name: id.to_string(),
        unique_size: 3 * 1024 * 1024,
    };
    let record = LastUsed::from([
        ("sha256:seeded".to_string(), 0),
        ("sha256:used".to_string(), 100_000 - 3 * 3600 - 20 * 60),
    ]);
    assert_eq!(
        describe_image(&candidate("sha256:used"), Some(&record), 100_000),
        "3.0 MiB, last used 3h20m ago"
    );
    assert_eq!(
        describe_image(&candidate("sha256:seeded"), Some(&record), 100_000),
        "3.0 MiB, not used since the record began"
    );
    assert_eq!(
        describe_image(&candidate("sha256:used"), None, 100_000),
        "3.0 MiB"
    );
}

#[test]
fn an_eviction_pass_stamps_what_is_in_use_and_what_it_has_never_seen() {
    let images = vec![
        image("sha256:old", Some("old:1"), 10, 0),
        image("sha256:running", Some("running:1"), 10, 0),
        image("sha256:pulled", Some("pulled:1"), 10, 0),
    ];
    let mut record = LastUsed::from([
        ("sha256:old".to_string(), 100),
        ("sha256:running".to_string(), 100),
    ]);
    stamp_for_eviction(
        &mut record,
        &HashSet::from(["sha256:running".to_string()]),
        &images,
        900,
    );
    assert_eq!(
        record["sha256:old"], 100,
        "unused and known: left as it was"
    );
    assert_eq!(record["sha256:running"], 900, "in use: now");
    assert_eq!(record["sha256:pulled"], 900, "never seen: now, not oldest");
}

#[test]
fn settling_drops_removed_images_and_prunes_only_after_a_complete_listing() {
    let listed = vec![
        image("sha256:kept", Some("kept:1"), 10, 0),
        image("sha256:evicted", Some("evicted:1"), 10, 0),
    ];
    let fresh = || {
        LastUsed::from([
            ("sha256:kept".to_string(), 1),
            ("sha256:evicted".to_string(), 2),
            ("sha256:unlisted".to_string(), 3),
        ])
    };
    let removed = HashSet::from(["sha256:evicted"]);

    let mut record = fresh();
    settle_record(&mut record, &listed, &removed, true);
    assert_eq!(record, LastUsed::from([("sha256:kept".to_string(), 1)]));

    // A filtered listing leaves out images that still exist: keep what it did not show.
    let mut record = fresh();
    settle_record(&mut record, &listed, &removed, false);
    assert_eq!(
        record,
        LastUsed::from([
            ("sha256:kept".to_string(), 1),
            ("sha256:unlisted".to_string(), 3),
        ])
    );
}
