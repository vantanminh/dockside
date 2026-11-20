# AGENTS.md

Guidance for AI coding agents (and humans) working on this repository.

- Language: Rust (edition 2024), a three-crate workspace: `dockside-core`, `dockside-server`,
  `dockside-cli`.
- Required lints: `unsafe_code = "forbid"`, `clippy::unwrap_used = "deny"`,
  `clippy::expect_used = "deny"`, `dbg_macro`/`todo = "deny"`. Handle errors; do not unwrap.
- Before reporting work done, run:
  ```sh
  cargo fmt --all
  cargo clippy --workspace --all-targets --locked -- -D warnings
  cargo test --workspace --locked
  ```
- **Secrets.** Never bypass `PolicyEngine::ensure_no_secrets` in either the CLI or the server. Never
  read, print or commit `.env` files or tokens. The server must refuse a non-loopback bind without
  `DOCKSIDE_AUTH_TOKEN`.
- **Automation is a design principle.** The tool has no human approval gate (no approve/reject
  state). Do not add one. Safety comes from automated checks instead: the secret guard, a verify
  gate that must really run for sensitive paths, and a runtime health check with automatic rollback
  on release.
- `release` / `rollback` without `--execute` is a dry run (protection against typos, not an
  approval step).
- **Honest reporting.** A verify step that did not run is `skipped`, never `passed`.
- Keep the docs ([README](README.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), the agent skill
  in `skills/dockside/SKILL.md`) in step with behaviour changes.
