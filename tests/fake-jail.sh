#!/usr/bin/env bash
# A stand-in for src/jail.sh in tests: the same arguments and env, no bwrap,
# no display, host paths. The jail itself is tests/jail.sh (on the host).
set -eu
root=$1
shift 2
cd "$root/job"
export HOME=$root/home WOK_OUT=$root/out
for name in ${!WOK_JOBENV_@}; do
  export "${name#WOK_JOBENV_}=${!name}"
done
exec "$@"
