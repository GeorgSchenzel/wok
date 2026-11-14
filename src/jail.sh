#!/usr/bin/env bash
# jail.sh — run one wok job in a bubblewrap jail on the host.
#
#   jail.sh <job-root> -- <argv...>
#
# <job-root> is the daemon's private dir for this job, holding job/ (the
# uploaded files), out/ (what goes back) and home/ (a scratch HOME). In the
# jail the home is /home/wok ($HOME), with job/ at /home/wok/job (the cwd)
# and out/ at /home/wok/out ($WOK_OUT): fixed paths, so no host path reaches
# a job. (They sit on the /home tmpfs because bwrap can't make mount points
# under the read-only /.)
#
# The daemon runs it with a cleared environment plus:
#   WOK_BWRAP        bwrap to use (else from PATH)
#   WOK_DISPLAY=1    start a headless compositor for the job
#   WOK_SCREENSHOT=s with WOK_DISPLAY: run the job in the background, grim
#                    $WOK_OUT/screenshot.png after s seconds, then stop the job
#   WOK_JAIL_PATH    dirs put first on the job's PATH (the package's sway, grim)
#   WOK_JOBENV_<K>   the job's own env, as K
#
# Properties (from the spike's R5/G1 jail, its 56 checks; tests/jail.sh):
#   - no network: own netns, nothing up
#   - writable on disk: only the job's home with job/ and out/ (all in
#     <job-root>, removed by the daemon after the job); /tmp, /dev/shm, /run,
#     /var… are private tmpfs
#   - invisible: every host home (~/.ssh, credentials, stews, boxes' dirs),
#     /run/user/<uid> (agent sockets, dbus, the desktop's wayland socket),
#     /tmp/.X11-unix, the nix daemon socket (/nix/var), host /var /srv /mnt /media
#   - no host environment (clearenv): API keys in the daemon's env don't leak
#   - no controlling tty (--new-session; stdin is /dev/null)
#   - GPU: /dev/dri and the NVIDIA nodes, /run/opengl-driver
#   - display: never the desktop's. A headless sway per job
#     (WLR_BACKENDS=headless, gles2, a generated config, XWayland disabled, so
#     it can't race the session for :0). So: no X11 in jobs, and the headless
#     seat has no input devices (mesa-demos' eglut tools crash on that).
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
user=$(id -un)

# Runs inside the jail as `bash -c <this> wok-job <argv...>`: the job's argv
# stays argv, never re-quoted through a shell.
INNER='
set -u
shot=${WOK_SHOT:-}
display=${WOK_DISPLAY:-0}
unset WOK_SHOT WOK_DISPLAY
if [ "$display" != 1 ]; then
  exec "$@"
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
  "$@"
  rc=$?
else
  # the app runs in the background: a foreground app is gone at capture
  # time and grim gives an all-black PNG with exit 0 (learnings.md)
  "$@" &
  app=$!
  sleep "$shot"
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

sys_tmpfs=()
for d in /var /srv /mnt /media; do
  [ -d "$d" ] && sys_tmpfs+=(--tmpfs "$d")
done

exec "$bwrap" \
  --ro-bind / / \
  --dev /dev \
  --tmpfs /dev/shm \
  --dev-bind-try /dev/dri /dev/dri \
  --dev-bind-try /dev/nvidia0 /dev/nvidia0 \
  --dev-bind-try /dev/nvidiactl /dev/nvidiactl \
  --dev-bind-try /dev/nvidia-modeset /dev/nvidia-modeset \
  --dev-bind-try /dev/nvidia-uvm /dev/nvidia-uvm \
  --dev-bind-try /dev/nvidia-uvm-tools /dev/nvidia-uvm-tools \
  --tmpfs /home \
  --bind "$root/home" /home/wok \
  --bind "$root/job" /home/wok/job \
  --bind "$root/out" /home/wok/out \
  --tmpfs /run \
  --ro-bind-try /run/opengl-driver /run/opengl-driver \
  --ro-bind-try /run/opengl-driver-32 /run/opengl-driver-32 \
  --ro-bind-try /run/current-system /run/current-system \
  --tmpfs /tmp \
  --tmpfs /nix/var \
  "${sys_tmpfs[@]}" \
  --proc /proc \
  --unshare-net \
  --unshare-pid \
  --unshare-ipc \
  --unshare-uts \
  --hostname wok \
  --new-session \
  --die-with-parent \
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
  -- "$BASH" -c "$INNER" wok-job "$@"
