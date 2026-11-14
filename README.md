# wok

GPU jobs from any box. A daemon on the GPU machine takes one job at a time
over one unix socket, runs it in a bubblewrap jail with the GPU and a headless
Wayland display of its own (never the desktop's), streams its output back and
returns what it wrote to `$WOK_OUT`. It knows nothing of stews or boxes: files
go in over the socket and results come back over it.

```sh
wok run --in build/ --screenshot 3 -- ./game     # → wok-out/<id>/screenshot.png
wok run --in . --out shots -- sh -c 'render --out $WOK_OUT/frame.png'
wok run --no-display -- vulkaninfo --summary
wok run --in build/ --show -- ./game                 # on the desktop's sway, to play it
wok status
```

- The job is argv, run in a copy of `--in` (`~/job`, in a scratch home
  `/home/wok`); `$WOK_OUT` is `~/out`. No network, no host home, no host env,
  no tty; only its own dirs are writable. Details: `src/jail.sh`.
- `--screenshot S`: the job runs in the background, `grim` takes
  `$WOK_OUT/screenshot.png` after S seconds, then the job is stopped.
- `--show`: the job's windows go to the desktop's sway session instead of a
  headless display, so you can see and play it. It has its own lane: one
  show job at a time, and it never waits for headless jobs nor holds them up.
  The job gets its own listener on the compositor through the
  security-context protocol (sandbox engine `wok`, app id `wok`, instance id
  the job's id; sway criteria `[sandbox_engine="wok"]` match its windows), so
  sway hides its privileged protocols from it: no screencopy, background
  clipboard access (data control), virtual keyboard or pointer, layer shell,
  toplevel lists; the daemon checks that through the job's listener and
  refuses otherwise. **Not hidden:** like any window, while it has keyboard
  focus the job gets the keys typed into it and the current clipboard and
  primary selection, and sway focuses new windows by default. The session's
  own socket stays invisible; still no X11, no sound. No `--screenshot` with
  it. Needs sway 1.9+ and `WAYLAND_DISPLAY` (under systemd, read from the user
  manager's environment for each job).
- Limits: each job runs in a transient systemd user scope (memory 8G, 1024
  tasks by default; tmpfs counts as memory); its dirs on disk are capped at
  8 GiB / 200k entries (and it is stopped when the disk is nearly full); a
  client that stops reading for ~10–20 s loses its job.
- The jail's filesystem is an allowlist: /nix/store, /etc, /usr, /bin,
  /lib64, /sys read-only (host mounts inside /etc and /sys masked), the GPU's
  render and NVIDIA nodes, private tmpfs elsewhere. Nothing else of the host.
- Results never include links or special files a job made (named on stderr).
- Exit status: the job's; 124 timed out (`--timeout`, default 600, max 3600;
  the job gets a TERM, then a KILL 5 s later); 125 also when wok stopped it
  (the reason on stderr);
  69 no daemon; 125 wok's own errors. A client that goes away takes its job
  with it.
- The socket: `$WOK_SOCKET`, else `/run/bento/wok.sock` (bento's `wok`
  channel, in a box), else `~/.local/share/wok/wok.sock` (the daemon's).

## Daemon

`wok daemon` is socket-activated (`homeManagerModules.default`:
`services.wok.enable = true;` gives `wok.socket` and `wok.service`) and exits
after 10 idle minutes. In the foreground: `wok daemon --socket <path>`. State
(job dirs, a lock) is in `~/.local/state/wok/`; job dirs are removed after each
job and at start.

## Protocol

Length-prefixed frames (u32 length, a tag byte, payload): JSON messages, data
chunks, stdout/stderr chunks. No message relies on a half-close or on EOF. See
`src/proto.rs`.

## Tests

- `just test`: unit tests, and the client and daemon end to end with a fake
  jail (anywhere).
- `just jail-test`: the jail's properties with bwrap, no display (on the host).
- `just gpu-test`: the display, the GPU, a screenshot, and that the desktop
  session is untouched (on the GPU machine).
- `just show-test`: `--show` against a private headless sway (pixman, no
  GPU), never the desktop's: privileged globals hidden, the window tagged,
  the lanes.
- `just show-desktop`: `--show` on the real session; opens vkcube there for
  15 s (run it from a terminal in the session).
