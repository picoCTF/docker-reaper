# Changelog

## v1.3.0

- Added a `shims` subcommand that reaps orphaned `containerd-shim-runc-v2` processes: shims whose container no longer exists, which cannot be reached through the Engine API (the daemon returns 404) and which each hold roughly 5 MiB until the host reboots. A shim is only signalled once it is in the same PID namespace as `docker-reaper` (so it is a host process, not something inside a container presenting itself as one) with an id of the right shape, its container id is absent from the full container list, it has been running for at least `--min-age`, and the latter two still hold after `--settle` with its `/proc/<pid>/cmdline` still naming the same container. `/proc/<pid>/exe` must also name `containerd-shim-runc-v2`; that link is the kernel's rather than the target's, so a process cannot qualify on argv alone. No shim of a container known to the daemon being swept is selected. Removal is `SIGTERM` to the batch, one shared `--grace` window, then `SIGKILL` to whatever remains, with the argv and PID-namespace checks repeated immediately before the escalation so a pid reused during the grace window is not signalled.
- Because it reads the local process table, `shims` only works against a local daemon and exits with an error if `DOCKER_HOST` or `DOCKER_CERT_PATH` is set. It also needs permission to signal the shims, which in practice means running as root.
- Outside `--dry-run`, `shims` refuses to run unless `--proc-root` names this process's own procfs. Every selection check resolves under that path while the signal goes to the real process table, so a directory of fixture data could otherwise nominate arbitrary pids for `SIGKILL`.
- The container-absence check is only meaningful for the containerd namespace the connected daemon owns, so a non-default `--namespace` is warned about.

## v1.2.1

- The `aarch64-unknown-linux-gnu` release tarball now contains an arm64 binary, built natively on an `ubuntu-24.04-arm` runner. In all previous releases (v1.0.0 through v1.2.0) it contained the same x86-64 binary as the `x86_64-unknown-linux-gnu` tarball.
- Linux release binaries are built on Ubuntu 24.04 runners again (v1.1.0 through v1.2.0 were built on Ubuntu 22.04). This does not change the minimum supported glibc version, which remains 2.34 (Ubuntu 22.04, Debian 12, RHEL 9, Amazon Linux 2023, or newer).
- CI and release workflows use `actions-rust-lang/setup-rust-toolchain` instead of `dtolnay/rust-toolchain`.
- Updated dependencies.

## v1.1.1
- Convert from bin+lib crate to standard bin crate
- Update dependencies

## v1.1.0

- Linux binaries are now built on Ubuntu 22.04 runners (rather than 24.04) for compability with a wider range of glibc versions.

## v1.0.0

- Added integration tests.
- Updated dependencies.

## v0.1.3

- Updated dependencies.

## v0.1.2

- Updated dependencies.

## v0.1.1

- Fixed volume removal.
- Updated dependencies.

## v0.1.0

Initial release. Supports removal of matching containers, networks, and volumes based on creation time and Docker filter syntax.
