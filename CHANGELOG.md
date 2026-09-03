# Changelog

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
