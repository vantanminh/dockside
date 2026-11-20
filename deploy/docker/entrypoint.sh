#!/bin/sh
# dockside-server container entrypoint: install the toolchain into /data in the background, then run the server.
# Secrets are read from the environment only; nothing is printed or written to the image.
set -eu
mkdir -p /data/toolchain /data/cache
/usr/local/bin/dockside-bootstrap-tools >>/data/toolchain/bootstrap.log 2>&1 &
exec /usr/local/bin/dockside-server "$@"
