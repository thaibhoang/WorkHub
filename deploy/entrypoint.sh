#!/bin/sh
# Migrate, then serve with login required. WH_FOREMAN=1 also runs the pipeline
# (agents, scorers, improver, benchmarks, GitHub poller) in the same process.
set -eu
wh db migrate
set -- wh serve --addr 0.0.0.0:7070 --auth --trust-proxy "$@"
if [ "${WH_FOREMAN:-0}" = 1 ]; then
    set -- "$@" --foreman --max-parallel "${WH_MAX_PARALLEL:-1}"
fi
exec "$@"
