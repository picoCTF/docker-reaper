# Changelog

## v1.4.0

- Added `images --lru <path>`, which evicts unused images least recently used first rather than largest first. Docker records no last-used time for an image (the Engine API has none, and a pull does not set `LastTagTime`), so the order comes from a record kept by the container sweep: `containers --record-image-use <path>` stamps the image of every container the sweep lists as in use now. It reads the container list the sweep fetches anyway, so it adds no call to the daemon; the one exception is the run that creates the record, which lists images once and stamps every image already on the host as older than anything seen since. An image the record has never seen counts as used just now, since it most likely arrived for a launch whose container does not exist yet. Ties are broken by largest reclaimable size, as the default order does.
- The record is a text file, `<unix seconds> <image id>` per line, replaced whole by a rename. A record that cannot be read is left as it is, and that pass orders by size alone; a line in it that cannot be parsed, including one that is not UTF-8, costs only that line; a record that cannot be written never stops the container sweep, and is not started (and its image list not paid for) until it can be saved. An image removed by other means and pulled again before the next eviction pass keeps its old stamp; after removing images by hand, delete the record.
- Added `images --min-size <size>` (e.g. `256MiB`, `1G`; binary units). Images that would free less are evicted only once no larger candidate is left. Deleting an image holds dockerd's image and layer store locks until its files are gone, stalling every container create and image pull on the host, so a removal should free something worth that. With `--lru`, size comes first: a large image used a minute ago goes before a small one unused for weeks. The default, `0`, leaves the order alone.
- Added `shims --data-root <path>`, dockerd's configured data-root. A shim whose container still has a directory there belongs to a container the daemon knows, since dockerd removes that directory before dropping the container from its list, so it is spared without asking the daemon. The container list is then fetched only when some shim is left to check, which on a host with no orphans is never. The `<uid>.<gid>` root userns-remap creates below the data-root is searched too.
- `--record-image-use`, `--lru` and `--data-root` keep their last value when repeated, so a deployment can append them to a command that may already carry them.

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
