#!/usr/bin/env bash
# tests/desktop.sh — `wok run --show` on the real desktop session (Georg's
# gate: it opens a window there). Run it from a terminal in that session.
#
#   just show-desktop
#
# Starts its own daemon (scratch socket and state under target/desktop-test,
# the built-in jail, this terminal's WAYLAND_DISPLAY as its desktop) and stops
# it by its PID. Checks what it can by itself (grim
# refused, the session's processes and sockets unchanged); the window you
# answer: vkcube should spin on your sway for 15 s, on the RTX 2080, and close.
# NOTE (accepted, 2026-11-15): while its window has focus, a --show job gets
# your keystrokes and can read and replace the clipboard. Don't type or copy
# anything secret meanwhile.

set -u
here=$(cd "$(dirname "$0")/.." && pwd)
wok=$here/target/debug/wok
T=$here/target/desktop-test
uid=$(id -u)

pass=0 fail=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf 'FAIL  %s\n' "$1"; }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

[ -x "$wok" ] || { echo "build first: cargo build"; exit 2; }
[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "run this from a terminal in the sway session"; exit 2; }

echo "building the tools…"
tools=$(cd "$here" && nix build --no-link --print-out-paths --inputs-from . \
  nixpkgs#grim nixpkgs#coreutils nixpkgs#bash nixpkgs#wayland-utils nixpkgs#vulkan-tools) || exit 2
jail_path=$(printf '%s/bin:' $tools)
jail_path=${jail_path%:}
winfo=$(printf '%s\n' $tools | grep wayland-utils | head -1)/bin/wayland-info

desktop() {
  for p in sway Xwayland pipewire wireplumber; do
    printf '%s: %s\n' "$p" "$(pgrep -u "$uid" -x "$p" | sort | tr '\n' ' ')"
  done
  printf 'sockets: %s\n' "$(ls /run/user/$uid | grep -E '^wayland-' | tr '\n' ' ')"
  printf 'x11: %s\n' "$(ls /tmp/.X11-unix 2>/dev/null | tr '\n' ' ')"
}
before=$(desktop)
echo "$before"

rm -rf "$T"
mkdir -p "$T"
WOK_JAIL_PATH=$jail_path "$wok" daemon --socket "$T/s" --state "$T/state" 2>"$T/daemon.log" &
dpid=$!
# stop our daemon (by its PID) however the script ends, Ctrl-C included
trap 'kill "$dpid" 2>/dev/null' EXIT
for _ in $(seq 50); do [ -S "$T/s" ] && break; sleep 0.1; done
W() { (cd "$T" && WOK_SOCKET=$T/s "$wok" "$@"); }

outside=$("$winfo" 2>&1)
inside=$(W run --show -- wayland-info 2>&1)
check "a --show job reaches the session" grep -q "interface: 'xdg_wm_base'" <<<"$inside"
names() { grep -o "interface: '[A-Za-z0-9_]*'" | sed "s/interface: '\(.*\)'/\1/" | sort -u; }
echo "      hidden from the job on your session: $(comm -23 <(names <<<"$outside") <(names <<<"$inside") | tr '\n' ' ')"
W run --show -- sh -c 'grim $WOK_OUT/stolen.png' >/dev/null 2>&1
check "grim can't capture the session from a job" [ -z "$(find "$T/wok-out" -name stolen.png 2>/dev/null)" ]

echo
echo ">>> vkcube now opens on your session for 15 s (sandboxed: try the window, then let it close)."
W run --show --timeout 15 -- vkcube >"$T/vkcube.log" 2>&1
rc=$?
check "vkcube ran until its timeout (rc $rc, 124 expected; log $T/vkcube.log)" [ $rc -eq 124 ]
grep -iE 'gpu|selected|nvidia|rtx' "$T/vkcube.log" | head -3 | sed 's/^/      /'

kill "$dpid"; wait "$dpid" 2>/dev/null
sleep 1
after=$(desktop)
check "the desktop session's processes and sockets are unchanged" [ "$before" = "$after" ]
[ "$before" = "$after" ] || diff <(echo "$before") <(echo "$after")

echo
echo "$pass passed, $fail failed (daemon log: $T/daemon.log)"
echo "Your part: did the cube spin in a window on your sway, take input focus, and close by itself?"
[ "$fail" -eq 0 ]
