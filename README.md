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
wok status
```

- The job is argv, run in a copy of `--in` (`~/job`, in a scratch home
  `/home/wok`); `$WOK_OUT` is `~/out`. No network, no host home, no host env,
  no tty; only its own dirs are writable. Details: `src/jail.sh`.
- `--screenshot S`: the job runs in the background, `grim` takes
  `$WOK_OUT/screenshot.png` after S seconds, then the job is stopped.
- Results never include links or special files a job made (named on stderr).
- Exit status: the job's; 124 timed out (`--timeout`, default 600, max 3600);
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
