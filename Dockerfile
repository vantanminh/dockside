# syntax=docker/dockerfile:1.7
# dockside-server image.
#
# The image is deliberately small: it holds the server, git, curl and the C toolchain + OpenSSL
# headers that `cargo` needs to link. The heavy developer tools used by verify gates and releases
# (Rust, Node/pnpm, optionally the Rudder CLI) are NOT baked in: deploy/docker/bootstrap-tools.sh
# installs pinned versions into the persistent /data volume on first start (idempotent; re-runs
# only when a version changes). Until it finishes, verify commands fail (they are never "skipped").
ARG RUST_VERSION=1.97

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p dockside-server --bin dockside-server \
    && strip target/release/dockside-server \
    && cp target/release/dockside-server /dockside-server

FROM debian:bookworm-slim AS runtime
ARG GIT_SHA=unknown
LABEL org.opencontainers.image.title="dockside-server" org.opencontainers.image.revision="${GIT_SHA}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
         ca-certificates curl git xz-utils tini gcc libc6-dev pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/* /usr/share/doc /usr/share/man /usr/share/locale /var/cache/debconf/* \
    && rm -rf /usr/lib/gcc/x86_64-linux-gnu/*/lto1 \
         /usr/bin/x86_64-linux-gnu-lto-dump-* /usr/bin/x86_64-linux-gnu-gprof* /usr/bin/x86_64-linux-gnu-ld.gold \
         /usr/lib/x86_64-linux-gnu/lib*san.so* /usr/lib/x86_64-linux-gnu/lib*san.a /usr/lib/x86_64-linux-gnu/libgprofng* \
         /usr/lib/gcc/x86_64-linux-gnu/*/lib*san.* /usr/lib/x86_64-linux-gnu/gconv \
         /usr/lib/x86_64-linux-gnu/perl /usr/share/perl /usr/lib/git-core/git-svn /usr/lib/git-core/git-p4 \
         /usr/lib/git-core/git-instaweb
# The rm above trims what neither cargo's link step nor git needs, keeping the image small.

# Unprivileged user; /data is the only writable persistent location.
RUN useradd --uid 10001 --create-home --home-dir /home/dockside --shell /bin/sh dockside \
    && mkdir -p /data /etc/dockside \
    && chown -R dockside:dockside /data

COPY --from=build /dockside-server /usr/local/bin/dockside-server
COPY dockside.example.toml /etc/dockside/dockside.toml
COPY deploy/docker/entrypoint.sh /usr/local/bin/dockside-entrypoint
COPY deploy/docker/bootstrap-tools.sh /usr/local/bin/dockside-bootstrap-tools
RUN chmod 0755 /usr/local/bin/dockside-entrypoint /usr/local/bin/dockside-bootstrap-tools

ENV TZ=UTC \
    DOCKSIDE_BIND_ADDR=0.0.0.0:7420 \
    DOCKSIDE_DATA_DIR=/data \
    DOCKSIDE_CONFIG=/etc/dockside/dockside.toml \
    DOCKSIDE_GIT_MIRROR_DIR=/data/git-mirror \
    DOCKSIDE_VERIFY_WORKDIR=/data/verify-work \
    DOCKSIDE_RUN_VERIFY=true \
    DOCKSIDE_TOOLS_DIR=/data/toolchain \
    RUSTUP_HOME=/data/toolchain/rustup \
    CARGO_HOME=/data/toolchain/cargo \
    COREPACK_HOME=/data/cache/corepack \
    npm_config_store_dir=/data/cache/pnpm-store \
    PATH=/data/toolchain/bin:/data/toolchain/cargo/bin:/data/toolchain/node/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    CI=true

USER dockside
VOLUME ["/data"]
EXPOSE 7420
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:7420/v1/health || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/dockside-entrypoint"]
