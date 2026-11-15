#!/usr/bin/env bash
# tests/show.sh — `wok run --show` against a real sway, but never the
# desktop's: a private headless sway (pixman renderer, no GPU work) in a
# scratch runtime dir stands in for the session. Checks the security context
# (no screencopy for the job), the job's window
# landing on that sway tagged sandbox engine `wok`, the lanes, the cleanup,
# and that the real desktop session is untouched. PASS/FAIL per check.
#
#   just show-test
#
# Scratch under target/show-test. Everything it starts is stopped by PID.
# Tools come from this flake's nixpkgs.

set -u
here=$(cd "$(dirname "$0")/.." && pwd)
wok=$here/target/debug/wok
T=$here/target/show-test
uid=$(id -u)

pass=0 fail=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf 'FAIL  %s\n' "$1"; }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

[ -x "$wok" ] || { echo "build first: cargo build"; exit 2; }

echo "building the tools…"
tools=$(cd "$here" && nix build --no-link --print-out-paths --inputs-from . \
  nixpkgs#sway-unwrapped nixpkgs#grim nixpkgs#coreutils nixpkgs#bash \
  nixpkgs#wayland-utils nixpkgs#weston nixpkgs#wl-clipboard) || exit 2
jail_path=$(printf '%s/bin:' $tools)
jail_path=${jail_path%:}
bin() { printf '%s\n' $tools | while read -r p; do [ -x "$p/bin/$1" ] && { echo "$p/bin/$1"; break; }; done; }
sway=$(bin sway) swaymsg=$(bin swaymsg) winfo=$(bin wayland-info) wlcopy=$(bin wl-copy)

desktop() {
  for p in sway Xwayland pipewire wireplumber; do
    printf '%s: %s\n' "$p" "$(pgrep -u "$uid" -x "$p" | sort | tr '\n' ' ')"
  done
  printf 'sockets: %s\n' "$(ls /run/user/$uid | grep -E '^wayland-' | tr '\n' ' ')"
  printf 'x11: %s\n' "$(ls /tmp/.X11-unix 2>/dev/null | tr '\n' ' ')"
}

rm -rf "$T"
mkdir -p "$T/rt"
chmod 700 "$T/rt"
rt=$T/rt
before_desktop=$(desktop)
echo "$before_desktop"

# ---- the stand-in session -----------------------------------------------------
printf '%s\n' "output HEADLESS-1 mode 1280x720" "xwayland disable" > "$T/sway.conf"
env -i HOME="$T" PATH="$jail_path" XDG_RUNTIME_DIR="$rt" \
  WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  "$sway" -c "$T/sway.conf" >"$T/sway.log" 2>&1 &
swpid=$!
wd=
for _ in $(seq 100); do
  for s in "$rt"/wayland-[0-9]*; do [ -S "$s" ] && { wd=$s; break 2; }; done
  sleep 0.1
done
[ -n "$wd" ] || { echo "the stand-in sway did not start:"; tail "$T/sway.log"; kill "$swpid"; exit 2; }
ipc=$(ls "$rt"/sway-ipc.*.sock 2>/dev/null | head -1)
outside=$(WAYLAND_DISPLAY=$wd XDG_RUNTIME_DIR=$rt "$winfo" 2>&1)

# the daemon sees the stand-in as its desktop
WAYLAND_DISPLAY=$wd WOK_JAIL_PATH=$jail_path \
  "$wok" daemon --socket "$T/s" --state "$T/state" 2>"$T/daemon.log" &
dpid=$!
# stop our daemon and stand-in sway (by PID) however the script ends
trap 'kill "$dpid" "$swpid" 2>/dev/null' EXIT
for _ in $(seq 50); do [ -S "$T/s" ] && break; sleep 0.1; done
W() { (cd "$T" && WOK_SOCKET=$T/s "$wok" "$@"); }

# ---- what the job sees ----------------------------------------------------------
inside=$(W run --show -- wayland-info 2>&1)
check "wayland-info runs in a --show job" grep -q 'interface: .wl_compositor' <<<"$inside"
check "the job gets xdg_wm_base (it can make windows)" grep -q "interface: 'xdg_wm_base'" <<<"$inside"
names() { grep -o "interface: '[A-Za-z0-9_]*'" | sed "s/interface: '\(.*\)'/\1/" | sort -u; }
echo "      hidden by the security context: $(comm -23 <(names <<<"$outside") <(names <<<"$inside") | tr '\n' ' ')"
check "the stand-in offers screencopy outside" grep -q "interface: 'zwlr_screencopy_manager_v1'" <<<"$outside"
check "  … but not to the job" bash -c "! grep -q \"interface: 'zwlr_screencopy_manager_v1'\" <<<\"\$1\"" _ "$inside"
W run --show -- sh -c 'grim $WOK_OUT/stolen.png' >"$T/grim.log" 2>&1
rc=$?
check "grim fails in a --show job (rc $rc)" [ $rc -ne 0 ]
check "no screenshot of the session came back" [ -z "$(find "$T/wok-out" -name stolen.png 2>/dev/null)" ]
env_out=$(W run --show -- sh -c 'echo "[$WAYLAND_DISPLAY] [${DISPLAY:-}] [${SWAYSOCK:-}]"; ls -A /run/wok' 2>&1)
check "the job's display is wayland-0, no X11, no sway IPC ($(tr '\n' ' ' <<<"$env_out"))" \
  [ "$env_out" = $'[wayland-0] [] []\nwayland-0' ]
check "the session's own socket is invisible" bash -c "! (cd '$T' && WOK_SOCKET='$T/s' '$wok' run --show -- test -e '$wd')"
check "the session's IPC socket is invisible" bash -c "! (cd '$T' && WOK_SOCKET='$T/s' '$wok' run --show -- test -e '$ipc')"

# ---- known and accepted (Georg, 2026-11-15): the clipboard reaches a focused job ----
# Reported, not a PASS/FAIL: a focused job may read (and replace) it.
secret=wok-clip-$$-$RANDOM
WAYLAND_DISPLAY=$wd XDG_RUNTIME_DIR=$rt "$wlcopy" "$secret" 2>/dev/null &
cpid=$!
sleep 0.5
got=$(W run --show --timeout 10 -- wl-paste --no-newline 2>/dev/null)
kill "$cpid" 2>/dev/null; wait "$cpid" 2>/dev/null
if [ "$got" = "$secret" ]; then
  echo "KNOWN  a --show job read the session's clipboard (accepted: a focused job may)"
else
  echo "KNOWN  inconclusive here: the stand-in seat has no keyboard, so no window gets focus and no"
  echo "       selection is offered (Wayland sends it to the focused client). On a real seat it is."
fi

# ---- a window on the session ------------------------------------------------
(cd "$T" && WOK_SOCKET=$T/s exec "$wok" run --show --timeout 6 -- weston-flower) >"$T/win.log" 2>&1 &
wpid=$!
tree=
for _ in $(seq 40); do
  tree=$("$swaymsg" -s "$ipc" -t get_tree 2>/dev/null)
  grep -q '"sandbox_engine": "wok"' <<<"$tree" && break
  sleep 0.1
done
check "the job's window is on the session, sandbox engine wok" grep -q '"sandbox_engine": "wok"' <<<"$tree"
check "  … app id wok" grep -q '"sandbox_app_id": "wok"' <<<"$tree"
sid=$(grep -o '"sandbox_instance_id": "[^"]*"' <<<"$tree" | head -1)
check "  … instance id the job's id ($sid)" grep -qE '"sandbox_instance_id": "[0-9]+-[0-9a-f]{8}"' <<<"$sid"

# ---- lanes: the show job above runs; headless work doesn't wait --------------
t0=$(date +%s.%N)
W run --no-display -- true 2>"$T/lane.log"
rc=$?
took=$(awk -v a="$t0" -v b="$(date +%s.%N)" 'BEGIN { printf "%.1f", b - a }')
check "a headless job runs beside a show job (rc $rc, ${took}s)" \
  bash -c "[ $rc -eq 0 ] && awk -v t=$took 'BEGIN { exit !(t < 2) }' && ! grep -q 'queued behind' '$T/lane.log'"
st=$(W status)
check "status shows the show job in its lane (id only)" grep -qE '^show: [0-9]+-[0-9a-f]{8} \([0-9]+s\)$' <<<"$st"
wait "$wpid"
rc=$?
check "the window job ends at its timeout (rc $rc)" [ $rc -eq 124 ]
tree=$("$swaymsg" -s "$ipc" -t get_tree 2>/dev/null)
check "its window is gone" bash -c "! grep -q '\"sandbox_engine\": \"wok\"' <<<\"\$1\"" _ "$tree"
check "the job dirs (and their listeners) are gone" [ -z "$(ls -A "$T/state/jobs")" ]

# ---- the session goes away -------------------------------------------------------
kill "$swpid"; wait "$swpid" 2>/dev/null
W run --show -- true >/dev/null 2>"$T/gone.log"
rc=$?
check "no session: --show exits 125 with a reason (rc $rc)" bash -c "[ $rc -eq 125 ] && grep -q 'desktop' '$T/gone.log'"

kill "$dpid"; wait "$dpid" 2>/dev/null

after_desktop=$(desktop)
check "the desktop session is untouched" [ "$before_desktop" = "$after_desktop" ]
[ "$before_desktop" = "$after_desktop" ] || diff <(echo "$before_desktop") <(echo "$after_desktop")
check "the stand-in sway is gone" bash -c "! kill -0 $swpid 2>/dev/null"

echo
echo "$pass passed, $fail failed (logs in $T)"
[ "$fail" -eq 0 ]
