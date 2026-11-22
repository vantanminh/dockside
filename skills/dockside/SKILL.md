---
name: dockside
description: Use the dockside CLI for server-centric source control (opening ephemeral workspaces, checkpoints/undo, submitting changesets into the single-writer merge queue, resolving conflicts, and planning Rudder/Worker releases). Never run git merge, git rebase, or git worktree in a dockside-managed workspace.
---

# Dockside Agent Workflow

For volume backups, run `dockside-server checkpoint` with the server's
`DOCKSIDE_DATA_DIR`, require success, stop the service, and archive the whole data
volume. A checkpoint alone is not a live snapshot. Restore only while stopped,
then start and verify `/v1/health`. The maintenance command is local, not HTTP.

You are working in a **server-centric monorepo** managed by `dockside-server` + `dockside`.
Do **not** create Git branches, worktrees, or run `git merge` / `git rebase`. The server owns `main` (`r1 → r2 → r3...`).

## 1. Standard 4-Step Loop (always prefer `--json`)

1. **Open an ephemeral workspace** (declaring your path scope to register a soft lease):
   ```bash
   dockside open --title "<short task summary>" --scope services/api,apps/web --story US-123 --author <agent-name> --json
   ```
   - If `lease_warnings` reports another active agent editing the exact same module, coordinate or stack on top of it:
     ```bash
     dockside open --title "<follow-up>" --scope apps/web --stack CS-0001 --author <agent-name> --json
     ```
2. **Checkpoint before risky edits** (safe instant undo without polluting `main`):
   ```bash
   dockside snap -m "before refactor" --dir <workspace_dir> --json
   dockside undo --dir <workspace_dir> --json
   ```
3. **Inspect changes** before submitting:
   ```bash
   dockside status --dir <workspace_dir> --json
   dockside diff --dir <workspace_dir>
   ```
4. **Submit to the Single-Writer Merge Queue** (do NOT merge locally):
   ```bash
   dockside submit --dir <workspace_dir> --json
   ```

## 2. Interpreting `dockside submit` Status

- **`landed`**: Your changeset auto-merged, passed verify gates, and landed on `main` as `r<N>`. Task complete!
- **Sensitive paths** (`migrations/`, `infra/`, `auth/`, `authz/`, `contracts/`, `openapi*.yaml`, `dockside.toml`) are flagged in `sensitive_paths` but need **no approval**: they land like any other change once automated checks pass. If the verify gate cannot run for them the changeset is `failed` (see `error_message`); fix the cause and resubmit. Never ask Minh to approve anything.
- **`conflict`**: Main diverged on overlapping lines (other agents are NOT blocked). Materialize a clean resolution workspace and re-submit:
  ```bash
  dockside conflicts <cs_id> --materialize <resolve_dir> --json
  # Inspect .dockside-conflicts/*.base|.ours|.theirs, resolve markers in <resolve_dir>
  dockside submit --dir <resolve_dir> --json
  ```
- **`failed`**: Verify gate failed (or could not run). Inspect `verify_report.steps` / `skip_reason` / `error_message`, fix in your workspace, and run `dockside submit` again. A report with `skipped: true` means checks did NOT run; it is not a pass.

## 3. Hot-File & Security Rules

- **SQL Migrations:** Never pick a 4-digit number (`0087_...`) manually! Create `services/api/src/db/migrations/pending_<slug>.sql`. `dockside-server` atomically assigns `0087_<slug>.sql` when landing on `main`. Never edit existing `NNNN_*.sql` files.
- **Harness / Story IDs:** Allocate monotonic IDs from the server to prevent collisions:
  ```bash
  dockside seq intake --json   # -> IN-001, IN-002...
  dockside seq story --json    # -> US-001, US-002...
  ```
- **Secrets:** Never place `.env`, `.dev.vars`, `*.pem`, or `*.key` in the workspace (`dockside` hard-blocks them).
- **Releases:** You decide when to release. Pass `--execute` to `dockside release` / `dockside rollback` when a release is warranted (without it you only get a dry-run plan). The server verifies `ready_url` after deploy and auto-rolls back on failure; check the returned `status`/`health_verified`, and report the real outcome to Minh afterwards (he reviews results, he does not approve steps).
