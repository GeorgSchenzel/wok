//! `--show`: a listening socket on the desktop's compositor for one job,
//! through the security-context protocol (wp_security_context_v1, staging).
//! The daemon connects to the desktop's Wayland socket, makes a listener and
//! hands it to the compositor, tagged sandbox engine `wok`, app id `wok`,
//! instance id the job's id. Clients that connect through it are sandboxed:
//! sway hides its privileged globals from them (screencopy, data control,
//! virtual keyboard/pointer, layer shell, foreign toplevel, …), so a job
//! can't capture the screen, list or inject into other windows, or watch the
//! clipboard in the background. The job never sees the desktop's own socket.
//!
//! What it does NOT stop (R4): a job's window is an ordinary client. When it
//! has keyboard focus it gets the keys typed into it and the current
//! clipboard and primary selection (wl_data_device and primary selection stay
//! available to sandboxed clients); sway focuses a new window by default, and
//! xdg_activation lets a client ask for focus again. So a --show job can read
//! what is typed or copied while its window is focused. See work-wok.md,
//! "Plan changes proposed".
//!
//! Hiding is the compositor's policy, so after making the listener the
//! daemon connects through it once and refuses if any privileged global
//! shows up (a compositor that implements the protocol but filters less).
//!
//! Just enough of the Wayland wire protocol, by hand: messages are
//! `object id: u32, (size << 16 | opcode): u32, args`, native endian; strings
//! are a u32 length (with the NUL) and the bytes padded to 4; fds travel as
//! SCM_RIGHTS with the message that takes them.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _};

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const MANAGER_IFACE: &str = "wp_security_context_manager_v1";

/// Globals a sandboxed client must not get (sway's `is_privileged`, plus DRM
/// leases); any of them through the job's listener refuses `--show`.
pub const PRIVILEGED: &[&str] = &[
    "zwlr_screencopy_manager_v1",
    "ext_image_copy_capture_manager_v1",
    "zwlr_export_dmabuf_manager_v1",
    "zwlr_data_control_manager_v1",
    "ext_data_control_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwlr_virtual_pointer_manager_v1",
    "zwlr_layer_shell_v1",
    "zwlr_foreign_toplevel_manager_v1",
    "ext_foreign_toplevel_list_v1",
    "ext_session_lock_manager_v1",
    "zwlr_gamma_control_manager_v1",
    "zwlr_output_manager_v1",
    "zwlr_output_power_manager_v1",
    "wp_security_context_manager_v1",
    "zwp_input_method_manager_v2",
    "zwp_keyboard_shortcuts_inhibit_manager_v1",
    "ext_transient_seat_manager_v1",
    "ext_workspace_manager_v1",
    "wp_drm_lease_device_v1",
];

/// Alive while the job may use the desktop. Dropping it closes the close fd
/// (the compositor stops accepting on the listener) and removes the socket.
pub struct Show {
    _desktop: UnixStream,
    _close: OwnedFd,
    socket: PathBuf,
}

impl Drop for Show {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The desktop's Wayland socket (`WAYLAND_DISPLAY`, relative to
/// `XDG_RUNTIME_DIR` unless absolute): from the systemd user manager's
/// environment as it is now (`from_systemd`: sway imports it there, and may
/// have restarted since the daemon did), else from the daemon's own.
pub fn desktop_socket(from_systemd: bool) -> Option<PathBuf> {
    let (wd, rt) = if from_systemd {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "show-environment"])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('=').map(std::ffi::OsString::from))
        };
        (get("WAYLAND_DISPLAY"), get("XDG_RUNTIME_DIR"))
    } else {
        (
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
    };
    let d = PathBuf::from(wd.filter(|d| !d.is_empty())?);
    if d.is_absolute() {
        return Some(d);
    }
    Some(PathBuf::from(rt?).join(d))
}

/// The globals a client connecting to `socket` is offered.
pub fn globals(socket: &Path) -> anyhow::Result<Vec<String>> {
    let s = UnixStream::connect(socket).with_context(|| format!("connecting to {}", socket.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut c = Conn { s, buf: Vec::new() };
    c.send(DISPLAY, 1, &Args::new().new_id(REGISTRY), &[])?;
    c.send(DISPLAY, 0, &Args::new().new_id(3), &[])?;
    let mut names = Vec::new();
    c.until_done(3, |obj, op, body| {
        if obj == REGISTRY && op == 0 {
            let mut a = Parse(body);
            a.u32()?;
            names.push(a.string()?);
        }
        Ok(())
    })?;
    Ok(names)
}

/// Make `socket` a sandboxed listener on the compositor behind `desktop`.
pub fn listen(desktop: &Path, socket: &Path, instance: &str) -> anyhow::Result<Show> {
    let conn = UnixStream::connect(desktop)
        .with_context(|| format!("connecting to the desktop's compositor at {}", desktop.display()))?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut c = Conn {
        s: conn,
        buf: Vec::new(),
    };

    // the registry; a sync marks the end of the initial globals
    c.send(DISPLAY, 1, &Args::new().new_id(REGISTRY), &[])?;
    c.send(DISPLAY, 0, &Args::new().new_id(3), &[])?;
    let mut manager = None;
    c.until_done(3, |obj, op, body| {
        if obj == REGISTRY && op == 0 {
            let mut a = Parse(body);
            let (name, iface, version) = (a.u32()?, a.string()?, a.u32()?);
            if iface == MANAGER_IFACE {
                manager = Some((name, version));
            }
        }
        Ok(())
    })?;
    let Some((name, _)) = manager else {
        bail!("the desktop's compositor has no {MANAGER_IFACE} (sway 1.9 or later has it); --show needs it");
    };

    let listener = UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    let (close_r, close_w) = pipe()?;
    let show = Show {
        _desktop: c.s.try_clone()?,
        _close: close_w,
        socket: socket.into(),
    };
    const MANAGER: u32 = 4;
    const CTX: u32 = 5;
    c.send(
        REGISTRY,
        0,
        &Args::new().u32(name).string(MANAGER_IFACE).u32(1).new_id(MANAGER),
        &[],
    )?;
    // create_listener(id, listen_fd, close_fd)
    c.send(
        MANAGER,
        1,
        &Args::new().new_id(CTX),
        &[listener.as_raw_fd(), close_r.as_raw_fd()],
    )?;
    c.send(CTX, 1, &Args::new().string("wok"), &[])?; // set_sandbox_engine
    c.send(CTX, 2, &Args::new().string("wok"), &[])?; // set_app_id
    c.send(CTX, 3, &Args::new().string(instance), &[])?; // set_instance_id
    c.send(CTX, 4, &Args::new(), &[])?; // commit
    c.send(DISPLAY, 0, &Args::new().new_id(6), &[])?;
    c.until_done(6, |_, _, _| Ok(()))?;
    // the compositor holds its own copies of the listener and the close fd
    drop(listener);
    drop(close_r);
    // what a job will see through it: nothing privileged, or no --show
    let seen = globals(socket).context("checking the job's listener")?;
    let leaked: Vec<&String> = seen.iter().filter(|g| PRIVILEGED.contains(&g.as_str())).collect();
    if !leaked.is_empty() {
        bail!("the desktop's compositor offers sandboxed clients privileged globals ({leaked:?}); refusing");
    }
    Ok(show)
}

struct Conn {
    s: UnixStream,
    buf: Vec<u8>,
}

impl Conn {
    fn send(&mut self, obj: u32, op: u16, args: &Args, fds: &[RawFd]) -> anyhow::Result<()> {
        let msg = message(obj, op, &args.0);
        sendmsg(self.s.as_raw_fd(), &msg, fds).context("writing to the compositor")
    }

    /// Read events until wl_callback `cb` is done; a wl_display.error fails.
    fn until_done(
        &mut self,
        cb: u32,
        mut each: impl FnMut(u32, u16, &[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        loop {
            while self.buf.len() >= 8 {
                let obj = u32::from_ne_bytes(self.buf[0..4].try_into().unwrap());
                let w = u32::from_ne_bytes(self.buf[4..8].try_into().unwrap());
                let (size, op) = ((w >> 16) as usize, (w & 0xffff) as u16);
                if size < 8 || size % 4 != 0 {
                    bail!("the compositor sent a bad message (size {size})");
                }
                if self.buf.len() < size {
                    break;
                }
                let body: Vec<u8> = self.buf[8..size].to_vec();
                self.buf.drain(..size);
                if obj == DISPLAY && op == 0 {
                    let mut a = Parse(&body);
                    let (o, code, m) = (a.u32()?, a.u32()?, a.string()?);
                    bail!("the compositor refused (object {o}, error {code}): {m}");
                }
                if obj == cb && op == 0 {
                    return Ok(());
                }
                each(obj, op, &body)?;
            }
            let mut chunk = [0u8; 4096];
            let n = self.s.read(&mut chunk).context("reading from the compositor")?;
            if n == 0 {
                bail!("the compositor closed the connection");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[derive(Default)]
struct Args(Vec<u8>);

impl Args {
    fn new() -> Args {
        Args::default()
    }
    fn u32(mut self, v: u32) -> Args {
        self.0.extend_from_slice(&v.to_ne_bytes());
        self
    }
    fn new_id(self, id: u32) -> Args {
        self.u32(id)
    }
    fn string(mut self, s: &str) -> Args {
        let len = s.len() + 1;
        self.0.extend_from_slice(&(len as u32).to_ne_bytes());
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
}

fn message(obj: u32, op: u16, body: &[u8]) -> Vec<u8> {
    let size = 8 + body.len();
    let mut m = Vec::with_capacity(size);
    m.extend_from_slice(&obj.to_ne_bytes());
    m.extend_from_slice(&(((size as u32) << 16) | op as u32).to_ne_bytes());
    m.extend_from_slice(body);
    m
}

struct Parse<'a>(&'a [u8]);

impl Parse<'_> {
    fn u32(&mut self) -> anyhow::Result<u32> {
        if self.0.len() < 4 {
            bail!("a short message from the compositor");
        }
        let v = u32::from_ne_bytes(self.0[..4].try_into().unwrap());
        self.0 = &self.0[4..];
        Ok(v)
    }
    fn string(&mut self) -> anyhow::Result<String> {
        let len = self.u32()? as usize;
        let padded = len.div_ceil(4) * 4;
        if len == 0 || self.0.len() < padded {
            bail!("a bad string from the compositor");
        }
        let s = String::from_utf8_lossy(&self.0[..len - 1]).into_owned();
        self.0 = &self.0[padded..];
        Ok(s)
    }
}

fn sendmsg(fd: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut _,
        iov_len: data.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
    let mut cbuf = vec![0u8; space.max(1)];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        unsafe {
            let cm = libc::CMSG_FIRSTHDR(&msg);
            (*cm).cmsg_level = libc::SOL_SOCKET;
            (*cm).cmsg_type = libc::SCM_RIGHTS;
            (*cm).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cm) as *mut RawFd, fds.len());
        }
    }
    let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != data.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "a short write to the compositor",
        ));
    }
    Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut p = [0; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn strings_are_padded_with_their_nul() {
        assert_eq!(Args::new().string("wok").0, [4, 0, 0, 0, b'w', b'o', b'k', 0]);
        assert_eq!(Args::new().string("woks").0.len(), 4 + 8);
        let mut p = Parse(&Args::new().string("woks").u32(7).0);
        assert_eq!(p.string().unwrap(), "woks");
        assert_eq!(p.u32().unwrap(), 7);
        assert!(p.u32().is_err());
    }

    #[test]
    fn a_message_header_has_size_and_opcode() {
        let m = message(5, 3, &[0; 8]);
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), 5);
        assert_eq!(u32::from_ne_bytes(m[4..8].try_into().unwrap()), (16 << 16) | 3);
    }

    /// recvmsg: bytes and any fds that came with them.
    fn recv_fds(fd: RawFd, buf: &mut [u8]) -> (usize, Vec<RawFd>) {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut _,
            iov_len: buf.len(),
        };
        let mut cbuf = [0u8; 128];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cbuf.len() as _;
        let n = unsafe { libc::recvmsg(fd, &mut msg, 0) };
        let mut fds = Vec::new();
        if n > 0 {
            unsafe {
                let mut cm = libc::CMSG_FIRSTHDR(&msg);
                while !cm.is_null() {
                    if (*cm).cmsg_type == libc::SCM_RIGHTS {
                        let count = ((*cm).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                        let p = libc::CMSG_DATA(cm) as *const RawFd;
                        fds.extend((0..count).map(|k| *p.add(k)));
                    }
                    cm = libc::CMSG_NXTHDR(&msg, cm);
                }
            }
        }
        (n.max(0) as usize, fds)
    }

    /// One client of the stand-in: `globals` announced, syncs answered; on
    /// create_listener, the passed listener is served with `inside`.
    fn serve(
        s: UnixStream,
        globals: &'static [&'static str],
        inside: &'static [&'static str],
        error_on_commit: bool,
    ) -> Vec<(u32, u16)> {
        let mut s = s;
        let mut seen = Vec::new();
        let mut buf = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let (n, fds) = recv_fds(s.as_raw_fd(), &mut chunk);
            if n == 0 {
                return seen;
            }
            if let Some(&lfd) = fds.first() {
                let l = unsafe { UnixListener::from_raw_fd(lfd) };
                std::thread::spawn(move || {
                    for c in l.incoming().flatten() {
                        std::thread::spawn(move || serve(c, inside, &[], false));
                    }
                });
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
                let op = (w & 0xffff) as u16;
                seen.push((obj, op));
                let mut out = Vec::new();
                if obj == DISPLAY && op == 1 {
                    for (i, g) in globals.iter().enumerate() {
                        out.extend(message(
                            REGISTRY,
                            0,
                            &Args::new().u32(i as u32 + 1).string(g).u32(1).0,
                        ));
                    }
                } else if obj == DISPLAY && op == 0 {
                    let cb = u32::from_ne_bytes(body[0..4].try_into().unwrap());
                    out.extend(message(cb, 0, &Args::new().u32(0).0));
                } else if obj == 5 && op == 4 && error_on_commit {
                    out.extend(message(DISPLAY, 0, &Args::new().u32(5).u32(2).string("nested").0));
                }
                if s.write_all(&out).is_err() {
                    return seen;
                }
            }
        }
    }

    type Fake = (
        tempfile::TempDir,
        PathBuf,
        std::thread::JoinHandle<Vec<(u32, u16)>>,
    );

    /// A compositor stand-in: the daemon's connection, then the listener's.
    fn fake(
        globals: &'static [&'static str],
        inside: &'static [&'static str],
        error_on_commit: bool,
    ) -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wayland-9");
        let l = UnixListener::bind(&path).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            serve(s, globals, inside, error_on_commit)
        });
        (dir, path, h)
    }

    #[test]
    fn makes_a_tagged_listener_and_removes_it_after() {
        let (dir, path, h) = fake(&["wl_compositor", MANAGER_IFACE], &["wl_compositor"], false);
        let sock = dir.path().join("job-wayland");
        let show = listen(&path, &sock, "1700000000-abcd").unwrap();
        assert!(sock.exists());
        drop(show);
        assert!(!sock.exists());
        let ops = h.join().unwrap();
        assert_eq!(
            ops,
            [
                (1, 1),
                (1, 0),
                (2, 0),
                (4, 1),
                (5, 1),
                (5, 2),
                (5, 3),
                (5, 4),
                (1, 0)
            ],
            "registry, sync, bind, create_listener, engine, app id, instance, commit, sync"
        );
    }

    #[test]
    fn no_security_context_is_a_refusal() {
        let (dir, path, _h) = fake(&["wl_compositor"], &[], false);
        let e = listen(&path, &dir.path().join("s"), "x").err().unwrap();
        assert!(format!("{e:#}").contains(MANAGER_IFACE), "{e:#}");
        assert!(!dir.path().join("s").exists());
    }

    #[test]
    fn a_protocol_error_is_a_refusal() {
        let (dir, path, _h) = fake(&[MANAGER_IFACE], &[], true);
        let e = listen(&path, &dir.path().join("s"), "x").err().unwrap();
        assert!(format!("{e:#}").contains("nested"), "{e:#}");
        assert!(!dir.path().join("s").exists(), "the socket goes with the failure");
    }

    #[test]
    fn a_compositor_that_doesnt_hide_is_refused() {
        let (dir, path, _h) = fake(
            &[MANAGER_IFACE],
            &["wl_compositor", "zwlr_screencopy_manager_v1"],
            false,
        );
        let e = listen(&path, &dir.path().join("s"), "x").err().unwrap();
        assert!(format!("{e:#}").contains("zwlr_screencopy_manager_v1"), "{e:#}");
        assert!(!dir.path().join("s").exists());
    }

    #[test]
    fn no_desktop_is_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let e = listen(&dir.path().join("none"), &dir.path().join("s"), "x")
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("desktop"), "{e:#}");
    }
}
