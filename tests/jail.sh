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
for d in /tmp /dev/shm /run /var /srv /mnt /media /nix/var /home; do
  [ -d "$d" ] || continue
  check "$d is a private tmpfs" [ "$(J stat -f -c %T "$d")" = tmpfs ]
done
JS 'echo x > /tmp/t && echo x > /dev/shm/t'; check "/tmp and /dev/shm are writable" [ $? -eq 0 ]
check "the jail's /tmp is not the host's" [ ! -e /tmp/t ] || [ "$(cat /tmp/t 2>/dev/null)" != x ]

# ---- invisible -------------------------------------------------------------
check "the host home is invisible" notJ test -e "$HOME"
check "~/.ssh is invisible" notJ test -e "$HOME/.ssh"
check "the job root's host path is invisible" notJ test -e "$T/root"
check "/run/user/$uid is invisible" notJ test -e /run/user/$uid
check "the X11 sockets are invisible" notJ test -e /tmp/.X11-unix
check "the nix daemon socket is invisible" notJ test -e /nix/var/nix/daemon-socket/socket
if [ -d /var/lib ] && [ -n "$(ls -A /var/lib)" ]; then
  check "host /var/lib is invisible" [ -z "$(J ls -A /var/lib 2>/dev/null)" ]
fi

# ---- network ---------------------------------------------------------------
# /sys is the host's (read-only; GPU drivers read it), so ask the netns itself
check "only lo in the jail's netns" [ "$(J sh -c "tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' '")" = lo ]
JS 'exec 3<>/dev/tcp/1.1.1.1/53' 2>/dev/null; check "no route out (tcp 1.1.1.1:53)" [ $? -ne 0 ]
JS 'exec 3<>/dev/tcp/127.0.0.1/17760' 2>/dev/null; check "host loopback is unreachable (nix daemon port)" [ $? -ne 0 ]

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
[ -e /dev/nvidiactl ] && check "/dev/nvidiactl is there" J test -c /dev/nvidiactl
check "other devices are not (no /dev/sda, /dev/input)" J sh -c '! ls -d /dev/sd* /dev/nvme* /dev/input 2>/dev/null | grep -q .'

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
  kill "$dpid"; wait "$dpid" 2>/dev/null
else
  bad "target/debug/wok missing: cargo build first"
fi

rm -rf "$T"
echo
echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
