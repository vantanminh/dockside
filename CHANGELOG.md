# Changelog

All notable changes to Dockside are documented here, following
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and semantic versioning.

## [Unreleased]

## [0.2.2] - 2026-11-22

### Added
- `dockside-server checkpoint` flushes the existing SQLite WAL and fails if it cannot obtain a consistent checkpoint. Rudder can run it before stopping Dockside and archiving `/data`; a checkpoint alone is not a live filesystem snapshot.

## [0.2.1] - 2026-11-22

### Fixed
- Server logs no longer contain ANSI color codes when stdout is not a terminal (e.g. in Docker or the Rudder log viewer); `NO_COLOR` also disables colors.
- The container no longer prints a tini warning when it is not PID 1 (for example under `init: true`): tini now runs as a child subreaper.

## [0.2.0] - 2026-11-22

### Added

- Published multi-architecture container images on GHCR and prebuilt CLI binaries on GitHub Releases.
- Server version in `/v1/health` and the CLI health output.
- Installable as a Rudder plugin using the release's digest-pinned `rudder-plugin.json` descriptor.

## [0.1.0] - 2026-11-20

### Added

- Initial release: server-centric version control, CLI workspaces, automated verification,
  release health checks and rollback, content-addressed storage, and Git mirroring.
