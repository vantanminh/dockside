# Architecture

Dockside replaces a distributed, branch-per-agent Git workflow with a **server-centric monorepo**:
the server owns `main`; agents only propose changesets.

## 1. Design goals

1. **A linear `main` (`r0 → r1 → r2 …`).** No free-form branches, no worktrees, no agent-run
   `git merge` / `git rebase`.
2. **Single-writer merge queue.** Only the server, under a sequential `merge_lock` plus a
   compare-and-set on SQLite, may advance `main`'s `rev_id`.
3. **Avoid conflicts before they happen.**
   - *Path leases:* `open --scope …` registers a lease (soft, to warn early; or `--exclusive`).
   - *Stacked changesets* (`--stack CS-xxxx`): when two agents need the same area, the second
     stacks on the first. The integrator lands the child only after the parent, using an
     `effective_base` so no false conflicts appear.
   - *Hot-file neutralizers:* SQL migrations are written as `pending_<slug>.sql` and numbered
     atomically as `NNNN_<slug>.sql` at land time; applied migrations are immutable. Registry files
     (for example a routes list) support a pure-addition sorted auto-merge when two agents add
     different entries at the same spot.
   - *Atomic sequence allocator* (`/v1/sequences/{kind}/next`): collision-free `IN-###`, `US-###`
     style IDs.
4. **Fully automated landing, risk-based checks.**
   - Every changeset: 3-way merge → verify gate → land on `main`. Nothing waits for a human.
   - Paths matching `sensitive_globs` are flagged (`sensitive_paths`) and require the verify gate
     to actually run when an affected target has `verify_commands`; if it cannot run, the
     changeset is `failed` and does not land. (`allow_unverified_sensitive` exists for
     development and tests only.)
   - `VerifyReport` distinguishes `skipped` (did not run, with `skip_reason`) from `passed` (ran
     and succeeded). A skipped check is never a pass.
   - The warm verify directory is synced to the exact revision, including removal of deleted
     files (tracked by a manifest; build outputs such as `target/` and `node_modules/` are left
     alone).
   - An internal error fails only that changeset. After a restart, changesets stuck in `merging`
     or `verifying` are re-queued and drained.
5. **Secrets stay out.** `.env`, `.env.*`, `.dev.vars`, `*.pem`, `*.key` and similar are blocked in
   both the CLI and the server; only template files such as `.env.example` are allowed.

## 2. Storage (CAS + Merkle tree)

- **Blob hash:** BLAKE3 (64 lowercase hex) of the raw content; stored zstd-compressed (level 3),
  sharded as `<data_dir>/cas/<xx>/<hash>.zst`.
- **Object store (L2):** with `DOCKSIDE_R2_*` set, the CAS writes through to Cloudflare R2 (or any
  S3-compatible store, using a small hand-written SigV4 client) and reads through when the local
  cache misses.
- **Merkle tree:** each directory is a `TreeNode { dirs, files }` stored in the CAS, so unchanged
  directories keep their hash across revisions and trees can be loaded sparsely by `--scope`.
- **One-way Git mirror:** with `DOCKSIDE_GIT_MIRROR_DIR` set, every revision landed on `main` is
  mirrored as one clean commit in a backup Git repository (never read back).
- **Metadata:** revisions, changesets, leases, deployments and sequences live in SQLite (WAL).

## 3. HTTP API (`/v1/*`)

Every endpoint except `GET /` (the web UI) and `GET /v1/health` requires
`Authorization: Bearer <DOCKSIDE_AUTH_TOKEN>` when auth is enabled (mandatory for a non-loopback
bind).

| Method and path | Purpose |
| :--- | :--- |
| `GET /` | Embedded review / merge-queue web UI |
| `GET /v1/health` | Server status, `version` (server crate package version), `main_rev`, object store, mirror, verify flags |
| `POST /v1/blobs/missing` | Which of these hashes does the server lack (delta upload) |
| `PUT /v1/blobs/{hash}` | Upload a blob (server re-checks the BLAKE3) |
| `GET /v1/blobs/{hash}` | Download a blob |
| `GET /v1/revisions` | Linear history of `main` |
| `GET /v1/revisions/{rev_id}/tree?scope=…` | Tree manifest (sparse by scope) |
| `POST /v1/import` | Import an initial snapshot |
| `POST /v1/changesets` | Open a changeset and register leases |
| `GET /v1/changesets/{cs_id}` | Changeset, conflicts, verify report, sensitive paths |
| `GET /v1/changesets/{cs_id}/diff` | Unified diff against `base_rev` |
| `POST /v1/changesets/{cs_id}/snapshots` | Save a checkpoint for instant undo |
| `POST /v1/changesets/{cs_id}/submit` | Enqueue (default `?wait=true`) |
| `POST /v1/changesets/{cs_id}/abandon` | Abandon and release leases |
| `GET /v1/leases` | Active path leases |
| `POST /v1/sequences/{kind}/next` | Allocate the next sequence number |
| `GET /v1/deployments` | Deployment history per target and environment |
| `POST /v1/releases` | Plan or execute a release; after `execute`, checks `ready_url` and rolls back automatically if unhealthy |
| `POST /v1/rollbacks` | Plan or execute a rollback to an earlier revision |

## 4. Release and rollback

Targets (`[[targets]]` in `dockside.toml`) have a `kind`: `rudder-image`, `cloudflare-worker` or
`expo-eas`. Without `--execute` a release only exports a clean snapshot and prints the commands it
would run. With `--execute`:

- A release is recorded `deployed` only after the target's `ready_url` returns 2xx (and, for
  `rudder-image`, the body carries the expected `rN` build marker). Without a `ready_url` it is
  `deployed_unverified`.
- A failed runtime check records `deploy_unhealthy` and rolls back to the last healthy deployment.
- Cloudflare and Expo commands run in the target's own subdirectory of the exported snapshot.

## 5. Large imports

A proxy such as Cloudflare cuts a request after about 100 seconds. The CLI therefore checks blob
existence in batches, uploads blobs concurrently, and, if `/v1/import` is cut by a proxy
(HTTP 502/504/522/524), polls the revision list for the new revision. The server runs the import
detached from the connection, so it completes even if the client disconnects.

## 6. Extending

- Add a target with a `[[targets]]` block and import its directory with
  `dockside import --map <prefix>=<dir>`.
- A new release kind means a new `TargetKind` plus its command builders in
  `dockside-server/src/release.rs`.
