//! The wire protocol between `wok run` and the daemon. Every message is a
//! frame: a u32 big-endian length, then a tag byte and the payload. Nothing
//! relies on a half-close or on EOF ending a body (phase0.md: a guest SHUT_WR
//! over a channel can truncate buffered data), so a connection is only ever
//! closed whole, after the last frame.
//!
//! A run, as frames:
//!   client: Run, the input tree (Dir/File+Data/Link entries), End
//!   daemon: Queued, Started, Stdout/Stderr chunks as they come, Exit,
//!           the results tree (Dir/File+Data entries, Skipped notes), End
//! An Error frame may come instead of any daemon frame and ends the run.

use std::ffi::OsStr;
use std::io::{self, Read, Write};

use anyhow::{bail, Context as _};
use serde::{Deserialize, Serialize};

use crate::safefs::{self, Dir, Kind};

pub const VERSION: u32 = 1;
/// Largest frame payload accepted; data goes in chunks well below it.
pub const MAX_FRAME: usize = 1 << 20;
const CHUNK: usize = 256 * 1024;

const TAG_MSG: u8 = b'J';
const TAG_DATA: u8 = b'D';
const TAG_STDOUT: u8 = b'O';
const TAG_STDERR: u8 = b'E';

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunReq {
    pub v: u32,
    /// Run as argv inside the jail; no shell sees it (`sh -c` is the caller's choice).
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Notes,
    pub timeout_s: u64,
    /// Start a headless compositor for the job (the default for `wok run`).
    pub display: bool,
    /// Take a screenshot this many seconds after start, then stop the job.
    #[serde(default)]
    pub screenshot_s: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobView {
    pub id: String,
    pub argv: Vec<String>,
    pub running_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Run(RunReq),
    Status { v: u32 },
    StatusReply { running: Option<JobView>, queued: usize },
    Dir { path: String },
    File { path: String, size: u64, exec: bool },
    Link { path: String, target: String },
    Skipped { path: String, why: String },
    End,
    Queued { id: String, ahead: usize },
    Started { id: String },
    Exit(ExitInfo),
    Error { msg: String },
}

#[derive(Debug)]
pub enum Frame {
    Msg(Msg),
    Data(Vec<u8>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    assert!(payload.len() < MAX_FRAME);
    w.write_all(&((payload.len() + 1) as u32).to_be_bytes())?;
    w.write_all(&[tag])?;
    w.write_all(payload)
}

pub fn send(w: &mut impl Write, msg: &Msg) -> io::Result<()> {
    write_frame(w, TAG_MSG, &serde_json::to_vec(msg).expect("messages serialize"))
}

pub fn send_stdout(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_frame(w, TAG_STDOUT, bytes)
}

pub fn send_stderr(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    write_frame(w, TAG_STDERR, bytes)
}

/// The next frame; `None` on EOF before its first byte.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad frame length {len}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let payload = buf.split_off(1);
    Ok(Some(match buf[0] {
        TAG_MSG => Frame::Msg(
            serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        ),
        TAG_DATA => Frame::Data(payload),
        TAG_STDOUT => Frame::Stdout(payload),
        TAG_STDERR => Frame::Stderr(payload),
        t => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad frame tag {t}"),
            ))
        }
    }))
}

/// The next frame, which must be a message; EOF is an error here.
pub fn read_msg(r: &mut impl Read) -> anyhow::Result<Msg> {
    match read_frame(r)? {
        Some(Frame::Msg(m)) => Ok(m),
        Some(f) => bail!("protocol error: expected a message, got {f:?}"),
        None => bail!("connection closed"),
    }
}

/// What a sender does with a symlink it finds.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Links {
    /// Send it as a link entry, never following it (the client's input).
    Send,
    /// Leave it out with a Skipped note (the daemon's results: never follow
    /// links a job made, and don't hand them to the box either).
    Skip,
}

/// Send every entry under `root` (not `End`). Returns the skipped paths.
pub fn send_tree(w: &mut impl Write, root: &Dir, links: Links) -> anyhow::Result<Vec<String>> {
    let mut skipped = Vec::new();
    send_dir(w, root, "", links, 0, &mut skipped)?;
    Ok(skipped)
}

fn send_dir(
    w: &mut impl Write,
    dir: &Dir,
    prefix: &str,
    links: Links,
    depth: usize,
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    for name in dir.names()? {
        let shown = format!("{prefix}{}", name.to_string_lossy());
        let mut skip = |why: &str| -> io::Result<()> {
            send(
                w,
                &Msg::Skipped {
                    path: shown.clone(),
                    why: why.into(),
                },
            )?;
            skipped.push(shown.clone());
            Ok(())
        };
        let Some(utf8) = name.to_str() else {
            skip("name is not UTF-8")?;
            continue;
        };
        let path = format!("{prefix}{utf8}");
        match dir.kind(&name)? {
            Kind::Dir if depth >= 63 => skip("too deep")?,
            Kind::Dir => {
                send(w, &Msg::Dir { path: path.clone() })?;
                let sub = dir.open_dir(&name).with_context(|| format!("opening {path}"))?;
                send_dir(w, &sub, &format!("{path}/"), links, depth + 1, skipped)?;
            }
            Kind::File { .. } => {
                let mut f = dir.open_file(&name).with_context(|| format!("opening {path}"))?;
                let meta = f.metadata()?;
                let size = meta.len();
                let exec = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o100 != 0;
                send(
                    w,
                    &Msg::File {
                        path: path.clone(),
                        size,
                        exec,
                    },
                )?;
                let mut left = size;
                let mut buf = vec![0u8; CHUNK];
                while left > 0 {
                    let want = left.min(CHUNK as u64) as usize;
                    let n = f.read(&mut buf[..want])?;
                    if n == 0 {
                        bail!("{path} shrank while it was being sent");
                    }
                    write_frame(w, TAG_DATA, &buf[..n])?;
                    left -= n as u64;
                }
            }
            Kind::Link { target } => match (links, target.to_str()) {
                (Links::Send, Some(t)) => send(
                    w,
                    &Msg::Link {
                        path,
                        target: t.into(),
                    },
                )?,
                (Links::Send, None) => skip("link target is not UTF-8")?,
                (Links::Skip, _) => skip("a symlink (never followed)")?,
            },
            Kind::Other => skip("not a regular file, dir or link")?,
        }
    }
    Ok(())
}

/// The sender's Skipped notes: (path, why).
pub type Notes = Vec<(String, String)>;

pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: u64,
    pub links: bool,
}

/// Receive entries into `root` up to `End`. Never writes through a link and
/// never replaces an existing file. Returns the sender's Skipped notes.
pub fn recv_tree(r: &mut impl Read, root: &Dir, lim: &Limits) -> anyhow::Result<Notes> {
    let mut bytes = 0u64;
    let mut entries = 0u64;
    let mut skipped = Vec::new();
    loop {
        let msg = read_msg(r)?;
        if !matches!(msg, Msg::End | Msg::Skipped { .. } | Msg::Error { .. }) {
            entries += 1;
            if entries > lim.max_entries {
                bail!("more than {} entries", lim.max_entries);
            }
        }
        match msg {
            Msg::End => return Ok(skipped),
            Msg::Error { msg } => bail!("{msg}"),
            Msg::Skipped { path, why } => skipped.push((path, why)),
            Msg::Dir { path } => {
                let parts = safefs::split_rel(&path)?;
                let (parent, last) = safefs::walk_parent(root, &parts)?;
                parent
                    .open_or_mkdir(last)
                    .with_context(|| format!("creating dir {path}"))?;
            }
            Msg::Link { path, target } => {
                if !lim.links {
                    bail!("refusing the link {path}");
                }
                let parts = safefs::split_rel(&path)?;
                let (parent, last) = safefs::walk_parent(root, &parts)?;
                parent
                    .symlink(OsStr::new(&target), last)
                    .with_context(|| format!("creating link {path}"))?;
            }
            Msg::File { path, size, exec } => {
                bytes = bytes.saturating_add(size);
                if bytes > lim.max_bytes {
                    bail!("more than {} bytes", lim.max_bytes);
                }
                let parts = safefs::split_rel(&path)?;
                let (parent, last) = safefs::walk_parent(root, &parts)?;
                let mode = if exec { 0o755 } else { 0o644 };
                let mut f = parent
                    .create_file(last, mode)
                    .with_context(|| format!("creating {path}"))?;
                let mut left = size;
                while left > 0 {
                    match read_frame(r)? {
                        Some(Frame::Data(d)) if d.len() as u64 <= left => {
                            f.write_all(&d)?;
                            left -= d.len() as u64;
                        }
                        Some(Frame::Msg(Msg::Error { msg })) => bail!("{msg}"),
                        Some(_) => bail!("protocol error in the data of {path}"),
                        None => bail!("connection closed in the data of {path}"),
                    }
                }
            }
            m => bail!("protocol error: unexpected {m:?} in a file tree"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn lim(links: bool) -> Limits {
        Limits {
            max_bytes: 1 << 30,
            max_entries: 10_000,
            links,
        }
    }

    fn roundtrip(
        src: &std::path::Path,
        dst: &std::path::Path,
        links: Links,
        lim: &Limits,
    ) -> anyhow::Result<(Vec<String>, Notes)> {
        let mut wire = Vec::new();
        let skipped = send_tree(&mut wire, &Dir::open(src)?, links)?;
        send(&mut wire, &Msg::End)?;
        let notes = recv_tree(&mut wire.as_slice(), &Dir::open(dst)?, lim)?;
        Ok((skipped, notes))
    }

    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        let run = Msg::Run(RunReq {
            v: VERSION,
            argv: vec!["vkcube".into()],
            env: vec![("A".into(), "b".into())],
            timeout_s: 5,
            display: true,
            screenshot_s: Some(1.5),
        });
        send(&mut wire, &run).unwrap();
        send_stdout(&mut wire, b"out").unwrap();
        let mut r = wire.as_slice();
        assert!(matches!(read_frame(&mut r).unwrap(), Some(Frame::Msg(m)) if m == run));
        assert!(matches!(read_frame(&mut r).unwrap(), Some(Frame::Stdout(d)) if d == b"out"));
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn a_truncated_or_oversized_frame_is_an_error() {
        let mut wire = Vec::new();
        send(&mut wire, &Msg::End).unwrap();
        wire.pop();
        assert!(read_frame(&mut wire.as_slice()).is_err());
        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(read_frame(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn trees_round_trip_with_exec_bits_and_big_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        fs::create_dir(src.path().join("empty")).unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7) as u8).collect();
        fs::write(src.path().join("a/b/big.bin"), &big).unwrap();
        fs::write(src.path().join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(src.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(src.path().join("zero"), "").unwrap();
        std::os::unix::fs::symlink("a/b/big.bin", src.path().join("link")).unwrap();

        let (skipped, _) = roundtrip(src.path(), dst.path(), Links::Send, &lim(true)).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(fs::read(dst.path().join("a/b/big.bin")).unwrap(), big);
        assert!(dst.path().join("empty").is_dir());
        assert_eq!(fs::read(dst.path().join("zero")).unwrap(), b"");
        let mode = fs::metadata(dst.path().join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
        assert_eq!(
            fs::read_link(dst.path().join("link")).unwrap().to_str(),
            Some("a/b/big.bin")
        );
    }

    #[test]
    fn results_leave_out_links_and_fifos() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        fs::write(secret.path().join("token"), "secret").unwrap();
        fs::write(src.path().join("shot.png"), "png").unwrap();
        std::os::unix::fs::symlink(secret.path().join("token"), src.path().join("token")).unwrap();
        std::os::unix::fs::symlink(secret.path(), src.path().join("dirlink")).unwrap();
        let fifo = std::ffi::CString::new(src.path().join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);

        let (skipped, notes) = roundtrip(src.path(), dst.path(), Links::Skip, &lim(false)).unwrap();
        assert_eq!(skipped, ["dirlink", "fifo", "token"]);
        assert_eq!(notes.len(), 3);
        let names: Vec<_> = fs::read_dir(dst.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["shot.png"]);
    }

    type Hostile = (anyhow::Result<Notes>, tempfile::TempDir, tempfile::TempDir);

    /// A hostile sender: entries that try to leave the root.
    fn hostile(msgs: &[Msg], lim: &Limits) -> Hostile {
        let dst = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut wire = Vec::new();
        for m in msgs {
            let m = match m {
                Msg::Link { path, target } if target == "OUTSIDE" => Msg::Link {
                    path: path.clone(),
                    target: outside.path().to_str().unwrap().into(),
                },
                m => m.clone(),
            };
            send(&mut wire, &m).unwrap();
            if let Msg::File { size, .. } = m {
                write_frame(&mut wire, TAG_DATA, &vec![b'x'; size as usize]).unwrap();
            }
        }
        send(&mut wire, &Msg::End).unwrap();
        let res = recv_tree(&mut wire.as_slice(), &Dir::open(dst.path()).unwrap(), lim);
        (res, dst, outside)
    }

    fn file(path: &str) -> Msg {
        Msg::File {
            path: path.into(),
            size: 3,
            exec: false,
        }
    }

    #[test]
    fn receiving_never_leaves_the_root() {
        for path in ["../x", "/tmp/x", "a/../../x", "./x"] {
            let (res, _, _) = hostile(&[file(path)], &lim(true));
            assert!(res.is_err(), "{path} must be refused");
        }
        // a link, then a file through it
        let link = Msg::Link {
            path: "l".into(),
            target: "OUTSIDE".into(),
        };
        let (res, _, outside) = hostile(&[link.clone(), file("l/x")], &lim(true));
        assert!(res.is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        // a link, then a file of the same name (would write through it)
        let (res, _, outside) = hostile(&[link.clone(), file("l")], &lim(true));
        assert!(res.is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
        // links refused outright when not allowed
        let (res, dst, _) = hostile(&[link], &lim(false));
        assert!(res.is_err());
        assert!(fs::symlink_metadata(dst.path().join("l")).is_err());
        // a file twice: never replaced
        let (res, _, _) = hostile(&[file("x"), file("x")], &lim(true));
        assert!(res.is_err());
    }

    #[test]
    fn receiving_enforces_limits() {
        let small = Limits {
            max_bytes: 5,
            max_entries: 10,
            links: false,
        };
        let (res, _, _) = hostile(&[file("a"), file("b")], &small);
        assert!(res.unwrap_err().to_string().contains("bytes"));
        let few = Limits {
            max_bytes: 100,
            max_entries: 1,
            links: false,
        };
        let (res, _, _) = hostile(&[file("a"), file("b")], &few);
        assert!(res.unwrap_err().to_string().contains("entries"));
    }
}
