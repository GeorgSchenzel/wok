//! The daemon's Linux backend: everything in the daemon that is Linux or
//! sway or systemd. daemon.rs (queue, lanes, timeout, transfers, job dirs)
//! uses only what is `pub` here, so a backend for another host (a Mac:
//! Seatbelt or nono, no sway) replaces this module and keeps the same items:
//!
//! - `Jail::new(state, script)`, `Jail::spawn(root, req)`: run the job in
//!   `root`'s job/, out/ and home/, with stdout and stderr piped and stdin
//!   null. The child leads its own process group: a TERM to the group lets
//!   every process of the job stop, a KILL ends them all, and the job dies
//!   with the daemon.
//! - `Jail::desktop_missing()`, `Jail::show(root, id)`: `--show`, a handle
//!   that keeps the job's way onto the desktop open while it is alive.
//! - `activation_listener()`: the socket the service manager passes.
//!
//! Here: bwrap through jail.sh, headless sway in the jail, `--show` on the
//! desktop's sway (wayland.rs), PR_SET_PDEATHSIG, systemd's LISTEN_FDS.

mod wayland;

use std::fs;
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::bail;

use crate::proto::RunReq;

pub use wayland::Show;

const JAIL_SCRIPT: &str = include_str!("jail.sh");

pub struct Jail {
    script: PathBuf,
    /// Look the desktop up in the systemd user manager's environment
    /// (`WOK_DESKTOP_ENV=systemd`, set by the unit), not the daemon's own.
    desktop_from_systemd: bool,
}

impl Jail {
    /// `script`: a jail script to use instead of the built-in one (tests).
    pub fn new(state: &Path, script: Option<PathBuf>) -> anyhow::Result<Jail> {
        let script = match script {
            Some(p) => p,
            None => {
                let p = state.join("jail.sh");
                let tmp = state.join("jail.sh.tmp");
                fs::write(&tmp, JAIL_SCRIPT)?;
                fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
                fs::rename(&tmp, &p)?; // never edit a script that may be running
                p
            }
        };
        Ok(Jail {
            script,
            desktop_from_systemd: std::env::var_os("WOK_DESKTOP_ENV").is_some_and(|v| v == "systemd"),
        })
    }

    pub fn spawn(&self, root: &Path, req: &RunReq) -> io::Result<Child> {
        let mut cmd = Command::new("bash");
        cmd.arg(&self.script).arg(root).arg("--").args(&req.argv);
        cmd.env_clear();
        for k in ["PATH", "WOK_BWRAP", "WOK_JAIL_PATH"] {
            if let Some(v) = std::env::var_os(k) {
                cmd.env(k, v);
            }
        }
        let display = match (req.display, req.show) {
            (_, true) => "show",
            (true, false) => "1",
            (false, false) => "0",
        };
        cmd.env("WOK_DISPLAY", display);
        if req.show {
            cmd.env("WOK_SHOW_SOCKET", root.join("wayland-0"));
        }
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

    /// Why `--show` can't work at all here, checked before the upload.
    pub fn desktop_missing(&self) -> Option<String> {
        if wayland::desktop_socket(self.desktop_from_systemd).is_some() {
            return None;
        }
        let from = if self.desktop_from_systemd {
            "the systemd user manager's environment"
        } else {
            "the daemon's environment"
        };
        Some(format!(
            "this wok daemon found no desktop session (WAYLAND_DISPLAY, with XDG_RUNTIME_DIR, not found in {from})"
        ))
    }

    /// The job's own listener on the desktop's sway, made at its turn (the
    /// desktop looked up again: sway may have restarted), gone when dropped.
    pub fn show(&self, root: &Path, id: &str) -> anyhow::Result<Show> {
        let Some(desktop) = wayland::desktop_socket(self.desktop_from_systemd) else {
            bail!("no desktop session (no WAYLAND_DISPLAY)");
        };
        wayland::listen(&desktop, &root.join("wayland-0"), id)
    }
}

/// The socket systemd's `wok.socket` passes as fd 3.
pub fn activation_listener() -> anyhow::Result<UnixListener> {
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
