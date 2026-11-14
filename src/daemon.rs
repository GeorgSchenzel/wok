//! `wok daemon`: one socket, one job at a time, FIFO. A connection uploads
//! its files into a private job dir, waits its turn, runs in the jail with its
//! output streamed back live, then gets `/out` back and the job dir is
//! removed. The daemon needs no paths from the caller and no box identity.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _};

use crate::proto::{self, ExitInfo, JobView, Limits, Links, Msg, RunReq};
use crate::safefs::Dir;

const JAIL_SCRIPT: &str = include_str!("jail.sh");

pub const MAX_TIMEOUT_S: u64 = 3600;
/// Input limits per job (the job dir is on the host's disk).
const MAX_INPUT_BYTES: u64 = 4 << 30;
const MAX_INPUT_ENTRIES: u64 = 200_000;
const MAX_ARGV_BYTES: usize = 128 * 1024;
/// A client must keep its upload moving.
const UPLOAD_READ_TIMEOUT: Duration = Duration::from_secs(60);
/// After a timeout or a hangup: TERM, then KILL after this.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Names a job can't set: the jail and its compositor own them.
const RESERVED_ENV: &[&str] = &["HOME", "PATH", "XDG_RUNTIME_DIR", "WAYLAND_DISPLAY", "DISPLAY"];

pub struct Config {
    /// Bind this path; without it, take the socket systemd passes (LISTEN_FDS).
    pub socket: Option<PathBuf>,
    pub state: PathBuf,
    /// A jail script to use instead of the embedded one (tests).
    pub jail: Option<PathBuf>,
    /// Exit after this long without connections or jobs.
    pub idle_exit: Option<Duration>,
}

#[derive(Default)]
struct Queue {
    next_ticket: u64,
    waiting: VecDeque<u64>,
    running: Option<(JobView, Instant)>,
}

struct Shared {
    queue: Mutex<Queue>,
    turn: Condvar,
    state: PathBuf,
    jail: PathBuf,
    connections: AtomicUsize,
    last_active: Mutex<Instant>,
}

pub fn run(cfg: Config) -> anyhow::Result<()> {
    fs::create_dir_all(&cfg.state).with_context(|| format!("creating {}", cfg.state.display()))?;
    fs::set_permissions(&cfg.state, fs::Permissions::from_mode(0o700))?;
    let _lock = lock_state(&cfg.state)?;

    // Jobs of an earlier daemon died with it (PDEATHSIG, --die-with-parent);
    // their dirs are all that's left.
    let jobs = cfg.state.join("jobs");
    if jobs.exists() {
        fs::remove_dir_all(&jobs).context("removing an earlier daemon's job dirs")?;
    }
    fs::create_dir(&jobs)?;

    let jail = match cfg.jail {
        Some(p) => p,
        None => {
            let p = cfg.state.join("jail.sh");
            let tmp = cfg.state.join("jail.sh.tmp");
            fs::write(&tmp, JAIL_SCRIPT)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            fs::rename(&tmp, &p)?; // never edit a script that may be running
            p
        }
    };

    let listener = match &cfg.socket {
        Some(path) => bind(path)?,
        None => systemd_listener()?,
    };

    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        turn: Condvar::new(),
        state: cfg.state.clone(),
        jail,
        connections: AtomicUsize::new(0),
        last_active: Mutex::new(Instant::now()),
    });

    if let Some(idle) = cfg.idle_exit {
        let s = shared.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1).min(idle));
            let busy = s.connections.load(Ordering::SeqCst) > 0;
            if !busy && s.last_active.lock().unwrap().elapsed() >= idle {
                eprintln!("wok: idle, exiting");
                std::process::exit(0);
            }
        });
    }

    eprintln!("wok: daemon ready, state in {}", cfg.state.display());
    for conn in listener.incoming() {
        let conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("wok: accept: {e}");
                continue;
            }
        };
        shared.connections.fetch_add(1, Ordering::SeqCst);
        let s = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(&s, conn) {
                eprintln!("wok: connection: {e:#}");
            }
            *s.last_active.lock().unwrap() = Instant::now();
            s.connections.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// One daemon per state dir: two would run jobs side by side.
fn lock_state(state: &Path) -> anyhow::Result<fs::File> {
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another wok daemon is using {}", state.display());
    }
    Ok(f)
}

fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("a daemon already listens on {}", path.display());
        }
        fs::remove_file(path)?; // stale
    }
    let l = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// The socket systemd's `wok.socket` passes as fd 3.
fn systemd_listener() -> anyhow::Result<UnixListener> {
    let pid_ok = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.parse::<u32>().ok())
        == Some(std::process::id());
    let fds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.parse::<i32>().ok());
    if !pid_ok || fds != Some(1) {
        bail!("no socket from systemd (LISTEN_FDS); run with --socket <path> in the foreground");
    }
    let fd: RawFd = 3;
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

fn handle(s: &Shared, conn: UnixStream) -> anyhow::Result<()> {
    conn.set_read_timeout(Some(UPLOAD_READ_TIMEOUT))?;
    let mut r = io::BufReader::new(conn.try_clone()?);
    let mut w = BufWriter::new(conn.try_clone()?);
    let req = match proto::read_msg(&mut r) {
        Ok(Msg::Run(req)) => req,
        Ok(Msg::Status { .. }) => {
            let q = s.queue.lock().unwrap();
            let running = q.running.as_ref().map(|(v, t)| JobView {
                running_s: t.elapsed().as_secs(),
                ..v.clone()
            });
            proto::send(
                &mut w,
                &Msg::StatusReply {
                    running,
                    queued: q.waiting.len(),
                },
            )?;
            return Ok(w.flush()?);
        }
        Ok(m) => return refuse(&mut w, &format!("unexpected first message {m:?}")),
        Err(e) => return refuse(&mut w, &format!("{e:#}")),
    };
    if let Err(e) = validate(&req) {
        return refuse(&mut w, &format!("{e:#}"));
    }

    let id = new_id();
    let job = JobDir::create(&s.state.join("jobs").join(&id))?;
    let lim = Limits {
        max_bytes: MAX_INPUT_BYTES,
        max_entries: MAX_INPUT_ENTRIES,
        links: true,
    };
    if let Err(e) = proto::recv_tree(&mut r, &Dir::open(&job.root.join("job"))?, &lim) {
        return refuse(&mut w, &format!("receiving the job's files: {e:#}"));
    }
    conn.set_read_timeout(None)?;

    let view = JobView {
        id: id.clone(),
        argv: req.argv.clone(),
        running_s: 0,
    };
    let Some(turn) = wait_turn(s, &mut w, &conn, view)? else {
        eprintln!("wok: job {id}: the client left while queued");
        return Ok(());
    };
    eprintln!("wok: job {id} started: {:?}", req.argv);
    proto::send(&mut w, &Msg::Started { id: id.clone() })?;
    w.flush()?;

    let (exit, client_gone) = run_job(s, &job.root, &req, &mut w, &conn)?;
    drop(turn); // the next job may start while the results go out
    eprintln!(
        "wok: job {id} done: {exit:?}{}",
        if client_gone { ", the client left" } else { "" }
    );
    if client_gone {
        return Ok(());
    }
    proto::send(&mut w, &Msg::Exit(exit))?;
    match proto::send_tree(&mut w, &Dir::open(&job.root.join("out"))?, Links::Skip) {
        Ok(skipped) => {
            for p in skipped {
                eprintln!("wok: job {id}: left out {p}");
            }
            proto::send(&mut w, &Msg::End)?;
        }
        Err(e) => proto::send(
            &mut w,
            &Msg::Error {
                msg: format!("sending results: {e:#}"),
            },
        )?,
    }
    w.flush()?;
    Ok(())
}

fn refuse(w: &mut impl Write, msg: &str) -> anyhow::Result<()> {
    proto::send(w, &Msg::Error { msg: msg.into() })?;
    w.flush()?;
    Ok(())
}

fn validate(req: &RunReq) -> anyhow::Result<()> {
    if req.v != proto::VERSION {
        bail!(
            "protocol version {} (this daemon speaks {})",
            req.v,
            proto::VERSION
        );
    }
    if req.argv.is_empty() || req.argv[0].is_empty() {
        bail!("no command");
    }
    let size: usize = req.argv.iter().map(|a| a.len() + 1).sum::<usize>()
        + req.env.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>();
    if size > MAX_ARGV_BYTES {
        bail!("command and env too long ({size} bytes, at most {MAX_ARGV_BYTES})");
    }
    if req
        .argv
        .iter()
        .chain(req.env.iter().map(|(_, v)| v))
        .any(|a| a.contains('\0'))
    {
        bail!("a NUL byte in the command or env");
    }
    if !(1..=MAX_TIMEOUT_S).contains(&req.timeout_s) {
        bail!("timeout must be 1..={MAX_TIMEOUT_S} seconds");
    }
    for (k, _) in &req.env {
        let ok = !k.is_empty()
            && !k.starts_with(|c: char| c.is_ascii_digit())
            && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok {
            bail!("bad env name {k:?}");
        }
        if RESERVED_ENV.contains(&k.as_str()) || k.starts_with("WOK_") {
            bail!("{k} is set by wok, not by the job");
        }
    }
    if let Some(shot) = req.screenshot_s {
        if !req.display {
            bail!("a screenshot needs the display");
        }
        if !(shot > 0.0 && shot < req.timeout_s as f64) {
            bail!("the screenshot delay must be above 0 and below the timeout");
        }
    }
    Ok(())
}

/// The job's private dir; removed when dropped, whatever happened.
struct JobDir {
    root: PathBuf,
}

impl JobDir {
    fn create(root: &Path) -> anyhow::Result<JobDir> {
        fs::create_dir(root)?;
        let job = JobDir {
            root: root.to_path_buf(),
        };
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        for d in ["job", "out", "home"] {
            fs::create_dir(root.join(d))?;
        }
        Ok(job)
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        // remove_dir_all doesn't follow links (std, since CVE-2022-21658)
        if let Err(e) = fs::remove_dir_all(&self.root) {
            eprintln!("wok: removing {}: {e}", self.root.display());
        }
    }
}

/// Holds the GPU while alive.
struct Turn<'a>(&'a Shared);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.queue.lock().unwrap().running = None;
        self.0.turn.notify_all();
    }
}

/// Queue up, tell the client how many are ahead, and wait. `None` when the
/// client hangs up while waiting.
fn wait_turn<'a>(
    s: &'a Shared,
    w: &mut impl Write,
    conn: &UnixStream,
    view: JobView,
) -> anyhow::Result<Option<Turn<'a>>> {
    let ticket = {
        let mut q = s.queue.lock().unwrap();
        let t = q.next_ticket;
        q.next_ticket += 1;
        q.waiting.push_back(t);
        t
    };
    let leave = || {
        let mut q = s.queue.lock().unwrap();
        q.waiting.retain(|t| *t != ticket);
        s.turn.notify_all();
    };
    let mut told = None;
    loop {
        {
            let mut q = s.queue.lock().unwrap();
            let ahead = q.waiting.iter().position(|t| *t == ticket).unwrap() + q.running.is_some() as usize;
            // told at least once (the client learns the id), then on changes
            if told.is_some() && ahead == 0 {
                q.waiting.pop_front();
                q.running = Some((view, Instant::now()));
                return Ok(Some(Turn(s)));
            }
            if told != Some(ahead) {
                told = Some(ahead);
                drop(q);
                let sent = proto::send(
                    w,
                    &Msg::Queued {
                        id: view.id.clone(),
                        ahead,
                    },
                )
                .and_then(|_| w.flush());
                if sent.is_err() {
                    leave();
                    return Ok(None);
                }
                continue;
            }
            let _ = s.turn.wait_timeout(q, Duration::from_millis(500)).unwrap();
        }
        if hung_up(conn.as_raw_fd()) {
            leave();
            return Ok(None);
        }
    }
}

/// The client sends nothing after its upload, so anything readable on the
/// socket (EOF above all) means it is gone.
fn hung_up(fd: RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    unsafe { libc::poll(&mut p, 1, 0) > 0 }
}

fn spawn_jail(s: &Shared, root: &Path, req: &RunReq) -> io::Result<Child> {
    let mut cmd = Command::new("bash");
    cmd.arg(&s.jail).arg(root).arg("--").args(&req.argv);
    cmd.env_clear();
    for k in ["PATH", "WOK_BWRAP", "WOK_JAIL_PATH"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    cmd.env("WOK_DISPLAY", if req.display { "1" } else { "0" });
    if let Some(shot) = req.screenshot_s {
        cmd.env("WOK_SCREENSHOT", format!("{shot}"));
    }
    for (k, v) in &req.env {
        cmd.env(format!("WOK_JOBENV_{k}"), v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.process_group(0);
    let daemon = std::process::id() as libc::pid_t;
    unsafe {
        cmd.pre_exec(move || {
            // the job dies with this thread's process; bwrap's
            // --die-with-parent takes the jail's pid namespace with it
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != daemon {
                libc::_exit(126);
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Run the job, streaming its output, until it exits. Returns how it ended
/// and whether the client went away (the job is killed then).
fn run_job(
    s: &Shared,
    root: &Path,
    req: &RunReq,
    w: &mut impl Write,
    conn: &UnixStream,
) -> anyhow::Result<(ExitInfo, bool)> {
    let mut child = match spawn_jail(s, root, req) {
        Ok(c) => c,
        Err(e) => bail!("starting the jail: {e}"),
    };
    let pgid = child.id() as libc::pid_t;
    // one type for both pipes
    let mut out = child.stdout.take().map(|p| fs::File::from(OwnedFd::from(p)));
    let mut err = child.stderr.take().map(|p| fs::File::from(OwnedFd::from(p)));
    let deadline = Instant::now() + Duration::from_secs(req.timeout_s);
    let mut killed_at: Option<Instant> = None;
    let mut timed_out = false;
    let mut client_gone = false;
    let mut exited_at: Option<Instant> = None;
    let mut buf = vec![0u8; 64 * 1024];

    let signal = |sig| unsafe { libc::kill(-pgid, sig) };
    loop {
        let mut fds = vec![libc::pollfd {
            fd: conn.as_raw_fd(),
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        }];
        for p in [&out, &err].into_iter().flatten() {
            fds.push(libc::pollfd {
                fd: p.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 200) };

        if fds[0].revents != 0 && !client_gone {
            client_gone = true;
            if killed_at.is_none() {
                signal(libc::SIGTERM);
                killed_at = Some(Instant::now());
            }
        }
        for (pipe, is_out) in [(&mut out, true), (&mut err, false)] {
            let Some(p) = pipe else { continue };
            let ready = fds[1..].iter().any(|f| f.fd == p.as_raw_fd() && f.revents != 0);
            if !ready {
                continue;
            }
            match p.read(&mut buf) {
                Ok(0) | Err(_) => *pipe = None,
                Ok(n) if !client_gone => {
                    let sent = if is_out {
                        proto::send_stdout(w, &buf[..n])
                    } else {
                        proto::send_stderr(w, &buf[..n])
                    };
                    if sent.and_then(|_| w.flush()).is_err() {
                        client_gone = true;
                        if killed_at.is_none() {
                            signal(libc::SIGTERM);
                            killed_at = Some(Instant::now());
                        }
                    }
                }
                Ok(_) => {}
            }
        }

        let now = Instant::now();
        if killed_at.is_none() && now >= deadline {
            timed_out = true;
            signal(libc::SIGTERM);
            killed_at = Some(now);
        }
        if killed_at.is_some_and(|t| now >= t + KILL_GRACE) {
            signal(libc::SIGKILL);
        }
        if exited_at.is_none() && child.try_wait()?.is_some() {
            exited_at = Some(now);
        }
        // done when both pipes are closed, or the jail exited and something
        // it left behind still holds a pipe (it can't outlive the pid ns long)
        let pipes_done = out.is_none() && err.is_none();
        if pipes_done || exited_at.is_some_and(|t| now >= t + Duration::from_secs(2)) {
            break;
        }
    }
    let status = child.wait()?;
    let exit = ExitInfo {
        code: status.code(),
        signal: status.signal(),
        timed_out,
    };
    Ok((exit, client_gone))
}

fn new_id() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut r = [0u8; 2];
    let _ = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut r));
    format!("{secs}-{:02x}{:02x}", r[0], r[1])
}

pub fn default_state() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("WOK_STATE") {
        return Ok(p.into());
    }
    if let Some(p) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(Path::new(&p).join("wok"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".local/state/wok"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> RunReq {
        RunReq {
            v: proto::VERSION,
            argv: vec!["true".into()],
            env: vec![],
            timeout_s: 10,
            display: true,
            screenshot_s: None,
        }
    }

    #[test]
    fn validation() {
        assert!(validate(&req()).is_ok());
        let bad = [
            RunReq {
                argv: vec![],
                ..req()
            },
            RunReq {
                timeout_s: 0,
                ..req()
            },
            RunReq {
                timeout_s: MAX_TIMEOUT_S + 1,
                ..req()
            },
            RunReq {
                env: vec![("PATH".into(), "/x".into())],
                ..req()
            },
            RunReq {
                env: vec![("WOK_OUT".into(), "/x".into())],
                ..req()
            },
            RunReq {
                env: vec![("A=B".into(), "x".into())],
                ..req()
            },
            RunReq {
                env: vec![("1A".into(), "x".into())],
                ..req()
            },
            RunReq {
                screenshot_s: Some(2.0),
                display: false,
                ..req()
            },
            RunReq {
                screenshot_s: Some(10.0),
                ..req()
            },
            RunReq {
                argv: vec!["a\0b".into()],
                ..req()
            },
            RunReq { v: 99, ..req() },
        ];
        for r in bad {
            assert!(validate(&r).is_err(), "{r:?} must be refused");
        }
    }
}
