//! The daemon and client end to end, driven through the binary, with a fake
//! jail (tests/fake-jail.sh) so it runs anywhere. The real jail and the GPU
//! are tests/jail.sh and tests/gpu.sh, on the host.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WOK: &str = env!("CARGO_BIN_EXE_wok");

struct Daemon {
    child: Child,
    sock: PathBuf,
    state: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Daemon {
        Daemon::start_with(None)
    }

    /// `desktop`: the daemon's WAYLAND_DISPLAY. Never the real session's:
    /// a test daemon doesn't inherit it.
    fn start_with(desktop: Option<&Path>) -> Daemon {
        Daemon::start_in(tempfile::tempdir().unwrap(), desktop)
    }

    /// In `dir`, which may hold an earlier daemon's state.
    fn start_in(dir: tempfile::TempDir, desktop: Option<&Path>) -> Daemon {
        let sock = dir.path().join("wok.sock");
        let state = dir.path().join("state");
        let jail = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake-jail.sh");
        let child = Command::new(WOK)
            .args(["daemon", "--socket"])
            .arg(&sock)
            .arg("--state")
            .arg(&state)
            .arg("--jail")
            .arg(jail)
            .env_remove("WAYLAND_DISPLAY")
            .envs(desktop.map(|d| ("WAYLAND_DISPLAY", d)))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sock.exists() {
            assert!(Instant::now() < deadline, "the daemon never made its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        Daemon {
            child,
            sock,
            state,
            _dir: dir,
        }
    }

    fn wok(&self, args: &[&str]) -> Command {
        let mut c = Command::new(WOK);
        c.args(args).env("WOK_SOCKET", &self.sock);
        c
    }

    fn run(&self, args: &[&str], cwd: &Path) -> Output {
        self.wok(args).current_dir(cwd).output().unwrap()
    }

    fn job_dirs(&self) -> usize {
        fs::read_dir(self.state.join("jobs")).unwrap().count()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill(); // by its own pid
        let _ = self.child.wait();
    }
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn output_streams_and_the_exit_code_passes_through() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let o = d.run(
        &[
            "run",
            "--no-display",
            "--",
            "sh",
            "-c",
            "echo out; echo err >&2; exit 3",
        ],
        t.path(),
    );
    assert_eq!(o.status.code(), Some(3), "{}", s(&o.stderr));
    assert_eq!(s(&o.stdout), "out\n");
    assert!(s(&o.stderr).contains("err\n"));
    // nothing came back, so no wok-out dir is left behind
    assert!(!t.path().join("wok-out").exists());
    assert_eq!(d.job_dirs(), 0);
}

#[test]
fn argv_is_not_a_shell_line() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let o = d.run(
        &["run", "--no-display", "--", "echo", "$HOME; rm -rf /", "'q'"],
        t.path(),
    );
    assert_eq!(s(&o.stdout), "$HOME; rm -rf / 'q'\n");
}

#[test]
fn files_go_in_and_out_comes_back_without_links() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let input = t.path().join("in");
    fs::create_dir_all(input.join("assets")).unwrap();
    fs::write(input.join("assets/level.txt"), "level 1").unwrap();
    let big: Vec<u8> = (0..5_000_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(input.join("big.bin"), &big).unwrap();
    let secret = t.path().join("secret");
    fs::write(&secret, "token").unwrap();

    let script = format!(
        "cat assets/level.txt > $WOK_OUT/seen.txt; cp big.bin $WOK_OUT/; mkdir $WOK_OUT/sub; \
         echo deep > $WOK_OUT/sub/f; ln -s {} $WOK_OUT/stolen; ln -s /etc $WOK_OUT/sub/etc; echo $GREETING",
        secret.display()
    );
    let out = t.path().join("res");
    let o = d.run(
        &[
            "run",
            "--no-display",
            "--in",
            input.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "-e",
            "GREETING=hi",
            "--",
            "sh",
            "-c",
            &script,
        ],
        t.path(),
    );
    assert_eq!(o.status.code(), Some(0), "{}", s(&o.stderr));
    assert_eq!(s(&o.stdout), "hi\n", "{}", s(&o.stderr));
    assert_eq!(fs::read_to_string(out.join("seen.txt")).unwrap(), "level 1");
    assert_eq!(fs::read(out.join("big.bin")).unwrap(), big);
    assert_eq!(fs::read_to_string(out.join("sub/f")).unwrap(), "deep\n");
    // links a job made never come back, and are named
    assert!(fs::symlink_metadata(out.join("stolen")).is_err());
    assert!(fs::symlink_metadata(out.join("sub/etc")).is_err());
    let err = s(&o.stderr);
    assert!(err.contains("left out of the results: stolen"), "{err}");
    assert!(err.contains("left out of the results: sub/etc"), "{err}");
    assert_eq!(d.job_dirs(), 0);
}

#[test]
fn results_default_to_wok_out_by_id() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let o = d.run(
        &["run", "--no-display", "--", "sh", "-c", "echo x > $WOK_OUT/r"],
        t.path(),
    );
    assert!(o.status.success(), "{}", s(&o.stderr));
    let dirs: Vec<_> = fs::read_dir(t.path().join("wok-out"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1);
    assert_eq!(fs::read_to_string(dirs[0].join("r")).unwrap(), "x\n");
    assert!(s(&o.stderr).contains("results in wok-out/"));
}

#[test]
fn a_timeout_kills_the_job() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let o = d.run(
        &["run", "--no-display", "--timeout", "1", "--", "sleep", "30"],
        t.path(),
    );
    assert_eq!(o.status.code(), Some(124), "{}", s(&o.stderr));
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(s(&o.stderr).contains("timed out after 1s"));
}

#[test]
fn a_job_that_ignores_term_is_killed() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let start = Instant::now();
    let o = d.run(
        &[
            "run",
            "--no-display",
            "--timeout",
            "1",
            "--",
            "sh",
            "-c",
            "trap '' TERM; sleep 30",
        ],
        t.path(),
    );
    assert_eq!(o.status.code(), Some(124), "{}", s(&o.stderr));
    assert!(start.elapsed() < Duration::from_secs(15));
}

#[test]
fn one_job_at_a_time_in_order() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let job = "date +%s.%N; sleep 1; date +%s.%N";
    let first = d
        .wok(&["run", "--no-display", "--", "sh", "-c", job])
        .current_dir(t.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let second = d
        .wok(&["run", "--no-display", "--", "sh", "-c", job])
        .current_dir(t.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (a, b) = (
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    );
    let times = |o: &Output| -> Vec<f64> { s(&o.stdout).lines().map(|l| l.parse().unwrap()).collect() };
    let (ta, tb) = (times(&a), times(&b));
    assert!(
        tb[0] >= ta[1],
        "the second job started before the first ended: {ta:?} {tb:?}"
    );
    assert!(s(&b.stderr).contains("queued behind 1"), "{}", s(&b.stderr));
}

#[test]
fn a_client_that_leaves_takes_its_job_with_it() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let mut c = d
        .wok(&["run", "--no-display", "--", "sleep", "60"])
        .current_dir(t.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let o = d.run(&["status"], t.path());
        if !s(&o.stdout).starts_with("running: nothing") {
            break;
        }
        assert!(Instant::now() < deadline, "the job never showed as running");
        std::thread::sleep(Duration::from_millis(50));
    }
    c.kill().unwrap(); // by its own pid
    c.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let o = d.run(&["status"], t.path());
        if s(&o.stdout).contains("running: nothing") && d.job_dirs() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the job outlived its client: {}",
            s(&o.stdout)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn status_when_idle() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let o = d.run(&["status"], t.path());
    assert!(o.status.success());
    assert_eq!(
        s(&o.stdout),
        "running: nothing\nqueued: 0\nshow: nothing\nshow queued: 0\n"
    );
}

#[test]
fn bad_requests_are_refused_with_a_reason() {
    let d = Daemon::start();
    let t = tempfile::tempdir().unwrap();
    let o = d.run(&["run", "--timeout", "0", "--", "true"], t.path());
    assert_eq!(o.status.code(), Some(125));
    assert!(s(&o.stderr).contains("timeout must be"), "{}", s(&o.stderr));
    let o = d.run(&["run", "-e", "PATH=/evil", "--", "true"], t.path());
    assert_eq!(o.status.code(), Some(125));
    assert!(s(&o.stderr).contains("PATH is set by wok"), "{}", s(&o.stderr));
}

#[test]
fn no_daemon_is_a_clear_error() {
    let t = tempfile::tempdir().unwrap();
    // nothing at the path
    let o = Command::new(WOK)
        .args(["run", "--", "true"])
        .env("WOK_SOCKET", t.path().join("none.sock"))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(69));
    assert!(s(&o.stderr).contains("no wok daemon at"), "{}", s(&o.stderr));
    // a listener that closes at once, as bento's shim does with no daemon behind it
    let sock = t.path().join("shim.sock");
    let l = UnixListener::bind(&sock).unwrap();
    let shim = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut c, _) = l.accept().unwrap();
            let mut b = [0u8; 16];
            let _ = c.read(&mut b);
        }
    });
    for args in [&["run", "--", "true"][..], &["status"][..]] {
        let o = Command::new(WOK)
            .args(args)
            .env("WOK_SOCKET", &sock)
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(69), "{args:?}: {}", s(&o.stderr));
        assert!(s(&o.stderr).contains("no wok daemon"), "{}", s(&o.stderr));
    }
    shim.join().unwrap();
}

#[test]
fn one_daemon_per_state_dir() {
    let d = Daemon::start();
    let other = d.state.parent().unwrap().join("other.sock");
    let o = Command::new(WOK)
        .args(["daemon", "--socket"])
        .arg(&other)
        .arg("--state")
        .arg(&d.state)
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(s(&o.stderr).contains("another wok daemon"), "{}", s(&o.stderr));
}

#[test]
fn an_upload_that_tries_to_escape_is_refused() {
    // a hand-made client: a file entry with a path out of the job dir
    let d = Daemon::start();
    let mut c = std::os::unix::net::UnixStream::connect(&d.sock).unwrap();
    let frame = |c: &mut std::os::unix::net::UnixStream, tag: u8, p: &[u8]| {
        c.write_all(&((p.len() + 1) as u32).to_be_bytes()).unwrap();
        c.write_all(&[tag]).unwrap();
        c.write_all(p).unwrap();
    };
    frame(
        &mut c,
        b'J',
        br#"{"t":"run","v":2,"argv":["true"],"timeout_s":5,"display":false}"#,
    );
    frame(
        &mut c,
        b'J',
        br#"{"t":"file","path":"../../escaped","size":1,"exec":false}"#,
    );
    frame(&mut c, b'D', b"x");
    frame(&mut c, b'J', br#"{"t":"end"}"#);
    let mut reply = Vec::new();
    c.read_to_end(&mut reply).unwrap();
    assert!(s(&reply).contains("bad path"), "{}", s(&reply));
    assert!(!d.state.join("escaped").exists() && !d.state.parent().unwrap().join("escaped").exists());
    assert_eq!(d.job_dirs(), 0);
}

/// One client of the stand-in compositor: `globals` announced, syncs
/// answered, instance ids recorded (the fds it is handed are dropped).
fn serve(
    mut c: std::os::unix::net::UnixStream,
    globals: &'static [&'static str],
    ids: Arc<Mutex<Vec<String>>>,
) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = c.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        while buf.len() >= 8 {
            let obj = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
            let w = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
            let size = (w >> 16) as usize;
            if buf.len() < size {
                break;
            }
            let body = buf[8..size].to_vec();
            buf.drain(..size);
            let op = w & 0xffff;
            let msg = |obj: u32, op: u32, body: &[u8]| {
                let mut m = obj.to_ne_bytes().to_vec();
                m.extend(((((8 + body.len()) as u32) << 16) | op).to_ne_bytes());
                m.extend(body);
                m
            };
            let out = match (obj, op) {
                (1, 1) => {
                    let mut out = Vec::new();
                    for (i, g) in globals.iter().enumerate() {
                        let mut b = (i as u32 + 1).to_ne_bytes().to_vec();
                        b.extend(((g.len() + 1) as u32).to_ne_bytes());
                        b.extend(g.as_bytes());
                        b.push(0);
                        while !b.len().is_multiple_of(4) {
                            b.push(0);
                        }
                        b.extend(1u32.to_ne_bytes());
                        out.extend(msg(2, 0, &b));
                    }
                    out
                }
                (1, 0) => msg(u32::from_ne_bytes(body[0..4].try_into().unwrap()), 0, &[0; 4]),
                (5, 3) => {
                    let len = u32::from_ne_bytes(body[0..4].try_into().unwrap()) as usize;
                    ids.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&body[4..3 + len]).into());
                    vec![]
                }
                _ => vec![],
            };
            if c.write_all(&out).is_err() {
                return;
            }
        }
    }
}

/// A compositor stand-in for `--show`; records each listener's instance id.
fn fake_compositor(path: &Path) -> Arc<Mutex<Vec<String>>> {
    let l = UnixListener::bind(path).unwrap();
    let ids = Arc::new(Mutex::new(Vec::new()));
    let ids2 = ids.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let ids = ids2.clone();
            std::thread::spawn(move || serve(c, &["wl_compositor", "wp_security_context_manager_v1"], ids));
        }
    });
    ids
}

#[test]
fn show_runs_with_its_own_listener_on_the_desktop() {
    let t = tempfile::tempdir().unwrap();
    let desktop = t.path().join("wayland-7");
    let ids = fake_compositor(&desktop);
    let d = Daemon::start_with(Some(&desktop));
    let o = d.run(
        &[
            "run",
            "--show",
            "--",
            "sh",
            "-c",
            "[ \"$WOK_DISPLAY\" = show ] && [ -S \"$WOK_SHOW_SOCKET\" ] && echo shown",
        ],
        t.path(),
    );
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert_eq!(s(&o.stdout), "shown\n");
    let ids = ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 1, "one listener per job: {ids:?}");
    assert!(ids[0].contains('-'), "tagged with the job id: {ids:?}");
    assert_eq!(d.job_dirs(), 0, "the job's socket and dir are gone");
}

#[test]
fn show_has_its_own_lane() {
    let t = tempfile::tempdir().unwrap();
    let desktop = t.path().join("wayland-7");
    let _ids = fake_compositor(&desktop);
    let d = Daemon::start_with(Some(&desktop));
    // a long show job, then a headless one: the headless one doesn't wait
    let show = d
        .wok(&["run", "--show", "--", "sleep", "5"])
        .current_dir(t.path())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !s(&d.run(&["status"], t.path()).stdout).contains("show: ")
        || s(&d.run(&["status"], t.path()).stdout).contains("show: nothing")
    {
        assert!(Instant::now() < deadline, "the show job never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    let o = d.run(&["run", "--no-display", "--", "true"], t.path());
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "the headless job waited for the show job"
    );
    assert!(!s(&o.stderr).contains("queued behind"), "{}", s(&o.stderr));
    // a second show job waits for the first
    let o = d.run(&["run", "--show", "--", "true"], t.path());
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(s(&o.stderr).contains("queued behind 1"), "{}", s(&o.stderr));
    let o = show.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", s(&o.stderr));
}

#[test]
fn show_without_a_desktop_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    let o = d.run(&["run", "--show", "--", "true"], t.path());
    assert_eq!(o.status.code(), Some(125));
    assert!(s(&o.stderr).contains("no desktop session"), "{}", s(&o.stderr));
    // set, but nothing listens there
    let d = Daemon::start_with(Some(&t.path().join("wayland-gone")));
    let o = d.run(&["run", "--show", "--", "true"], t.path());
    assert_eq!(o.status.code(), Some(125));
    assert!(
        s(&o.stderr).contains("--show: connecting to the desktop"),
        "{}",
        s(&o.stderr)
    );
    assert_eq!(d.job_dirs(), 0);
}

#[test]
fn show_takes_no_screenshot_and_needs_the_display() {
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    for args in [
        ["--show", "--screenshot", "2"].as_slice(),
        &["--show", "--no-display"],
    ] {
        let mut a = vec!["run"];
        a.extend(args);
        a.extend(["--", "true"]);
        let o = d.run(&a, t.path());
        assert_eq!(
            o.status.code(),
            Some(2),
            "clap refuses {args:?}: {}",
            s(&o.stderr)
        );
    }
}

#[test]
fn a_refusal_with_an_upload_still_says_why() {
    // the daemon refuses before reading the upload; the reason must arrive
    // (not a reset read as "no daemon")
    let t = tempfile::tempdir().unwrap();
    let input = t.path().join("in");
    fs::create_dir(&input).unwrap();
    fs::write(input.join("big"), vec![7u8; 8 << 20]).unwrap();
    let d = Daemon::start();
    for args in [
        ["run", "--in", "in", "--show", "--", "true"].as_slice(),
        &["run", "--in", "in", "--timeout", "0", "--", "true"],
    ] {
        let o = d.run(args, t.path());
        assert_eq!(o.status.code(), Some(125), "{args:?}: {}", s(&o.stderr));
        assert!(!s(&o.stderr).contains("no wok daemon"), "{}", s(&o.stderr));
    }
}

#[test]
fn status_shows_no_argv() {
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    let mut c = d
        .wok(&[
            "run",
            "--no-display",
            "--",
            "sh",
            "-c",
            "sleep 3; : secret-token-123",
        ])
        .current_dir(t.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let o = s(&d.run(&["status"], t.path()).stdout);
        assert!(!o.contains("secret-token"), "{o}");
        if !o.starts_with("running: nothing") {
            assert!(o.lines().next().unwrap().ends_with("s)"), "{o}");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(50));
    }
    c.kill().unwrap();
    c.wait().unwrap();
}

#[test]
fn a_client_that_stops_reading_loses_its_job() {
    // a hand-made client: uploads, then never reads; the job floods stdout
    use std::os::unix::net::UnixStream;
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    let mut c = UnixStream::connect(&d.sock).unwrap();
    let frame = |c: &mut UnixStream, p: &[u8]| {
        c.write_all(&((p.len() + 1) as u32).to_be_bytes()).unwrap();
        c.write_all(b"J").unwrap();
        c.write_all(p).unwrap();
    };
    frame(
        &mut c,
        br#"{"t":"run","v":2,"argv":["sh","-c","yes flood"],"timeout_s":600,"display":false}"#,
    );
    frame(&mut c, br#"{"t":"end"}"#);
    // the lane must free up: well within the write timeout plus the kill grace
    let start = Instant::now();
    loop {
        let o = s(&d.run(&["status"], t.path()).stdout);
        if start.elapsed() > Duration::from_secs(3) && o.starts_with("running: nothing") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(40),
            "the stalled client still holds the lane: {o}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(start.elapsed() >= Duration::from_secs(8), "dropped too early");
    drop(c);
}

#[test]
fn a_job_that_locks_its_dirs_is_cleaned_up() {
    // chmod 000 on dirs it made: left out of the results, named; the dir goes
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    let o = d.run(
        &[
            "run",
            "--no-display",
            "--out",
            "res",
            "--",
            "sh",
            "-c",
            "mkdir $WOK_OUT/locked ~/locked; echo hi > $WOK_OUT/locked/f; echo ok > $WOK_OUT/top; chmod 000 $WOK_OUT/locked ~/locked",
        ],
        t.path(),
    );
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert_eq!(fs::read_to_string(t.path().join("res/top")).unwrap(), "ok\n");
    assert!(!t.path().join("res/locked").exists());
    assert!(s(&o.stderr).contains("locked"), "{}", s(&o.stderr));
    let deadline = Instant::now() + Duration::from_secs(5);
    while d.job_dirs() != 0 {
        assert!(
            Instant::now() < deadline,
            "the job dir stays with a chmod 000 dir in it"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_deep_locked_tree_is_cleaned_up() {
    // past PATH_MAX, with a chmod 000 dir and a read-only dir at the bottom
    let t = tempfile::tempdir().unwrap();
    let d = Daemon::start();
    let o = d.run(
        &[
            "run",
            "--no-display",
            "--",
            "sh",
            "-c",
            "cd ~; for i in $(seq 1100); do mkdir dddd; cd dddd; done; echo x > f; mkdir l; chmod 000 l; chmod 500 .",
        ],
        t.path(),
    );
    assert!(o.status.success(), "{}", s(&o.stderr));
    let deadline = Instant::now() + Duration::from_secs(10);
    while d.job_dirs() != 0 {
        assert!(Instant::now() < deadline, "the deep job dir stays");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_daemon_starts_over_leftover_locked_job_dirs() {
    // an earlier daemon died mid-job, leaving read-only and chmod 000 dirs
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let jobs = dir.path().join("state/jobs");
    for (name, mode) in [("1-ro/job/cache", 0o555), ("2-locked/out/x", 0o000)] {
        let p = jobs.join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("f"), "x").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }
    let d = Daemon::start_in(dir, None);
    assert_eq!(d.job_dirs(), 0, "the leftovers stay");
    let t = tempfile::tempdir().unwrap();
    let o = d.run(&["run", "--no-display", "--", "true"], t.path());
    assert!(o.status.success(), "{}", s(&o.stderr));
}
