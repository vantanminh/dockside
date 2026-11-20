# Dockside

[![CI](https://github.com/vantanminh/dockside/actions/workflows/ci.yml/badge.svg)](https://github.com/vantanminh/dockside/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Server-centric version control for AI coding agents.** Agents open a throwaway workspace, edit,
and *submit*. A single-writer merge queue on the server merges, runs your checks, and lands the
change on a linear `main` (`r1 → r2 → r3 …`), then can release it and roll it back if the release
is unhealthy. There are no branches, no worktrees, and no human approval step.

> **Status: early (0.1).** It runs in production for one team, but the API and config format may
> still change. Expect rough edges and read the [limits](#limits) before relying on it.

## Why not just Git?

Git is built for people who sync full histories and resolve conflicts by hand. Many agents working
on one monorepo hit the same problems over and over: stale branches, merge storms, forgotten
worktrees, two agents numbering the same migration. Dockside moves that coordination to a server
that owns `main`:

| | Git + a forge | Dockside |
| --- | --- | --- |
| Source of truth | Every clone | The server |
| History | Branch graph | One linear line of revisions |
| Work unit | Branch / PR | Ephemeral workspace → *changeset* |
| Merging | PR + review + merge button | Single-writer queue, server-side 3-way merge |
| Checks | CI after push | Verify gate **before** landing, on the merged result |
| Release | Separate pipeline | `release` / `rollback` with a runtime health check |

It is deliberately **not** a Git replacement for people: no forks, PR comments, issues, per-user
permissions, or offline clones. A one-way Git mirror keeps the history readable by normal tools.

## How it works

1. `dockside open` creates a workspace and registers a *path lease* so other agents are warned
   before touching the same area.
2. The agent edits files, uses `snap` / `undo` for checkpoints, then `dockside submit`.
3. The server merges against the current `main`. Conflicts are returned to the agent to resolve and
   resubmit. Nothing waits for a person.
4. The **verify gate** runs your configured commands on the merged tree. Paths matching
   `sensitive_globs` (migrations, auth, infra, API contracts…) *must* have the gate actually run;
   a skipped check is never treated as a pass.
5. On success the changeset lands as the next revision. SQL migrations written as
   `pending_<slug>.sql` are numbered atomically (`NNNN_<slug>.sql`) at land time.
6. `dockside release --execute` deploys a target, checks its `ready_url`, and automatically rolls
   back to the last healthy deployment if the check fails. Without `--execute` it is a dry run.

Blobs are content-addressed (BLAKE3 + zstd) in a Merkle tree. Metadata lives in SQLite (WAL);
blobs can be written through to Cloudflare R2 or any S3-compatible store. Secret-looking files
(`.env`, `*.pem`, `*.key`, …) are rejected by both CLI and server.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for details and the HTTP API.

## Quick start

Requires a recent stable Rust toolchain (see `rust-version` in `Cargo.toml`).

```sh
cp dockside.example.toml dockside.toml          # edit the targets for your repo
cargo run -p dockside-server --bin dockside-server   # http://127.0.0.1:7420, UI at /

# in another shell
export DOCKSIDE_URL=http://127.0.0.1:7420
cargo run -p dockside-cli --bin dockside -- health
```

Import an existing repository as the baseline (read-only on the source directory), then work:

```sh
dockside import --map services/api=/path/to/api --map apps/web=/path/to/web
dockside open --title "Fix pagination" --scope services/api --author agent-1 --json
# ... edit files in the printed workspace directory ...
dockside status --dir <workspace> --json
dockside submit --dir <workspace> --json
```

Every command accepts `--json`. An agent skill that explains this loop is in
[`skills/dockside/SKILL.md`](skills/dockside/SKILL.md).

## Configuration

`dockside.toml` (see [`dockside.example.toml`](dockside.example.toml)) declares policy, hot files,
and *targets*: a path prefix, how to verify it (`verify_commands`), and how to release it
(`rudder-image`, `cloudflare-worker`, or `expo-eas`). Environment variables are listed in
[`.env.example`](.env.example). Key ones:

| Variable | Meaning |
| --- | --- |
| `DOCKSIDE_BIND_ADDR` | Listen address. Non-loopback **requires** `DOCKSIDE_AUTH_TOKEN` |
| `DOCKSIDE_AUTH_TOKEN` | Bearer token for `/v1/*` |
| `DOCKSIDE_RUN_VERIFY` | `true` to really run verify gates (needed for sensitive paths to land) |
| `DOCKSIDE_R2_*` | Optional S3-compatible blob store |
| `DOCKSIDE_GIT_MIRROR_DIR` | Optional one-way Git mirror |

## Running it in a container

```sh
docker build -t dockside-server .
DOCKSIDE_AUTH_TOKEN=$(openssl rand -hex 32) \
  docker compose -f deploy/docker/compose.yml up -d
```

The image is small; Rust, Node/pnpm (and optionally the Rudder CLI) used by verify gates are
installed into the `/data` volume on first start. Everything durable lives in that one volume:
**back it up** (`dockside.sqlite` is the revision history). Put a TLS-terminating proxy in front
before exposing the server beyond localhost.

## Limits

- Single server, single SQLite writer; no HA. Losing `dockside.sqlite` loses history (blobs in
  object storage survive but cannot be mapped back to revisions).
- One shared bearer token; no per-user identity or permissions.
- The merge queue serialises landings; throughput is bounded by your verify gate.
- Release orchestration for `rudder-image` targets needs a Docker-capable environment.
- Only Linux containers are exercised in CI for the server image; the CLI and server are tested on
  Linux and Windows.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Workspace: `dockside-core` (CAS, trees, merge, policy), `dockside-server` (HTTP API, queue,
verify, mirror, release), `dockside-cli` (the `dockside` binary). See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at
your option.
