# docker-reaper

Automatically remove Docker resources (containers, networks, or volumes) older than a certain duration.

In situations where containers and other resources are spawned on-demand by users (such as CTF challenge servers), it is often desirable to enforce a maximum lifespan for containers.

However, the Docker Engine API does not provide a simple way to perform actions like "remove all containers which are more than 30 minutes old." Instead, it is necessary to inspect the creation time of each container (or other resource) and determine whether to remove each one individually. `docker-reaper` automates this process, along with some additional helpful functionality.

## Sample Usage

```bash
# Remove all containers older than 30m
$ docker-reaper containers --min-age 30m

# Remove all networks with a certain label created within the last 3 days
$ docker-reaper networks --filter label=<value> --max-age 72h

# Check which volumes would be removed (non-destructive)
$ docker-reaper volumes --min-age 10m --dry-run
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

## Library and Semantic Versioning

While the application logic is implemented as a library, the binary is intended as the primary interface for clients. Semantic versioning will apply to the binary, not the library. If you depend on this crate as a library, pin a specific version in your `Cargo.toml`.
