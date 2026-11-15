#!/usr/bin/env bash
# tests/jail.sh — the jail's properties (src/jail.sh), PASS/FAIL per property.
# Runs on the host, no display (WOK_DISPLAY=0): bwrap only, no compositor, no
# GPU work. The display and the GPU are tests/gpu.sh.
#
#   tests/jail.sh            # needs bwrap on PATH; `just jail-test`
#
# Scratch under target/jail-test (removed at the start of each run).

set -u
here=$(cd "$(dirname "$0")/.." && pwd)
JAIL=$here/src/jail.sh
T=$here/target/jail-test
CANARY=wok-canary-$$
uid=$(id -u)

pass=0 fail=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf 'FAIL  %s\n' "$1"; }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

rm -rf "$T"
mkdir -p "$T"
fresh() { rm -rf "$T/root"; mkdir -p "$T/root/job" "$T/root/out" "$T/root/home"; }
# J <argv…>: one job in the jail, a canary in the jail script's own env
J() { SECRETCANARY=$CANARY WOK_DISPLAY=0 "$JAIL" "$T/root" -- "$@" </dev/null; }
JS() { J bash -c "$1"; }
notJ() { ! J "$@" 2>/dev/null; }

fresh
# ---- basics ---------------------------------------------------------------
J true; check "exit status 0 passes through" [ $? -eq 0 ]
JS 'exit 7'; check "exit status 7 passes through" [ $? -eq 7 ]
out=$(JS 'echo out; echo err >&2' 2>&1)
check "stdout and stderr pass through" [ "$out" = $'out\nerr' ]
check "argv stays argv (no shell)" [ "$(J echo '$HOME;' '`id`')" = '$HOME; `id`' ]
check "cwd is /home/wok/job" [ "$(J pwd)" = /home/wok/job ]
check "HOME is /home/wok" [ "$(JS 'echo $HOME')" = /home/wok ]
check "WOK_OUT is /home/wok/out" [ "$(JS 'echo $WOK_OUT')" = /home/wok/out ]
check "hostname is wok" [ "$(J cat /proc/sys/kernel/hostname)" = wok ]
"$JAIL" 2>/dev/null; check "bad arguments exit 252" [ $? -eq 252 ]

# ---- env ------------------------------------------------------------------
env_out=$(JS 'env')
check "the host env does not leak (canary)" bash -c "! grep -q '$CANARY' <<<\"\$1\"" _ "$env_out"
check "no host credentials-like vars" bash -c "! grep -qE '^(SSH_AUTH_SOCK|DBUS_SESSION_BUS_ADDRESS|ANTHROPIC|GITHUB_TOKEN|DISPLAY|WAYLAND_DISPLAY)=' <<<\"\$1\"" _ "$env_out"
names=$(JS 'env | cut -d= -f1 | sort | tr "\n" " "')
expected="HOME LANG PATH PWD SHLVL TERM TMPDIR USER WOK_OUT XDG_CACHE_HOME XDG_RUNTIME_DIR _ "
check "the env is exactly the jail's ($names)" [ "$names" = "$expected" ]
check "WOK_JOBENV_<K> becomes K" [ "$(WOK_JOBENV_GAME_MODE=fast J bash -c 'echo $GAME_MODE')" = fast ]

# ---- writable: only the job's dirs --------------------------------------
fresh
JS 'echo j > f; echo o > $WOK_OUT/f; echo h > ~/f'
check "cwd, WOK_OUT and HOME land in the job root" bash -c "[ \"\$(cat '$T/root/job/f' '$T/root/out/f' '$T/root/home/f')\" = \$'j\no\nh' ]"
for p in / /etc /nix/store /usr /bin /sys; do
  [ -e "$p" ] || continue
  JS "echo x > '$p/wok-write-test'" 2>/dev/null
  check "$p is not writable" [ $? -ne 0 ]
done
check "the root dir is unchanged by write attempts" [ ! -e /wok-write-test ]
for d in /tmp /dev/shm /run /home; do
  check "$d is a private tmpfs" [ "$(J stat -f -c %T "$d")" = tmpfs ]
done
check "/tmp has a size (2 GiB)" [ "$(J sh -c 'echo $(( $(stat -f -c %b /tmp) * $(stat -f -c %S /tmp) ))')" = $((2 << 30)) ]
check "/dev/shm has a size (1 GiB)" [ "$(J sh -c 'echo $(( $(stat -f -c %b /dev/shm) * $(stat -f -c %S /dev/shm) ))')" = $((1 << 30)) ]
JS 'echo x > /tmp/t && echo x > /dev/shm/t'; check "/tmp and /dev/shm are writable" [ $? -eq 0 ]
check "the jail's /tmp is not the host's" [ ! -e /tmp/t ] || [ "$(cat /tmp/t 2>/dev/null)" != x ]

# ---- the filesystem is an allowlist ----------------------------------------
# Not a list of hidden paths: everything the jail has must be on a short list,
# so a host dir nobody thought of (/persist, /backups, a new mount) can't show.
top=$(J ls -A / | sort | tr '\n' ' ')
check "/ holds only the allowlist ($top)" bash -c '
  for e in $1; do case $e in bin|dev|etc|home|lib64|nix|proc|run|sys|tmp|usr) ;; *) exit 1 ;; esac; done' _ "$top"
check "/nix holds only the store" [ "$(J ls -A /nix)" = store ]
check "/home holds only the job's home" [ "$(J ls -A /home)" = wok ]
runs=$(J ls -A /run | tr '\n' ' ')
check "/run holds only the GPU's driver links ($runs)" bash -c '
  for e in $1; do case $e in current-system|opengl-driver|opengl-driver-32) ;; *) exit 1 ;; esac; done' _ "$runs"
# every mount point: on the list, a device node, or a masked mount in /etc or /sys
mounts=$(J cut -d' ' -f5 /proc/self/mountinfo | sort -u)
badm=$(J sh -c '
  for m in $(cut -d" " -f5 /proc/self/mountinfo | sort -u); do
    case $m in
      /|/bin|/dev|/dev/pts|/dev/shm|/etc|/home|/home/wok|/home/wok/job|/home/wok/out|/lib64|/nix/store|/proc|/run|/sys|/tmp|/usr) ;;
      /run/current-system|/run/opengl-driver|/run/opengl-driver-32|/run/wok/wayland-0) ;;
      /dev/null|/dev/zero|/dev/full|/dev/random|/dev/urandom|/dev/tty) ;; # bwrap --dev
      /dev/dri/renderD*|/dev/nvidia0|/dev/nvidiactl|/dev/nvidia-modeset|/dev/nvidia-uvm|/dev/nvidia-uvm-tools) ;;
      /etc/*|/sys/*)
        if [ -d "$m" ]; then [ -z "$(ls -A "$m")" ] || echo "$m"
        else [ -f "$m" ] && [ ! -s "$m" ] || echo "$m"; fi ;;
      *) echo "$m" ;;
    esac
  done')
check "every mount is on the allowlist or masked ($(wc -l <<<"$mounts") mounts; off it: [${badm//$'\n'/ }])" [ -z "$badm" ]
# the host's own mounts inside /etc and /sys (persisted keys, efivars…) are masked
host_sub=$(cut -d' ' -f5 /proc/self/mountinfo | grep -E '^/(etc|sys)/' | sort -u | head -20)
if [ -n "$host_sub" ]; then
  unmasked=$(J sh -c 'for m in "$@"; do if [ -d "$m" ]; then [ -z "$(ls -A "$m" 2>/dev/null)" ] || echo "$m"; elif [ -e "$m" ]; then [ -f "$m" ] && [ ! -s "$m" ] || echo "$m"; fi; done' _ $host_sub)
  check "the host's mounts in /etc and /sys are masked ($(wc -l <<<"$host_sub") of them)" [ -z "$unmasked" ]
fi
# a masked file reads as empty (an unreadable machine-id trips up D-Bus, SDL)
[ -e /etc/machine-id ] && check "/etc/machine-id is readable (empty, if masked)" J cat /etc/machine-id
check "masks are read-only" notJ sh -c 'for m in /sys/firmware/efi/efivars /sys/kernel/debug; do [ -d "$m" ] && : > "$m/x" && exit 0; done; exit 1'
# nested host mounts under /etc and /sys (tracefs under debugfs after perf or
# bpftrace; a persisted dir holding persisted files), simulated without root
# in a user and mount namespace: the jail still starts, the parent's mask
# hides the children
etcd=$(for d in /etc/*/; do [ -L "${d%/}" ] || { echo "${d%/}"; break; }; done)
if [ -n "$etcd" ] && unshare -rm true 2>/dev/null; then
  nested=$(unshare -rm --propagation private env SECRETCANARY=$CANARY WOK_DISPLAY=0 bash -c '
    d=$1 jail=$2 root=$3
    mount -t tmpfs t "$d" && mkdir "$d/inner" && : > "$d/key" &&
      mount -t tmpfs t "$d/inner" && : > "$d/inner/x" && mount --bind /etc/hostname "$d/key" || exit 90
    if mount -t tmpfs t /sys/kernel/debug 2>/dev/null; then
      mkdir /sys/kernel/debug/tracing && mount -t tmpfs t /sys/kernel/debug/tracing || exit 91
    fi
    "$jail" "$root" -- sh -c "ls -A \"\$1\" /sys/kernel/debug 2>/dev/null | grep -v -e : -e ^\$; echo ran" _ "$d" </dev/null
  ' _ "$etcd" "$JAIL" "$T/root" 2>&1)
  check "nested host mounts under /etc and /sys: the jail starts, all masked [${nested//$'\n'/ }]" [ "$nested" = ran ]
else
  echo "SKIP  nested host mounts (no unprivileged user namespaces)"
fi
# a canary written on the host, outside the allowlist: nowhere in the jail's
# writable or host-backed trees (the store, /proc and /sys aside)
canary=wok-fs-canary-$$-$RANDOM
echo "$canary" > "$T/canary"
check "a host canary file is unreachable" notJ grep -rqsF "$canary" /etc /usr /bin /run /home /tmp /dev/shm
check "the job root's host path is invisible" notJ test -e "$T/root"
for p in "$HOME" /persist /backups /root /boot /swap /var /srv /mnt /media /nix/var /run/user/$uid /tmp/.X11-unix; do
  check "$p is absent" notJ test -e "$p"
done

# ---- network ---------------------------------------------------------------
# /sys is the host's (read-only; GPU drivers read it), so ask the netns itself
check "only lo in the jail's netns" [ "$(J sh -c "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '")" = lo ]
JS 'exec 3<>/dev/tcp/1.1.1.1/53' 2>/dev/null; check "no route out (tcp 1.1.1.1:53)" [ $? -ne 0 ]
JS 'exec 3<>/dev/tcp/127.0.0.1/17760' 2>/dev/null; check "host loopback is unreachable (nix daemon port)" [ $? -ne 0 ]

# ---- namespaces -------------------------------------------------------------
check "no nested user namespaces" notJ unshare -U true
check "own cgroup namespace" [ "$(J cat /proc/self/cgroup)" = "0::/" ]

# ---- signals -----------------------------------------------------------------
# a job starts with no signal ignored (a background job in a non-interactive
# shell would get INT and QUIT ignored): Python's KeyboardInterrupt, kill -INT
ign=$(J sh -c 'sed -n "s/^SigIgn:\t//p" /proc/self/status')
check "the job starts with no signal ignored (SigIgn $ign)" [ "$((16#${ign:-1}))" -eq 0 ]
J sh -c 'kill -INT $$; sleep 2'; rc=$?
check "a SIGINT stops the job (rc $rc)" [ $rc -eq 130 ]

# ---- processes and tty -----------------------------------------------------
J kill -0 $$ 2>/dev/null; check "host processes are invisible (own pid ns)" [ $? -ne 0 ]
check "few processes in the jail" [ "$(J sh -c 'ls /proc | grep -c "^[0-9]"')" -le 5 ]
JS 'exec 3</dev/tty' 2>/dev/null; check "no controlling tty" [ $? -ne 0 ]
check "stdin is not a tty" notJ test -t 0
marker=wok-left-behind-$$
# the marker lives in a sh command line (`exec -a` doesn't work on coreutils' multi-call sleep)
JS "sh -c 'sleep 300; : $marker' & disown; sleep 0.3; pgrep -f '[w]${marker#w}' >/dev/null" \
  && ok "the leftover was running inside the jail" || bad "the leftover was running inside the jail"
sleep 0.5
check "nothing outlives the job" bash -c "! pgrep -f '[w]${marker#w}' >/dev/null"

# ---- GPU nodes (present, not exercised) ------------------------------------
[ -e /dev/dri ] && check "/dev/dri is there" J test -d /dev/dri
for n in /dev/dri/renderD*; do [ -c "$n" ] && check "$n is there" J test -c "$n"; done
check "no DRM card (primary) nodes" J sh -c '! ls /dev/dri/card* 2>/dev/null | grep -q .'
[ -c /dev/dri/renderD128 ] && check "the render node opens (read, no GPU work)" J sh -c 'exec 3</dev/dri/renderD128' 
[ -e /dev/nvidiactl ] && check "/dev/nvidiactl is there" J test -c /dev/nvidiactl
check "other devices are not (no /dev/sda, /dev/input)" J sh -c '! ls -d /dev/sd* /dev/nvme* /dev/input 2>/dev/null | grep -q .'

# ---- limits (a transient user scope) ---------------------------------------
scope=$(command -v systemd-run || true)
if [ -n "$scope" ] && "$scope" --user --scope --quiet --collect -- true 2>/dev/null; then
  JL() { WOK_SCOPE=$scope WOK_MEMORY_MAX=256M WOK_TASKS_MAX=64 WOK_DISPLAY=0 "$JAIL" "$T/root" -- "$@" </dev/null; }
  check "the job runs in a scope of its own" bash -c "[ \"\$(WOK_SCOPE=$scope WOK_DISPLAY=0 '$JAIL' '$T/root' -- true </dev/null; echo \$?)\" = 0 ]"
  JL sh -c 'head -c 600M /dev/zero > /tmp/fill && echo filled' >"$T/mem.log" 2>&1
  rc=$?
  check "memory is capped: 600M into tmpfs under a 256M cap is killed (rc $rc)" bash -c "[ $rc -ne 0 ] && ! grep -q filled '$T/mem.log'"
  out=$(JL sh -c 'for i in $(seq 200); do sleep 3 & done 2>/dev/null; jobs -p | wc -l' 2>/dev/null)
  check "tasks are capped: 200 forks under a cap of 64 (got ${out:-0})" bash -c "[ \"\${1:-0}\" -lt 200 ]" _ "$out"
else
  bad "no systemd user scope here: limits not checked"
fi

# ---- show mode (WOK_DISPLAY=show): one socket, nothing else ---------------
# A stand-in for the daemon's listener on the desktop: a socket that answers
# "hi". The real compositor and its security context are tests/show.sh.
py=$(command -v python3 || true)
if [ -n "$py" ]; then
  fresh
  sock=$T/show.sock
  "$py" -c '
import socket, sys
s = socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(4)
while True:
    c, _ = s.accept(); c.sendall(b"hi"); c.close()
' "$sock" &
  spid=$!
  for _ in $(seq 50); do [ -S "$sock" ] && break; sleep 0.1; done
  JW() { WOK_DISPLAY=show WOK_SHOW_SOCKET=$sock "$JAIL" "$T/root" -- "$@" </dev/null; }
  notJW() { ! JW "$@" 2>/dev/null; }
  check "show: WAYLAND_DISPLAY is wayland-0, no DISPLAY" [ "$(JW sh -c 'echo "$WAYLAND_DISPLAY [${DISPLAY:-}]"')" = "wayland-0 []" ]
  check "show: the runtime dir holds only wayland-0" [ "$(JW ls -A /run/wok)" = wayland-0 ]
  check "show: wayland-0 reaches the daemon's listener" [ "$(JW "$py" -c '
import socket; s = socket.socket(socket.AF_UNIX); s.connect("/run/wok/wayland-0"); print(s.recv(2).decode())')" = hi ]
  check "show: the runtime dir is private" [ "$(JW stat -c %a /run/wok)" = 700 ]
  check "show: /run/user/$uid is invisible" notJW test -e /run/user/$uid
  check "show: the X11 sockets are invisible" notJW test -e /tmp/.X11-unix
  check "show: the listener's host path is invisible" notJW test -e "$sock"
  names=$(JW sh -c 'env | cut -d= -f1 | sort | tr "\n" " "')
  check "show: the env is the jail's plus WAYLAND_DISPLAY ($names)" \
    [ "$names" = "HOME LANG PATH PWD SHLVL TERM TMPDIR USER WAYLAND_DISPLAY WOK_OUT XDG_CACHE_HOME XDG_RUNTIME_DIR _ " ]
  WOK_DISPLAY=show "$JAIL" "$T/root" -- true 2>/dev/null
  check "show: no socket exits 252" [ $? -eq 252 ]
  : > "$T/notasock"
  WOK_DISPLAY=show WOK_SHOW_SOCKET=$T/notasock "$JAIL" "$T/root" -- true 2>/dev/null
  check "show: a plain file exits 252" [ $? -eq 252 ]
  ln -s "$sock" "$T/link.sock"
  WOK_DISPLAY=show WOK_SHOW_SOCKET=$T/link.sock "$JAIL" "$T/root" -- true 2>/dev/null
  check "show: a link exits 252" [ $? -eq 252 ]
  WOK_SCREENSHOT=1 JW true 2>/dev/null
  check "show: a screenshot exits 252" [ $? -eq 252 ]
  WOK_DISPLAY=bogus "$JAIL" "$T/root" -- true 2>/dev/null
  check "a bad WOK_DISPLAY exits 252" [ $? -eq 252 ]
  kill "$spid"; wait "$spid" 2>/dev/null
else
  bad "python3 missing: show-mode checks skipped"
fi

# ---- the daemon with the real jail -----------------------------------------
wok=$here/target/debug/wok
if [ -x "$wok" ]; then
  d=$T/daemon
  mkdir -p "$d"
  env -u WAYLAND_DISPLAY "$wok" daemon --socket "$d/s" --state "$d/state" 2>"$d/log" &
  dpid=$!
  for _ in $(seq 50); do [ -S "$d/s" ] && break; sleep 0.1; done
  mkdir -p "$d/in" && echo hello > "$d/in/greeting"
  out=$(cd "$d" && WOK_SOCKET=$d/s "$wok" run --no-display --in in --out res -- \
    sh -c 'cat greeting; cp greeting $WOK_OUT/; ln -s /etc/passwd $WOK_OUT/passwd' 2>"$d/err")
  rc=$?
  check "daemon + jail: runs, output streams" [ $rc -eq 0 ] && [ "$out" = hello ]
  check "daemon + jail: results come back" [ "$(cat "$d/res/greeting" 2>/dev/null)" = hello ]
  check "daemon + jail: a planted link is left out" [ ! -e "$d/res/passwd" ] && [ ! -L "$d/res/passwd" ]
  check "daemon + jail: the job dir is gone" [ -z "$(ls -A "$d/state/jobs")" ]
  mark=wok-timeout-$$
  t0=$SECONDS
  (cd "$d" && WOK_SOCKET=$d/s "$wok" run --no-display --timeout 2 -- \
    bash -c "trap '' TERM; sh -c 'trap \"\" TERM; sleep 300; : $mark' & wait" 2>/dev/null)
  rc=$?
  check "daemon + jail: a timeout exits 124 (got $rc, $((SECONDS - t0))s)" [ $rc -eq 124 ]
  check "daemon + jail: a TERM-ignoring job is gone after its timeout" bash -c "! pgrep -f '[w]${mark#w}' >/dev/null"
  # the timeout's TERM reaches the job, which may still write its results
  (cd "$d" && WOK_SOCKET=$d/s "$wok" run --no-display --timeout 2 --out term -- \
    sh -c 'trap "echo flushed > \$WOK_OUT/last; exit 0" TERM; sleep 60 & wait' >/dev/null 2>&1)
  rc=$?
  check "daemon + jail: a timeout TERMs the job first, its last write comes back (rc $rc)" \
    bash -c "[ $rc -eq 124 ] && [ \"\$(cat '$d/term/last' 2>/dev/null)\" = flushed ]"
  kill "$dpid"; wait "$dpid" 2>/dev/null
  # a second daemon whose free-space floor is 64 MiB below what is free now:
  # a job that writes past it is killed (and ignoring TERM doesn't help)
  mkdir -p "$d/state2"
  free=$(( $(stat -f -c '%a * %S' "$d/state2") ))
  env -u WAYLAND_DISPLAY WOK_MIN_FREE=$((free - (64 << 20))) "$wok" daemon --socket "$d/s2" --state "$d/state2" 2>"$d/log2" &
  dpid=$!
  for _ in $(seq 50); do [ -S "$d/s2" ] && break; sleep 0.1; done
  t0=$SECONDS
  err=$(cd "$d" && WOK_SOCKET=$d/s2 "$wok" run --no-display --out big -- \
    sh -c 'trap "" TERM; for i in $(seq 400); do head -c 1M /dev/urandom > $WOK_OUT/f$i; sync -f $WOK_OUT; sleep 0.02; done' 2>&1 >/dev/null)
  rc=$?
  check "daemon + jail: a job that fills the disk is killed with a reason (rc $rc, $((SECONDS - t0))s)" \
    bash -c "[ $rc -eq 125 ] && grep -q 'stopped: the host.s disk is nearly full' <<<\"\$1\"" _ "$err"
  for _ in $(seq 50); do [ -z "$(ls -A "$d/state2/jobs")" ] && break; sleep 0.1; done
  check "daemon + jail: its dir is gone" [ -z "$(ls -A "$d/state2/jobs")" ]
  grep -q 'no systemd user scope' "$d/log2" && bad "daemon: no scope (no memory/task caps)" || ok "daemon: jobs get a scope (no warning in its log)"
  kill "$dpid"; wait "$dpid" 2>/dev/null
else
  bad "target/debug/wok missing: cargo build first"
fi

rm -rf "$T"
echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
