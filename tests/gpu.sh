#!/usr/bin/env bash
# tests/gpu.sh — the display and the GPU, through a real daemon and jail, on
# the GPU machine (phase 3's verification, over a host socket). PASS/FAIL per
# check; the desktop session is snapshotted before and compared after.
#
#   just gpu-test
#
# Starts its own daemon (scratch socket and state under target/gpu-test, the
# built-in jail) and stops it by its PID. Tools come from this flake's nixpkgs.

set -u
here=$(cd "$(dirname "$0")/.." && pwd)
wok=$here/target/debug/wok
T=$here/target/gpu-test
uid=$(id -u)

pass=0 fail=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf 'FAIL  %s\n' "$1"; }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }

[ -x "$wok" ] || { echo "build first: cargo build"; exit 2; }

echo "building the tools…"
tools=$(cd "$here" && nix build --no-link --print-out-paths --inputs-from . \
  nixpkgs#sway nixpkgs#grim nixpkgs#coreutils nixpkgs#bash \
  nixpkgs#mesa-demos nixpkgs#vulkan-tools nixpkgs#imagemagick) || exit 2
jail_path=$(printf '%s/bin:' $tools)
jail_path=${jail_path%:}
magick=$(printf '%s\n' $tools | grep imagemagick | head -1)/bin/magick

# ---- the desktop, before ----------------------------------------------------
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
for _ in $(seq 50); do [ -S "$T/s" ] && break; sleep 0.1; done
W() { (cd "$T" && WOK_SOCKET=$T/s "$wok" "$@"); }

# ---- jobs -------------------------------------------------------------------
out=$(W run -- eglinfo -B 2>&1)
check "eglinfo in the headless display sees NVIDIA" grep -qi nvidia <<<"$out"
check "eglinfo runs on the Wayland platform" grep -qi 'wayland platform' <<<"$out"
out=$(W run --no-display -- vulkaninfo --summary 2>&1)
check "vulkaninfo sees the GPU (no display)" grep -qiE 'nvidia|geforce|rtx' <<<"$out"
out=$(W run -- sh -c 'echo "[$WAYLAND_DISPLAY] [${DISPLAY:-}]"' 2>&1)
check "the job's display is its own, no X11 ($out)" grep -qxE '\[wayland-[0-9]+\] \[\]' <<<"$out"

start=$(date +%s.%N)
W run --screenshot 3 --out shot -- vkcube >"$T/vkcube.log" 2>&1
rc=$?
took=$(awk -v a="$start" -v b="$(date +%s.%N)" 'BEGIN { printf "%.1f", b - a }')
check "a screenshot job succeeds (rc $rc, ${took}s)" [ $rc -eq 0 ]
check "the screenshot came back" [ -s "$T/shot/screenshot.png" ]
if [ -s "$T/shot/screenshot.png" ]; then
  mean=$("$magick" "$T/shot/screenshot.png" -format '%[fx:mean]' info: 2>/dev/null || echo 0)
  check "the screenshot is not black (mean $mean)" awk -v m="$mean" 'BEGIN { exit !(m > 0.01) }'
  echo "      $T/shot/screenshot.png"
fi

# the client itself in the background (not a subshell), so $! is its pid
(cd "$T" && WOK_SOCKET=$T/s exec "$wok" run -- sleep 30) >/dev/null 2>&1 &
cpid=$!
sleep 2
kill "$cpid"; wait "$cpid" 2>/dev/null
sleep 2
st=$(W status | head -1)
check "a client that leaves takes its job along ($st)" [ "$st" = "running: nothing" ]

kill "$dpid"; wait "$dpid" 2>/dev/null

# ---- the desktop, after -------------------------------------------------------
after=$(desktop)
check "the desktop session is untouched" [ "$before" = "$after" ]
[ "$before" = "$after" ] || diff <(echo "$before") <(echo "$after")
check "no compositor of ours is left" bash -c "[ -z \"\$(pgrep -u $uid -f '[s]way -c /run/wok/sway.conf')\" ]"

echo
echo "$pass passed, $fail failed (daemon log: $T/daemon.log)"
[ "$fail" -eq 0 ]
