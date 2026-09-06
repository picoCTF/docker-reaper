//! Discovery of orphaned `containerd-shim-runc-v2` processes.
//!
//! An orphaned shim cannot be reached through the Docker API: by definition its
//! container is already gone from the daemon's view, so `remove_container` returns 404.
//! Finding one means reading the local process table, which is why the `shims`
//! subcommand only works against a local daemon.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Executable name of the shim containerd starts for each container. This is the exe
/// name, not `/proc/<pid>/comm`, which the kernel truncates to 15 bytes (and which any
/// process can rewrite with prctl(PR_SET_NAME) anyway).
pub(crate) const SHIM_EXE: &str = "containerd-shim-runc-v2";

/// The containerd namespace Docker uses for its containers.
pub(crate) const DEFAULT_NAMESPACE: &str = "moby";

/// Default location of containerd's v2 runtime task state.
pub(crate) const DEFAULT_RUNTIME_ROOT: &str = "/run/containerd/io.containerd.runtime.v2.task";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShimProcess {
    pub(crate) pid: i32,
    pub(crate) namespace: String,
    pub(crate) container_id: String,
    /// Time since the shim process started.
    pub(crate) age: Duration,
}

/// Extracts the containerd namespace and container id from a shim's argv.
///
/// A shim is invoked roughly as:
/// `containerd-shim-runc-v2 -namespace moby -id <container-id> -address <sock>`
///
/// Returns `None` for any process that is not a shim, or whose argv lacks either value.
pub(crate) fn parse_shim_cmdline(cmdline: &[u8]) -> Option<(String, String)> {
    let args: Vec<String> = cmdline
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if !args.first()?.contains(SHIM_EXE) {
        return None;
    }
    let mut namespace: Option<String> = None;
    let mut container_id: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        let (flag, inline) = match args[i].split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (args[i].clone(), None),
        };
        let slot = match flag.as_str() {
            "-namespace" | "--namespace" => &mut namespace,
            "-id" | "--id" => &mut container_id,
            _ => {
                i += 1;
                continue;
            }
        };
        match inline {
            Some(value) => {
                *slot = Some(value);
                i += 1;
            }
            None => {
                *slot = args.get(i + 1).cloned();
                i += 2;
            }
        }
    }
    let container_id = container_id?;
    // Docker container ids are 64 hex characters. Requiring the shape costs nothing and
    // keeps a human-readable name out of the path built in `task_state_dir`.
    if container_id.len() != 64 || !container_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((namespace?, container_id))
}

/// Whether `pid` shares this process's PID namespace.
///
/// Everything else about a candidate comes from `/proc/<pid>/cmdline`, which is argv the
/// target chose for itself: a process inside a container can trivially present itself as a
/// shim. Namespace membership cannot be forged from inside a container, and a genuine
/// orphaned shim is a host process whatever state containerd left behind — so unlike a
/// check against containerd's own bookkeeping, this can never spare a real orphan.
///
/// Returns false when either link is unreadable, so an unreadable candidate is spared.
pub(crate) fn shares_pid_namespace(proc_root: &Path, pid: i32) -> bool {
    let ours = fs::read_link(proc_root.join("self").join("ns").join("pid"));
    let theirs = fs::read_link(proc_root.join(pid.to_string()).join("ns").join("pid"));
    match (ours, theirs) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether a process is a zombie, i.e. already dead and merely awaiting reaping.
///
/// `/proc/<pid>` outlives the process itself until its parent reaps it, so a bare
/// existence test reports a killed process as still running.
pub(crate) fn is_zombie(proc_root: &Path, pid: i32) -> bool {
    let Ok(stat) = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")) else {
        return false;
    };
    let Some(after_comm) = stat.rfind(')').and_then(|i| stat.get(i + 1..)) else {
        return false;
    };
    after_comm.split_whitespace().next() == Some("Z")
}

/// Reads a process's start time, in clock ticks since boot, from `/proc/<pid>/stat`.
///
/// The second field is the executable name in parentheses and may itself contain spaces
/// and parentheses, so parsing starts after the final `)`.
pub(crate) fn process_start_ticks(stat: &str) -> Option<u64> {
    let after_comm = stat.get(stat.rfind(')')? + 1..)?;
    // Fields after the name begin at field 3 (state); starttime is field 22.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// Whether `/proc/<pid>/exe` names the shim executable.
///
/// argv is the target's own; the exe link is the kernel's, so this rejects a process that
/// merely wears a shim's argv. An unreadable link KEEPS the candidate: sparing a genuine
/// orphan is the worse failure, and an unreadable link is not evidence of an impostor.
fn exe_names_the_shim(proc_dir: &Path) -> bool {
    let Ok(exe) = fs::read_link(proc_dir.join("exe")) else {
        return true;
    };
    let exe = exe.to_string_lossy();
    // containerd upgraded under a running shim leaves the kernel reporting "<path> (deleted)".
    let exe = exe.strip_suffix(" (deleted)").unwrap_or(&exe);
    Path::new(exe).file_name().and_then(|name| name.to_str()) == Some(SHIM_EXE)
}

/// Lists every shim process under `proc_root`.
///
/// `uptime` and `clock_ticks` are passed in rather than read here so this can be
/// exercised against a fixture directory in tests.
pub(crate) fn list_shims(
    proc_root: &Path,
    uptime: Duration,
    clock_ticks: u64,
) -> io::Result<Vec<ShimProcess>> {
    let mut shims = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = entry?;
        // Reject non-positive pids here, not just before signalling: a non-standard
        // proc_root can hold a directory named "0" or "-1", and kill() reads those as the
        // caller's process group and as every process it may signal.
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let dir = entry.path();
        // A process can exit between readdir and here; skip it rather than failing.
        let Ok(cmdline) = fs::read(dir.join("cmdline")) else {
            continue;
        };
        let Some((namespace, container_id)) = parse_shim_cmdline(&cmdline) else {
            continue;
        };
        if !exe_names_the_shim(&dir) {
            continue;
        }
        // An unreadable start time yields an age of zero, so the shim looks brand new and
        // is spared by any min-age filter. Failing safe is the right direction here.
        let age = fs::read_to_string(dir.join("stat"))
            .ok()
            .as_deref()
            .and_then(process_start_ticks)
            .filter(|_| clock_ticks > 0)
            // Keep the remainder: truncating to whole seconds would push the start time
            // earlier and make every process look up to a tick short of a second older
            // than it is, letting a shim clear --min-age before it actually has.
            .map(|ticks| {
                let nanos =
                    u128::from(ticks % clock_ticks) * 1_000_000_000 / u128::from(clock_ticks);
                let started = Duration::new(ticks / clock_ticks, nanos as u32);
                uptime.saturating_sub(started)
            })
            .unwrap_or(Duration::ZERO);
        shims.push(ShimProcess {
            pid,
            namespace,
            container_id,
            age,
        });
    }
    Ok(shims)
}

/// Re-reads a process's argv to confirm it is still the same shim for the same container.
///
/// Guards against the pid having been reused by an unrelated process between discovery
/// and the kill, which would otherwise make this subcommand capable of killing anything.
pub(crate) fn still_same_shim(proc_root: &Path, shim: &ShimProcess) -> bool {
    let path = proc_root.join(shim.pid.to_string()).join("cmdline");
    let Ok(cmdline) = fs::read(path) else {
        return false;
    };
    matches!(
        parse_shim_cmdline(&cmdline),
        Some((namespace, id)) if namespace == shim.namespace && id == shim.container_id
    )
}

/// Whether the process is still running.
///
/// A zombie counts as gone: it has already exited and is only waiting to be reaped, and
/// treating it as alive would report a successful kill as still in progress.
pub(crate) fn is_alive(proc_root: &Path, pid: i32) -> bool {
    proc_root.join(pid.to_string()).exists() && !is_zombie(proc_root, pid)
}

/// Path of containerd's task state directory for a container.
pub(crate) fn task_state_dir(runtime_root: &Path, namespace: &str, container_id: &str) -> PathBuf {
    runtime_root.join(namespace).join(container_id)
}

/// Reads the system uptime from `/proc/uptime`.
pub(crate) fn uptime(proc_root: &Path) -> io::Result<Duration> {
    let raw = fs::read_to_string(proc_root.join("uptime"))?;
    let secs: f64 = raw
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc/uptime"))?;
    Ok(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docker container ids are 64 hex characters; the parser requires that shape.
    const CID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CID_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn argv(args: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for a in args {
            out.extend_from_slice(a.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn parses_space_separated_flags() {
        let c = argv(&[
            "/usr/bin/containerd-shim-runc-v2",
            "-namespace",
            "moby",
            "-id",
            CID_A,
            "-address",
            "/run/containerd/containerd.sock",
        ]);
        assert_eq!(
            parse_shim_cmdline(&c),
            Some((String::from("moby"), String::from(CID_A)))
        );
    }

    #[test]
    fn parses_inline_values() {
        let id = format!("-id={CID_A}");
        let c = argv(&["containerd-shim-runc-v2", "-namespace=moby", &id]);
        assert_eq!(
            parse_shim_cmdline(&c),
            Some((String::from("moby"), String::from(CID_A)))
        );
    }

    #[test]
    fn ignores_non_shim_processes() {
        assert_eq!(parse_shim_cmdline(&argv(&["/usr/bin/dockerd"])), None);
        assert_eq!(parse_shim_cmdline(&argv(&["sleep", "-id", "abc"])), None);
    }

    #[test]
    fn requires_both_namespace_and_id() {
        assert_eq!(
            parse_shim_cmdline(&argv(&["containerd-shim-runc-v2", "-id", "abc"])),
            None
        );
        assert_eq!(
            parse_shim_cmdline(&argv(&["containerd-shim-runc-v2", "-namespace", "moby"])),
            None
        );
    }

    #[test]
    fn implausible_container_ids_are_rejected() {
        for bad in [
            "short",
            "not-hex-not-hex",
            &"a".repeat(63),
            &"a".repeat(65),
            &"g".repeat(64),
        ] {
            assert_eq!(
                parse_shim_cmdline(&argv(&[
                    "containerd-shim-runc-v2",
                    "-namespace",
                    "moby",
                    "-id",
                    bad
                ])),
                None,
                "{bad} should not parse as a container id"
            );
        }
    }

    #[test]
    fn empty_cmdline_is_not_a_shim() {
        assert_eq!(parse_shim_cmdline(&[]), None);
    }

    #[test]
    fn reads_start_time_past_a_name_containing_spaces_and_parens() {
        // field:      1   2                3 4 5 6 7  8 9 ...                22
        let stat = "42 (odd (name) here) S 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 8675309 0 0";
        assert_eq!(process_start_ticks(stat), Some(8675309));
    }

    #[test]
    fn malformed_stat_yields_no_start_time() {
        assert_eq!(process_start_ticks("no parens here"), None);
        assert_eq!(process_start_ticks("42 (sh) S 1"), None);
    }

    /// Builds a fake /proc/<pid> entry with the given argv and start time.
    fn shim_argv(container_id: &str) -> [&str; 5] {
        [
            "containerd-shim-runc-v2",
            "-namespace",
            "moby",
            "-id",
            container_id,
        ]
    }

    #[test]
    fn exe_must_name_the_shim_when_it_can_be_read() {
        let root = fixture_root("exe");
        let dir = root.join("1");
        fs::create_dir_all(&dir).unwrap();

        // No exe link at all: fail open, so an unreadable link never spares a real orphan.
        assert!(exe_names_the_shim(&dir));

        let link = dir.join("exe");
        std::os::unix::fs::symlink("/usr/bin/containerd-shim-runc-v2", &link).unwrap();
        assert!(exe_names_the_shim(&dir));

        // A process wearing a shim's argv: the exe link is the kernel's, and gives it away.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/bin/sh", &link).unwrap();
        assert!(!exe_names_the_shim(&dir));

        // containerd upgraded under a still-running shim.
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/usr/bin/containerd-shim-runc-v2 (deleted)", &link).unwrap();
        assert!(exe_names_the_shim(&dir));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_positive_pid_directories_are_ignored() {
        let root = fixture_root("nonpositive");
        // kill() reads 0 as the caller's process group and -1 as everything it may signal.
        write_proc(&root, 0, &shim_argv(CID_A), 1000);
        write_proc(&root, -1, &shim_argv(CID_B), 1000);
        write_proc(&root, 42, &shim_argv(CID_C), 1000);

        let found = list_shims(&root, Duration::from_secs(1000), 100).unwrap();
        let pids: Vec<i32> = found.iter().map(|shim| shim.pid).collect();
        assert_eq!(pids, vec![42]);

        fs::remove_dir_all(&root).ok();
    }

    fn write_proc(root: &Path, pid: i32, args: &[&str], start_ticks: u64) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cmdline"), argv(args)).unwrap();
        // Pad to put start_ticks at field 22, as the kernel does.
        fs::write(
            dir.join("stat"),
            format!("{pid} (proc) S 1 1 0 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 0 0"),
        )
        .unwrap();
    }

    fn fixture_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("docker-reaper-shims-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn lists_shims_and_computes_age_from_start_time() {
        let root = fixture_root("list");
        // 100 ticks/sec, uptime 1000s. A shim started at tick 40_000 is 400s in, so 600s old.
        write_proc(
            &root,
            101,
            &[
                "containerd-shim-runc-v2",
                "-namespace",
                "moby",
                "-id",
                CID_A,
            ],
            40_000,
        );
        write_proc(&root, 202, &["/usr/bin/dockerd"], 1_000);
        write_proc(
            &root,
            303,
            &[
                "containerd-shim-runc-v2",
                "-namespace",
                "k8s.io",
                "-id",
                CID_B,
            ],
            90_000,
        );
        fs::create_dir_all(root.join("not-a-pid")).unwrap();

        let mut found = list_shims(&root, Duration::from_secs(1000), 100).unwrap();
        found.sort_by_key(|s| s.pid);

        assert_eq!(
            found.len(),
            2,
            "dockerd and non-pid entries must be ignored"
        );
        assert_eq!(found[0].pid, 101);
        assert_eq!(found[0].namespace, "moby");
        assert_eq!(found[0].container_id, CID_A);
        assert_eq!(found[0].age, Duration::from_secs(600));
        assert_eq!(
            found[1].namespace, "k8s.io",
            "other namespaces are listed, and filtered later"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unreadable_start_time_yields_zero_age_so_the_shim_is_spared() {
        let root = fixture_root("noage");
        let dir = root.join("404");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("cmdline"),
            argv(&[
                "containerd-shim-runc-v2",
                "-namespace",
                "moby",
                "-id",
                CID_C,
            ]),
        )
        .unwrap();
        // No stat file at all.
        let found = list_shims(&root, Duration::from_secs(1000), 100).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].age, Duration::ZERO);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn still_same_shim_detects_a_reused_pid() {
        let root = fixture_root("reuse");
        write_proc(
            &root,
            777,
            &[
                "containerd-shim-runc-v2",
                "-namespace",
                "moby",
                "-id",
                CID_A,
            ],
            1000,
        );
        let shim = ShimProcess {
            pid: 777,
            namespace: String::from("moby"),
            container_id: String::from(CID_A),
            age: Duration::from_secs(60),
        };
        assert!(still_same_shim(&root, &shim));

        // Same pid, different container: the pid was reused.
        write_proc(
            &root,
            777,
            &[
                "containerd-shim-runc-v2",
                "-namespace",
                "moby",
                "-id",
                CID_B,
            ],
            1000,
        );
        assert!(!still_same_shim(&root, &shim));

        // Same pid, something else entirely.
        write_proc(&root, 777, &["/bin/sh"], 1000);
        assert!(!still_same_shim(&root, &shim));

        // Process gone.
        fs::remove_dir_all(root.join("777")).unwrap();
        assert!(!still_same_shim(&root, &shim));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_uptime() {
        let root = fixture_root("uptime");
        fs::write(
            root.join("uptime"),
            "12345.67 98765.43
",
        )
        .unwrap();
        assert_eq!(uptime(&root).unwrap().as_secs(), 12345);
        fs::remove_dir_all(&root).ok();
    }
}
