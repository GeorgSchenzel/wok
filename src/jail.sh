#!/usr/bin/env bash
# jail.sh — run one wok job in a bubblewrap jail on the host.
#
#   jail.sh <job-root> -- <argv...>
#
# <job-root> is the daemon's private dir for this job, holding job/ (the
# uploaded files), out/ (what goes back) and home/ (a scratch HOME). In the
# jail the home is /home/wok ($HOME), with job/ at /home/wok/job (the cwd)
# and out/ at /home/wok/out ($WOK_OUT): fixed paths, so no host path reaches
# a job.
#
# The daemon runs it with a cleared environment plus:
#   WOK_BWRAP        bwrap to use (else from PATH)
#   WOK_DISPLAY=1    start a headless compositor for the job
#   WOK_DISPLAY=show the job's windows go to the desktop, through
#                    WOK_SHOW_SOCKET: a listener the daemon made on the
#                    desktop's compositor for this job (security context),
#                    bound as the job's only Wayland socket
#   WOK_SCREENSHOT=s with WOK_DISPLAY: run the job in the background, grim
#                    $WOK_OUT/screenshot.png after s seconds, then stop the job
#   WOK_JAIL_PATH    dirs put first on the job's PATH (the package's sway, grim)
#   WOK_JOBENV_<K>   the job's own env, as K
#   WOK_SCOPE        systemd-run to put the job in a transient user scope
#                    with WOK_MEMORY_MAX and WOK_TASKS_MAX (unset: no cgroup
#                    limits; the daemon warns). XDG_RUNTIME_DIR and
#                    DBUS_SESSION_BUS_ADDRESS are for systemd-run; the job
#                    never sees them (--clearenv).
#
# Properties (tests/jail.sh):
#   - the filesystem is an allowlist, not the host's / with holes: an empty
#     root with only /nix/store, /etc, /usr, /bin, /lib64, /sys (read-only),
#     a fresh /dev and /proc, the GPU's driver links in /run, and private
#     tmpfs for /tmp, /dev/shm, /run, /home. Mounts the host has inside /etc
#     or /sys (persisted host keys, machine-id, efivars, debugfs…) are masked.
#     Nothing else of the host exists in the jail: no home, no /persist, no
#     /backups, no /var, no /nix/var (the nix daemon socket).
#   - writable on disk: only the job's home with job/ and out/ (all in
#     <job-root>, removed by the daemon after the job; the daemon stops a
#     job when the disk gets nearly full); /tmp, /dev/shm, /run and /home are
#     sized tmpfs (bwrap's /dev isn't; any tmpfs counts as the job's memory
#     in its scope)
#   - no network: own netns, nothing up (AF_VSOCK isn't netns-scoped; there
#     is no seccomp filter yet)
#   - own user, pid, ipc, uts and cgroup namespaces; no nested user
#     namespaces (--disable-userns)
#   - memory and tasks capped by the scope (WOK_SCOPE)
#   - no host environment (clearenv): API keys in the daemon's env don't leak
#   - no controlling tty (--new-session; stdin is /dev/null)
#   - GPU: the DRM render nodes (not the card nodes), the NVIDIA nodes,
#     /run/opengl-driver
#   - display: never the desktop's. A headless sway per job
#     (WLR_BACKENDS=headless, gles2, a generated config, XWayland disabled, so
#     it can't race the session for :0). So: no X11 in jobs, and the headless
#     seat has no input devices (mesa-demos' eglut tools crash on that).
#     --show: the desktop's compositor, but only through the job's own
#     sandboxed listener (never the session's socket); still no X11.
#   - stopping: a TERM to this script is passed to the jail's pid 1, which
#     passes it to every process in the jail, so a job can flush its output;
#     a KILL to this script takes the jail along (--die-with-parent).
#
# Exit codes of its own: 250 the compositor didn't start; 251 the job exited
# before its screenshot; 252 bad arguments.

set -euo pipefail

[ $# -ge 3 ] && [ "$2" = "--" ] || { echo "usage: jail.sh <job-root> -- <argv...>" >&2; exit 252; }
root=$1
shift 2
for d in job out home; do
  [ -d "$root/$d" ] && [ ! -L "$root/$d" ] || { echo "wok jail: $root/$d is not a dir" >&2; exit 252; }
done

bwrap=${WOK_BWRAP:-$(command -v bwrap)} || { echo "wok jail: bwrap not found" >&2; exit 252; }

show=()
case ${WOK_DISPLAY:-0} in
  0|1) ;;
  show)
    sock=${WOK_SHOW_SOCKET:-}
    [ -S "$sock" ] && [ ! -L "$sock" ] || { echo "wok jail: WOK_SHOW_SOCKET is not a socket" >&2; exit 252; }
    [ -z "${WOK_SCREENSHOT:-}" ] || { echo "wok jail: no screenshot with --show" >&2; exit 252; }
    show=(--bind "$sock" /run/wok/wayland-0)
    ;;
  *) echo "wok jail: bad WOK_DISPLAY" >&2; exit 252 ;;
esac
user=$(id -un)

# Runs inside the jail as its pid 1, `bash -c <this> wok-job <argv...>`: the
# job's argv stays argv, never re-quoted through a shell. As pid 1 it gets
# only the signals it handles: a TERM goes to every process in the jail.
INNER='
set -u
shot=${WOK_SHOT:-}
display=${WOK_DISPLAY:-0}
unset WOK_SHOT WOK_DISPLAY
trap "kill -TERM -1 2>/dev/null" TERM
# wait for pid $1 even through the TERM trap; its status in rc
await() { while :; do wait "$1"; rc=$?; kill -0 "$1" 2>/dev/null || return 0; done; }
# the job in the background, its pid in $!: a non-interactive shell would
# start it with INT and QUIT ignored, so give them back their defaults
job() { (trap - INT QUIT; exec "$@") & }
if [ "$display" = show ]; then
  chmod 700 "$XDG_RUNTIME_DIR" || exit 250
  export WAYLAND_DISPLAY=wayland-0
fi
if [ "$display" != 1 ]; then
  job "$@"
  await $!
  exit $rc
fi
rt=$XDG_RUNTIME_DIR
mkdir -p "$rt" && chmod 700 "$rt" || exit 250
printf "%s\n" "output HEADLESS-1 mode --custom 1920x1080@60.000Hz" "xwayland disable" > "$rt/sway.conf"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=gles2 \
  sway -c "$rt/sway.conf" > "$rt/sway.log" 2>&1 &
sw=$!
wd=
for i in $(seq 1 100); do # 10 s for the compositor to come up
  for s in "$rt"/wayland-[0-9]*; do
    [ -S "$s" ] && { wd=${s##*/}; break 2; }
  done
  kill -0 "$sw" 2>/dev/null || break
  sleep 0.1
done
if [ -z "$wd" ]; then
  echo "wok: the headless compositor did not start:" >&2
  tail -5 "$rt/sway.log" >&2
  kill "$sw" 2>/dev/null; wait "$sw" 2>/dev/null
  exit 250
fi
export WAYLAND_DISPLAY=$wd
if [ -z "$shot" ]; then
  job "$@"
  await $!
else
  # the app runs in the background: a foreground app is gone at capture
  # time and grim gives an all-black PNG with exit 0 (learnings.md)
  job "$@"
  app=$!
  sleep "$shot" &
  wait $!
  if kill -0 "$app" 2>/dev/null; then
    grim "$WOK_OUT/screenshot.png"
    rc=$?
    kill "$app" 2>/dev/null; wait "$app" 2>/dev/null
  else
    wait "$app"
    echo "wok: the job exited (status $?) before its screenshot" >&2
    rc=251
  fi
fi
kill "$sw" 2>/dev/null; wait "$sw" 2>/dev/null
exit $rc
'

env_job=()
for name in ${!WOK_JOBENV_@}; do
  env_job+=(--setenv "${name#WOK_JOBENV_}" "${!name}")
done

# The allowlist. Read-only trees from the host:
ro=()
for d in /nix/store /etc /usr /bin /lib64 /sys; do
  [ -d "$d" ] && ro+=(--ro-bind "$d" "$d")
done
# … minus whatever the host mounts inside /etc and /sys (impermanence binds
# persisted files there: host keys, machine-id, NetworkManager's secrets;
# /sys has efivars, debugfs, tracefs, bpf…): an empty read-only dir or file
# instead (a readable empty file: an unreadable machine-id trips up D-Bus
# and SDL).
[ -d "$root/.mask-dir" ] || mkdir -m 555 "$root/.mask-dir"
[ -f "$root/.mask-file" ] || { : > "$root/.mask-file"; chmod 444 "$root/.mask-file"; }
mask=()
while read -r _ _ _ _ mp _; do
  # mountinfo escapes space, tab, newline and backslash as \ooo
  mp=${mp//\\040/ }; mp=${mp//\\011/$'\t'}; mp=${mp//\\012/$'\n'}; mp=${mp//\\134/\\}
  case $mp in
    /etc/*|/sys/*)
      if [ -d "$mp" ]; then mask+=(--ro-bind "$root/.mask-dir" "$mp")
      elif [ -e "$mp" ]; then mask+=(--ro-bind "$root/.mask-file" "$mp")
      fi ;;
  esac
done < /proc/self/mountinfo
# GPU: render nodes and NVIDIA's nodes only
gpu=()
for n in /dev/dri/renderD* /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm /dev/nvidia-uvm-tools; do
  [ -c "$n" ] && gpu+=(--dev-bind "$n" "$n")
done
runs=()
for l in /run/opengl-driver /run/opengl-driver-32 /run/current-system; do
  [ -e "$l" ] && runs+=(--ro-bind "$l" "$l")
done

scope=()
if [ -n "${WOK_SCOPE:-}" ]; then
  scope=("$WOK_SCOPE" --user --scope --quiet --collect --expand-environment=no
    -p "MemoryMax=${WOK_MEMORY_MAX:-8G}" -p MemorySwapMax=0
    -p "TasksMax=${WOK_TASKS_MAX:-1024}" --)
fi

info=$root/.bwrap-info
rm -f "$info"
# bwrap in a process group of its own (job control): the daemon's TERM to our
# group must reach the jail's pid 1 through us, not kill bwrap (whose
# --die-with-parent would then KILL the jail at once)
set -m
"${scope[@]}" "$bwrap" \
  --unshare-user \
  --disable-userns \
  --unshare-net \
  --unshare-pid \
  --unshare-ipc \
  --unshare-uts \
  --unshare-cgroup-try \
  --as-pid-1 \
  --info-fd 3 \
  --hostname wok \
  --new-session \
  --die-with-parent \
  --tmpfs / \
  "${ro[@]}" \
  "${mask[@]}" \
  --proc /proc \
  --dev /dev \
  --size $((1 << 30)) --tmpfs /dev/shm \
  "${gpu[@]}" \
  --size $((16 << 20)) --tmpfs /home \
  --bind "$root/home" /home/wok \
  --bind "$root/job" /home/wok/job \
  --bind "$root/out" /home/wok/out \
  --size $((64 << 20)) --tmpfs /run \
  "${runs[@]}" \
  "${show[@]}" \
  --size $((2 << 30)) --tmpfs /tmp \
  --remount-ro / \
  --chdir /home/wok/job \
  --clearenv \
  --setenv HOME /home/wok \
  --setenv USER "$user" \
  --setenv PATH "${WOK_JAIL_PATH:+$WOK_JAIL_PATH:}/run/current-system/sw/bin:/etc/profiles/per-user/$user/bin" \
  --setenv LANG C.UTF-8 \
  --setenv TERM dumb \
  --setenv TMPDIR /tmp \
  --setenv XDG_RUNTIME_DIR /run/wok \
  --setenv XDG_CACHE_HOME /home/wok/.cache \
  --setenv WOK_OUT /home/wok/out \
  --setenv WOK_DISPLAY "${WOK_DISPLAY:-0}" \
  --setenv WOK_SHOT "${WOK_SCREENSHOT:-}" \
  "${env_job[@]}" \
  -- "$BASH" -c "$INNER" wok-job "$@" 3>"$info" &
bw=$!
set +m

# A TERM: to the jail's pid 1 (its host pid, from bwrap's info).
forward() {
  local pid=
  for _ in $(seq 20); do
    pid=$(sed -n 's/.*"child-pid": *\([0-9]*\).*/\1/p' "$info" 2>/dev/null)
    [ -n "$pid" ] && break
    sleep 0.05
  done
  if [ -n "$pid" ]; then kill -TERM "$pid" 2>/dev/null; else kill -KILL "$bw" 2>/dev/null; fi
}
trap forward TERM
set +e
while :; do
  wait "$bw"
  rc=$?
  kill -0 "$bw" 2>/dev/null || break
done
rm -f "$info"
exit $rc
