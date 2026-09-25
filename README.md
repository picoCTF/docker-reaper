# docker-reaper

Automatically remove Docker resources (containers, networks, or volumes) older than a certain duration, or evict unused images when disk usage exceeds a threshold.

In situations where containers and other resources are spawned on-demand by users (such as CTF challenge servers), it is often desirable to enforce a maximum lifespan for containers or prevent disk exhaustion from accumulated container images.

However, the Docker Engine API does not provide a simple way to perform actions like "remove all containers which are more than 30 minutes old" or automatically evict unused images when disk space runs low. Instead, it is necessary to inspect the creation time of each container (or other resource) or monitor disk usage and determine whether to remove each resource individually. `docker-reaper` automates this process, along with some additional helpful functionality.

## Sample Usage

```bash
# Remove all containers older than 30m
$ docker-reaper containers --min-age 30m

# Remove all networks with a certain label created within the last 3 days
$ docker-reaper networks --filter label=<value> --max-age 72h

# Check which volumes would be removed (non-destructive)
$ docker-reaper volumes --min-age 10m --dry-run

# Evict unused images (largest reclaimable first) when disk usage exceeds 80% until reaching 70%
$ docker-reaper images --threshold 80 --target 70

# The same, least recently used first, by the record the container sweep keeps
$ docker-reaper containers --min-age 30m --record-image-use /var/lib/docker-reaper/image-use
$ docker-reaper images --threshold 80 --target 70 --lru /var/lib/docker-reaper/image-use

# Sweep containerd shims left behind by containers that no longer exist
$ docker-reaper shims --min-age 10m
```

Run `docker-reaper --help` for a full list of available options.

## Installation

Prebuilt binaries for certain targets are available as GitHub release artifacts. For all other platforms, install from source using `cargo`:

```bash
$ cargo install --locked .
```

## Notes

- `docker-reaper` forcibly removes containers by sending `SIGKILL` (equivalent to `docker rm -f`).
- Connection to the Docker daemon is negotiated automatically based on the presence of environment variables `DOCKER_HOST` and `DOCKER_CERT_PATH` (for TLS connections), falling back to a local socket if neither are set.
- While `docker-reaper` will bail out entirely if an unrecoverable error occurs (such as being unable to contact the Docker daemon), in general it will proceed even when removal of a specific resource fails. A report at the end of the run indicates whether each eligible resource was successfully removed (or the error encountered during removal).
- Logging is configurable via the standard `RUST_LOG` environment variable.

## Additional Options

### Remove container-associated networks

When removing containers, you can also attempt to remove all networks which were associated with those containers. This can be useful if, for example, you are associating a custom bridge network with each container:

```bash
$ docker network create my-network
$ docker run -i -t --detach --net my-network --name my-container ubuntu bash

# Will remove both `my-container` and `my-network`
$ docker-reaper containers --filter name=my-container --reap-networks
```

Network removal is attempted only after attempting to remove all matching containers to avoid active endpoint errors.

### Run repeatedly

By default, `docker-reaper` will run once and exit. To run repeately, we recommend using a scheduling tool such as `systemd` or `cron`. However, in a pinch, you can also use the `--every` option. For example:

```bash
$ docker-reaper containers --min-age 15m --every 1m
```

will repeatedly remove containers more than 15 minutes old, waiting 1 minute between each attempt.

### Disk-pressure image eviction

The `images` subcommand monitors filesystem usage and automatically evicts unused Docker images (images not referenced by any container, running or stopped) when disk usage exceeds a configured threshold:

```bash
$ docker-reaper images --threshold 80 --target 70
```

Key flags for `docker-reaper images`:
- `--threshold <percent>`: Only reap when disk usage is at or above this percentage (default: `80`).
- `--target <percent>`: Remove unused images until disk usage falls below this percentage (default: `70`).
- `--disk-path <path>`: Filesystem path to measure disk usage on. Defaults to the Docker daemon's root directory (`docker_root_dir`). Note: when targeting a remote daemon via `DOCKER_HOST`, `--disk-path` must be explicitly specified because disk measurement operates on local storage.
- `-f, --filter <name=value>`: Only reap images matching Docker Engine-supported filters (can be specified multiple times).
- `--lru <path>`: Evict least recently used first, by the record at this path (see below).

Images are selected and evicted largest-unique-size first (reclaimable bytes not shared with other images) until disk usage drops below the target percentage. Non-forced removals skip images that gain containers mid-run.

#### Least recently used first

Docker records nothing about when an image was last used: the Engine API has no such field,
and a pull does not set `LastTagTime`. So `--lru` reads a record kept by the container
sweep instead:

```bash
$ docker-reaper containers --min-age 30m --record-image-use /var/lib/docker-reaper/image-use
$ docker-reaper images --threshold 80 --target 70 --lru /var/lib/docker-reaper/image-use
```

- `containers --record-image-use <path>` stamps the image of every container the sweep
  matches, old enough to reap or not, as in use now. It uses the container list the sweep
  fetches anyway, so it adds no call to the daemon; the one exception is the run that
  creates the record, which lists images once and stamps every image already on the host
  at 0, older than anything seen since. It starts the record only once it has checked it
  could save it, so a record that cannot be written never costs that list on every run.
  Nothing is written in a dry run, and a record that cannot be written never stops the sweep
  reaping containers.
- `images --lru <path>` evicts in order of those stamps, oldest first, and breaks ties by
  largest reclaimable size, as the default order does. An image the record has never seen
  arrived after the record started — most likely pulled for a launch whose container does
  not exist yet — so it counts as used just now. A record it cannot read is left as it is,
  and that pass orders by size alone.

Repeating `--record-image-use`, `--lru` or `--data-root` keeps the last value, so a
deployment can append them to a command that may already carry them.

Sampling is as frequent as the container sweep runs, so a container that comes and goes
between two runs is not seen. The record is a text file, `<unix seconds> <image id>` per
line; deleting it starts it over.

One case it cannot see: an image removed by something other than this sweep, then pulled
again before the next eviction pass, keeps the stamp it had. Noticing the removal in between
would take an image list every run. After removing images by hand, delete the record.

### Orphaned containerd shim sweep

Each running container has a `containerd-shim-runc-v2` process. If a container is removed
but its shim is not reaped — for example when a runtime call is cancelled partway through
a `runc delete` — the shim keeps running, holding roughly 5 MiB, until the host reboots.
On a small host these accumulate into memory pressure and eventually host-wide OOM kills.

Such a shim cannot be reached through the Engine API: its container is already gone from
the daemon, so `remove_container` returns 404. The `shims` subcommand finds them by
reading the local process table instead:

```shell
# Report orphaned shims without touching them
$ docker-reaper shims --dry-run

# Sweep shims orphaned for at least 10 minutes
$ docker-reaper shims --min-age 10m
```

Key flags for `docker-reaper shims`:

- `--min-age <duration>`: Only reap shims running at least this long (default: `5m`).
- `--settle <duration>`: Wait this long, then re-confirm each candidate before signalling
  it (default: `10s`).
- `--grace <duration>`: How long a shim gets to exit after `SIGTERM` before it is sent
  `SIGKILL` (default: `10s`).
- `--namespace <name>`: containerd namespace to sweep (default: `moby`, which is Docker's).
- `--proc-root <path>` / `--runtime-root <path>`: Override the filesystem locations, mainly
  for testing.
- `--data-root <path>`: dockerd's configured data-root. dockerd keeps a directory there for
  every container it knows, running or stopped, and removes it before dropping the container
  from its list, so a shim whose container still has one is spared without asking the
  daemon. The container list is then only fetched when some shim is left to check, which
  on a healthy host is never. userns-remap's `<uid>.<gid>` root below the data-root is
  searched too.

A shim is only signalled once it has passed four checks:

1. It is in the same PID namespace as `docker-reaper` itself, so it is a host process
   rather than something inside a container. Everything else comes from
   `/proc/<pid>/cmdline`, which is argv the target chose for itself — on a host running
   untrusted containers, any of them can present itself as an orphaned shim. Namespace
   membership cannot be forged from inside a container, and a genuine orphan is a host
   process whatever state containerd left behind, so this never spares a real orphan.
   Its `-id` must also be 64 hex characters, the shape of a Docker container id, and
   `/proc/<pid>/exe` must name `containerd-shim-runc-v2`. That link is the kernel's rather
   than the target's, so it rejects a process wearing a shim's argv; an unreadable link
   keeps the candidate, since sparing a real orphan is the worse failure.
2. Its container id is absent from the full container list, so neither a running nor a
   stopped container claims it. With `--data-root`, a container directory on disk answers
   this without the list.
3. It has been alive for at least `--min-age`, sparing anything mid-creation.
4. Checks 2 and 3 still hold after `--settle`, and its `/proc/<pid>/cmdline` still names
   the same container. The re-read closes the window where a container has just been
   removed but its shim has not yet exited. The argv and PID-namespace checks run once
   more immediately before the SIGKILL escalation, so a pid reused during the grace
   window is not signalled.

Check 2 is only meaningful for the containerd namespace the connected daemon owns, so a
non-default `--namespace` is warned about: the daemon will never report ids from another
namespace, which makes the check vacuous. For the same reason the sweep assumes the
connected daemon is the only client of that namespace — a second daemon, a Docker-in-Docker
inner daemon, or `ctr -n moby` are all invisible to it and visible in `/proc`.

`--dry-run` applies checks 1 to 3 and reports what it would signal; it returns before the
settle window, so it does not reflect check 4.

Because discovery reads this machine's process table, this subcommand only works against
a local daemon and exits with an error if `DOCKER_HOST` or `DOCKER_CERT_PATH` is set. It
also needs permission to signal the shims, which in practice means running as root.

## Library and Semantic Versioning

While the application logic is implemented as a library, the binary is intended as the primary interface for clients. Semantic versioning will apply to the binary, not the library. If you depend on this crate as a library, pin a specific version in your `Cargo.toml`.
