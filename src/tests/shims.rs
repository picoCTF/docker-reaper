//! Orphaned shim sweep tests.
//!
//! `reap_shims` decides what to signal by cross-referencing the local process table
//! against the daemon's container list. These drive that decision against a real daemon
//! while pointing `proc_root` at a fixture tree, so the filtering can be exercised
//! without needing genuinely orphaned shims on the test machine.
//!
//! All of these run in dry-run mode: they assert which shims are *selected*, and never
//! signal a process.

use std::fs;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::common::{RunContainerResult, cleanup, docker_client, run_container};
use crate::reaper::{ReapShimsConfig, RemovalStatus, ResourceType, reap_shims};
use serial_test::serial;
use tokio::time::Duration;

// Container ids must be 64 hex characters, which the parser now requires.
const BOGUS_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOGUS_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BOGUS_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// Fixture uptime. Start times below are derived from it so ages are exact.
const UPTIME_SECS: u64 = 1000;

fn fixture_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "docker-reaper-reap-shims-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("uptime"),
        format!("{UPTIME_SECS}.00 {UPTIME_SECS}.00"),
    )
    .unwrap();
    // reap_shims compares each candidate's PID namespace against its own, so the fixture
    // needs a self/ns/pid to compare with. Symlink targets need not exist.
    fs::create_dir_all(root.join("self").join("ns")).unwrap();
    symlink(HOST_NS, root.join("self").join("ns").join("pid")).unwrap();
    root
}

/// Stand-in for the host PID namespace a genuine shim lives in.
const HOST_NS: &str = "pid:[4026531836]";
/// A different namespace, as a process inside a container would report.
const CONTAINER_NS: &str = "pid:[4026533077]";

/// Writes a fake `/proc/<pid>` entry for a shim of the given age.
fn write_shim(root: &Path, pid: i32, namespace: &str, container_id: &str, age_secs: u64) {
    write_shim_in_ns(root, pid, namespace, container_id, age_secs, HOST_NS)
}

/// As [write_shim], but places the process in an explicit PID namespace.
fn write_shim_in_ns(
    root: &Path,
    pid: i32,
    namespace: &str,
    container_id: &str,
    age_secs: u64,
    pid_ns: &str,
) {
    let ticks = rustix::param::clock_ticks_per_second();
    let start_ticks = (UPTIME_SECS - age_secs) * ticks;
    let dir = root.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    let args = [
        "/usr/bin/containerd-shim-runc-v2",
        "-namespace",
        namespace,
        "-id",
        container_id,
        "-address",
        "/run/containerd/containerd.sock",
    ];
    let mut cmdline = Vec::new();
    for a in args {
        cmdline.extend_from_slice(a.as_bytes());
        cmdline.push(0);
    }
    fs::write(dir.join("cmdline"), cmdline).unwrap();
    fs::write(
        dir.join("stat"),
        format!("{pid} (shim) S 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 0 0"),
    )
    .unwrap();
    fs::create_dir_all(dir.join("ns")).unwrap();
    symlink(pid_ns, dir.join("ns").join("pid")).unwrap();
}

fn config(root: &Path, min_age: Duration) -> ReapShimsConfig {
    ReapShimsConfig {
        dry_run: true,
        min_age,
        namespace: String::from("moby"),
        settle: Duration::from_millis(1),
        grace: Duration::from_millis(1),
        proc_root: root.to_path_buf(),
        runtime_root: root.join("no-task-state"),
    }
}

/// A shim whose container still exists must never be selected, however old it is. This is
/// the property that stops the sweep from killing a live container.
#[tokio::test]
#[serial]
async fn shims_of_live_containers_are_never_selected() {
    let RunContainerResult {
        container_id: ref live_id,
        ..
    } = run_container(false, None).await;

    let root = fixture_root("live");
    write_shim(&root, 9001, "moby", live_id, 900);
    write_shim(&root, 9002, "moby", BOGUS_A, 900);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![BOGUS_A],
        "only the shim with no matching container should be selected"
    );
    assert_eq!(found[0].resource_type, ResourceType::Shim);
    assert!(matches!(found[0].status, RemovalStatus::Eligible));

    fs::remove_dir_all(&root).ok();
    cleanup().await;
}

/// Shims in another containerd namespace belong to a different workload and are out of
/// scope regardless of whether Docker knows the id.
#[tokio::test]
#[serial]
async fn other_namespaces_are_ignored() {
    let root = fixture_root("namespace");
    write_shim(&root, 9003, "k8s.io", BOGUS_B, 900);
    write_shim(&root, 9004, "moby", BOGUS_A, 900);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec![BOGUS_A], "k8s.io shims must be left alone");

    fs::remove_dir_all(&root).ok();
    cleanup().await;
}

/// A shim younger than `min_age` is spared, which is what keeps a container still being
/// created from being caught mid-flight.
#[tokio::test]
#[serial]
async fn shims_below_min_age_are_spared() {
    let root = fixture_root("minage");
    write_shim(&root, 9005, "moby", BOGUS_A, 900);
    write_shim(&root, 9006, "moby", BOGUS_C, 5);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![BOGUS_A],
        "the 5s-old shim is below the 60s minimum"
    );

    fs::remove_dir_all(&root).ok();
    cleanup().await;
}

/// With nothing orphaned there is no work, and in particular no settle delay.
#[tokio::test]
#[serial]
async fn no_orphans_selects_nothing() {
    let RunContainerResult {
        container_id: ref live_id,
        ..
    } = run_container(false, None).await;

    let root = fixture_root("clean");
    write_shim(&root, 9007, "moby", live_id, 900);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");
    assert!(found.is_empty());

    fs::remove_dir_all(&root).ok();
    cleanup().await;
}

/// Processes that are not shims at all are never considered.
#[tokio::test]
#[serial]
async fn non_shim_processes_are_ignored() {
    let root = fixture_root("nonshim");
    let dir = root.join("9008");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("cmdline"), b"/usr/bin/dockerd\0").unwrap();
    fs::write(
        dir.join("stat"),
        "9008 (dockerd) S 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0",
    )
    .unwrap();

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");
    assert!(found.is_empty());

    fs::remove_dir_all(&root).ok();
}

/// A process that merely *claims* to be a shim in its argv, but is not in this process's
/// PID namespace, is not a host shim. Everything else about a candidate comes from
/// /proc/<pid>/cmdline, which the target controls.
#[tokio::test]
#[serial]
async fn processes_outside_our_pid_namespace_are_ignored() {
    let root = fixture_root("foreign-ns");
    write_shim_in_ns(&root, 9101, "moby", BOGUS_A, 900, CONTAINER_NS);
    write_shim(&root, 9102, "moby", BOGUS_B, 900);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![BOGUS_B],
        "a process in another PID namespace must not be treated as a host shim"
    );

    fs::remove_dir_all(&root).ok();
}

/// A container id that is not 64 hex characters is not a Docker container id.
#[tokio::test]
#[serial]
async fn implausible_container_ids_are_ignored() {
    let root = fixture_root("bad-id");
    write_shim(&root, 9103, "moby", "not-a-container-id", 900);
    write_shim(&root, 9104, "moby", BOGUS_A, 900);

    let found = reap_shims(docker_client(), &config(&root, Duration::from_secs(60)))
        .await
        .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec![BOGUS_A]);

    fs::remove_dir_all(&root).ok();
}

/// Spawns a process whose argv looks exactly like a shim's, ignoring SIGTERM so the
/// SIGKILL escalation is exercised.
///
/// argv[0] is spoofed with `arg0` rather than by copying a binary, so this must exec a
/// real program: a `#!` script would have argv[0] replaced by the kernel. The program is a
/// copy of /bin/sh named like a shim, because `list_shims` also checks `/proc/<pid>/exe`,
/// which `arg0` cannot reach. The copy is deleted as soon as it is running: the process
/// keeps its inode, so this still works, `/proc/<pid>/exe` reads "<path> (deleted)" -- which
/// exercises the branch handling a containerd upgraded under a live shim -- and no artefact
/// outlives the test even if an assertion panics.
fn spawn_decoy_shim(namespace: &str, container_id: &str) -> std::process::Child {
    let dir = std::env::temp_dir().join(format!("docker-reaper-decoy-{}", &container_id[..12]));
    fs::create_dir_all(&dir).expect("failed to create decoy directory");
    let exe = dir.join(crate::shims::SHIM_EXE);
    fs::copy("/bin/sh", &exe).expect("failed to copy decoy binary");
    let child = Command::new(&exe)
        .arg0(&exe)
        .arg("-c")
        // The trailing ":" leaves the shell something to do after the sleep. Without it
        // bash exec-optimises itself away into sleep, taking the spoofed argv and the
        // shim-named exe with it, and the decoy stops being discoverable at all.
        .arg("trap '' TERM; sleep 30; :")
        .arg("-namespace")
        .arg(namespace)
        .arg("-id")
        .arg(container_id)
        .spawn()
        .expect("failed to spawn decoy shim");
    // spawn() only returns once the exec has been reported successful, so removing the
    // copy cannot race it.
    fs::remove_dir_all(&dir).ok();
    child
}

/// argv alone must never be enough to be selected.
///
/// This is the shape a hostile container would use: a real program with `arg0` set to a
/// shim's path. `/proc/<pid>/exe` still names what actually got exec'd, which is what
/// rejects it. Dry run, so nothing is signalled either way.
#[tokio::test]
#[serial]
async fn a_process_wearing_a_shims_argv_is_not_selected() {
    const NS: &str = "docker-reaper-argv-test";
    let impostor_id = "e".repeat(64);

    let mut impostor = Command::new("/bin/sh")
        .arg0("/usr/bin/containerd-shim-runc-v2")
        .arg("-c")
        .arg("trap '' TERM; sleep 30; :")
        .arg("-namespace")
        .arg(NS)
        .arg("-id")
        .arg(&impostor_id)
        .spawn()
        .expect("failed to spawn impostor");
    let impostor_pid = impostor.id() as i32;

    let found = reap_shims(
        docker_client(),
        &ReapShimsConfig {
            dry_run: true,
            min_age: Duration::ZERO,
            namespace: String::from(NS),
            settle: Duration::ZERO,
            grace: Duration::ZERO,
            proc_root: PathBuf::from("/proc"),
            runtime_root: PathBuf::from("/nonexistent"),
        },
    )
    .await
    .expect("failed to reap shims");

    // The impostor has to still be running, or this would pass for the wrong reason.
    assert!(
        crate::shims::is_alive(Path::new("/proc"), impostor_pid),
        "impostor exited before the sweep ran"
    );
    assert_eq!(
        found.len(),
        0,
        "a process wearing a shim's argv was selected"
    );

    let _ = impostor.kill();
    let _ = impostor.wait();
}

/// The signalling path, end to end against the real process table.
///
/// Deliberately uses a private containerd namespace rather than "moby": with the real
/// /proc as proc_root and a zero min-age, sweeping "moby" would match this machine's
/// genuine shims, including any briefly orphaned while other tests tear their containers
/// down. proc_root must be the real /proc regardless, because a fixture directory outlives
/// the process and would make the liveness check permanently true.
#[tokio::test]
#[serial]
async fn orphans_are_signalled_and_live_containers_are_spared() {
    const NS: &str = "docker-reaper-test";
    let orphan_id = "d".repeat(64);

    // A container that really exists, so its id is in list_containers(all).
    let RunContainerResult {
        container_id: ref live_id,
        ..
    } = run_container(false, None).await;

    let mut orphan = spawn_decoy_shim(NS, &orphan_id);
    let mut impostor = spawn_decoy_shim(NS, live_id);
    let orphan_pid = orphan.id() as i32;
    let impostor_pid = impostor.id() as i32;

    let found = reap_shims(
        docker_client(),
        &ReapShimsConfig {
            dry_run: false,
            min_age: Duration::ZERO,
            namespace: String::from(NS),
            settle: Duration::from_millis(200),
            grace: Duration::from_millis(300),
            proc_root: PathBuf::from("/proc"),
            runtime_root: PathBuf::from("/nonexistent"),
        },
    )
    .await
    .expect("failed to reap shims");

    let ids: Vec<&str> = found.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![orphan_id.as_str()],
        "only the shim with no matching container should have been signalled"
    );
    assert!(
        matches!(found[0].status, RemovalStatus::Success),
        "expected the orphan to be reported as removed, got {:?}",
        found[0].status
    );

    // Reap so the zombie does not linger; the status assertion above already required the
    // process to be gone, which is_alive reports correctly for a zombie.
    let _ = orphan.wait();
    assert!(
        !crate::shims::is_alive(Path::new("/proc"), orphan_pid),
        "orphan pid {orphan_pid} survived the sweep"
    );

    // The impostor named a live container, so check 1 must have spared it.
    assert!(
        crate::shims::is_alive(Path::new("/proc"), impostor_pid),
        "a shim naming a live container was signalled"
    );
    let _ = impostor.kill();
    let _ = impostor.wait();

    cleanup().await;
}
