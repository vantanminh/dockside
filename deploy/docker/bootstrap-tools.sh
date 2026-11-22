#!/bin/sh
# Installs the verify/release toolchain into the persistent volume ($DOCKSIDE_TOOLS_DIR, default /data/toolchain).
# Idempotent: a stamp file per tool+version makes re-runs a no-op. Runs in the background from the
# entrypoint, so dockside-server serves immediately; until it finishes, verify commands fail (never "skipped").
set -u
TOOLS="${DOCKSIDE_TOOLS_DIR:-/data/toolchain}"
RUST_VERSION="${DOCKSIDE_RUST_VERSION:-1.97.0}"
NODE_VERSION="${DOCKSIDE_NODE_VERSION:-22.12.0}"
RUDDER_VERSION="${DOCKSIDE_RUDDER_VERSION:-v0.10.1}"
mkdir -p "$TOOLS/bin" "$TOOLS/stamps" /data/cache
log() { echo "bootstrap-tools: $*"; }

stamp() { [ -f "$TOOLS/stamps/$1" ]; }
mark() { : > "$TOOLS/stamps/$1"; }

if ! stamp "rust-$RUST_VERSION"; then
  log "installing Rust $RUST_VERSION"
  if curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal \
       --default-toolchain "$RUST_VERSION" -c clippy -c rustfmt >/dev/null 2>&1; then
    mark "rust-$RUST_VERSION"; log "rust ok"
  else
    log "rust FAILED"
  fi
fi

if ! stamp "node-$NODE_VERSION"; then
  log "installing Node $NODE_VERSION"
  case "$(uname -m)" in
    x86_64) NODE_ARCH=x64 ;;
    aarch64|arm64) NODE_ARCH=arm64 ;;
    *) log "unsupported Node architecture"; exit 1 ;;
  esac
  rm -rf "$TOOLS/node" && mkdir -p "$TOOLS/node"
  if curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
       | tar -xJ -C "$TOOLS/node" --strip-components=1 \
     && "$TOOLS/node/bin/corepack" enable --install-directory "$TOOLS/bin"; then
    mark "node-$NODE_VERSION"; log "node ok"
  else
    log "node FAILED"
  fi
fi

# Rudder CLI is only needed for `rudder-image` release targets; opt in with DOCKSIDE_INSTALL_RUDDER=1.
if [ "${DOCKSIDE_INSTALL_RUDDER:-0}" = "1" ] && ! stamp "rudder-$RUDDER_VERSION"; then
  log "installing rudder $RUDDER_VERSION"
  if curl -fsSL https://github.com/vantanminh/rudder/releases/latest/download/install-cli.sh \
       | sh -s -- --version "$RUDDER_VERSION" --bin-dir "$TOOLS/bin" >/dev/null 2>&1; then
    mark "rudder-$RUDDER_VERSION"; log "rudder ok"
  else
    log "rudder FAILED"
  fi
fi

# Sign the Rudder CLI in (for `dockside release --execute`) once it exists.
if [ -n "${RUDDER_TOKEN:-}" ] && [ -n "${RUDDER_URL:-}" ] && command -v rudder >/dev/null 2>&1; then
  rudder login --url "$RUDDER_URL" >/dev/null 2>&1 || log "rudder login failed; rudder-image releases will fail"
fi
log "done"
