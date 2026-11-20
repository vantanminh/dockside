#!/bin/sh
# dockside-server container entrypoint: install the toolchain into /data in the background, then run the server.
# Secrets are read from the environment only; nothing is printed or written to the image.
set -eu
mkdir -p /data/toolchain /data/cache
# Optional: base64 of a dockside.toml (targets, policy; not secret) to keep the config out of the image.
if [ -n "${DOCKSIDE_CONFIG_B64:-}" ]; then
  printf '%s' "$DOCKSIDE_CONFIG_B64" | base64 -d >/data/dockside.toml
  export DOCKSIDE_CONFIG=/data/dockside.toml
fi
/usr/local/bin/dockside-bootstrap-tools >>/data/toolchain/bootstrap.log 2>&1 &
exec /usr/local/bin/dockside-server "$@"
