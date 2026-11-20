# Contributing

Thanks for helping! Dockside is small and opinionated, so please open an issue to discuss larger
changes before sending a pull request.

## Setup

You need a recent stable Rust toolchain (the minimum supported version is `rust-version` in
`Cargo.toml`) and `git`. No database or external service is needed to build and test.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

CI runs exactly these on Linux and Windows, plus a build of the Docker image and a
minimum-supported-Rust-version build. A pull request needs a green CI.

## Guidelines

- No `unwrap` / `expect` / `dbg!` / `todo!` in non-test code; the lints enforce this.
- Every behaviour change needs a test. Server behaviour is covered by integration tests in
  `crates/dockside-server/tests`, CLI behaviour by `crates/dockside-cli/tests`.
- Never weaken the secret guard or the "skipped is not passed" rule of the verify gate.
- Keep commits focused, with a short imperative subject (`fix: …`, `feat: …`, `docs: …`).
- Do not commit secrets, `.env` files or local data directories.

By contributing you agree that your contribution is licensed under the project's
`MIT OR Apache-2.0` license.
