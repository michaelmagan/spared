#!/bin/sh
# Container entrypoint: spared serve with the image's defaults. Extra arguments are appended,
# unless the first one is `spared`: then the arguments run as a command of their own.
#
#   spared serve --data-dir /data --examples-dir /examples --listen 0.0.0.0:${PORT:-8080}
#
# The admin listener stays on 127.0.0.1:8081 inside the container and is never published; its
# bearer token is generated into /data/admin-token on first start.
#
# Path routing (/_/<app>/) is on by default here so the bundled examples answer on a single
# Railway or Fly hostname. It puts every app on one browser origin, and the server prints a
# warning saying so at startup. Pass -e SPARED_PATH_ROUTING=0 to turn it off.
# Hosted platforms (Fly, Railway) mount a fresh volume owned by root, so when started as root this
# script fixes ownership of /data and then drops to the unprivileged `spared` user (uid 10001)
# before running the server, with no-new-privs set and the bounding and inheritable capability
# sets emptied, so a setuid binary in the image cannot raise it again. Started with --user, it
# runs as that user directly and needs no capability at all. As root it needs CHOWN, SETUID,
# SETGID and SETPCAP (the Docker defaults include all four).
set -eu
SPARED_PATH_ROUTING=${SPARED_PATH_ROUTING:-1}
export SPARED_PATH_ROUTING
# `docker run IMAGE spared <command>` (for example `spared --version`) runs that command as given.
if [ "${1:-}" != spared ]; then
  set -- spared serve --data-dir /data --examples-dir /examples --listen "0.0.0.0:${PORT:-8080}" "$@"
fi
if [ "$(id -u)" = 0 ]; then
  if [ "$(stat -c %u /data)" != 10001 ]; then chown -R spared:spared /data; fi
  exec setpriv --reuid=10001 --regid=10001 --clear-groups --no-new-privs --bounding-set=-all --inh-caps=-all "$@"
fi
exec "$@"
