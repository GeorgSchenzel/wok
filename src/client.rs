//! `wok run` and `wok status`: the client, in a box (over bento's `wok`
//! channel) or on the host (the daemon's own socket).

use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use crate::proto::{self, ExitInfo, Frame, Limits, Links, Msg, RunReq};
use crate::safefs::Dir;

/// wok's own failures (the job's exit code is passed through otherwise).
pub const EXIT_WOK: i32 = 125;
/// No daemon behind the socket.
pub const EXIT_NO_DAEMON: i32 = 69;
/// The job ran out of time (as timeout(1)).
pub const EXIT_TIMEOUT: i32 = 124;

const MAX_RESULT_BYTES: u64 = 8 << 30;
const MAX_RESULT_ENTRIES: u64 = 200_000;

/// `$WOK_SOCKET`, else bento's channel in a box, else the daemon's socket.
pub fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("WOK_SOCKET") {
        return p.into();
    }
    let channel = Path::new("/run/bento/wok.sock");
    if channel.exists() {
        return channel.into();
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    Path::new(&home).join(".local/share/wok/wok.sock")
}

pub struct RunOpts {
    pub input: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub req: RunReq,
}

enum Failure {
    NoDaemon(String),
    Wok(String),
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        Failure::Wok(format!("{e:#}"))
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::Wok(e.to_string())
    }
}

fn report(sock: &Path, f: Failure) -> i32 {
    match f {
        Failure::NoDaemon(why) => {
            eprintln!("wok: no wok daemon at {} ({why})", sock.display());
            EXIT_NO_DAEMON
        }
        Failure::Wok(msg) => {
            eprintln!("wok: {msg}");
            EXIT_WOK
        }
    }
}

fn connect(sock: &Path) -> Result<UnixStream, Failure> {
    UnixStream::connect(sock).map_err(|e| Failure::NoDaemon(e.to_string()))
}

/// The first reply decides whether a daemon is there at all: through bento's
/// shim a connect succeeds even without one, then reads EOF (or a reset).
fn first_reply(r: &mut impl io::Read) -> Result<Frame, Failure> {
    match proto::read_frame(r) {
        Ok(Some(f)) => Ok(f),
        Ok(None) => Err(Failure::NoDaemon("the connection closed before any reply".into())),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
            ) =>
        {
            Err(Failure::NoDaemon(e.to_string()))
        }
        Err(e) => Err(e.into()),
    }
}

pub fn status() -> i32 {
    let sock = socket_path();
    match status_inner(&sock) {
        Ok(()) => 0,
        Err(f) => report(&sock, f),
    }
}

fn status_inner(sock: &Path) -> Result<(), Failure> {
    let conn = connect(sock)?;
    let mut w = BufWriter::new(conn.try_clone()?);
    proto::send(&mut w, &Msg::Status { v: proto::VERSION })
        .and_then(|_| w.flush())
        .map_err(|e| Failure::NoDaemon(e.to_string()))?;
    match first_reply(&mut BufReader::new(conn))? {
        Frame::Msg(Msg::StatusReply {
            running,
            queued,
            show,
            show_queued,
        }) => {
            let job = |j: Option<proto::JobView>| match j {
                Some(j) => format!("{} ({}s)", j.id, j.running_s),
                None => "nothing".into(),
            };
            println!("running: {}", job(running));
            println!("queued: {queued}");
            println!("show: {}", job(show));
            println!("show queued: {show_queued}");
            Ok(())
        }
        Frame::Msg(Msg::Error { msg }) => Err(Failure::Wok(msg)),
        f => Err(Failure::Wok(format!("protocol error: {f:?}"))),
    }
}

pub fn run(opts: RunOpts) -> i32 {
    let sock = socket_path();
    match run_inner(&sock, opts) {
        Ok(code) => code,
        Err(f) => report(&sock, f),
    }
}

fn run_inner(sock: &Path, opts: RunOpts) -> Result<i32, Failure> {
    // check the input before connecting: a bad --in is not a daemon problem
    let input = match &opts.input {
        Some(p) => Some(Dir::open(p).map_err(|e| Failure::Wok(format!("--in {}: {e}", p.display())))?),
        None => None,
    };
    let conn = connect(sock)?;
    let mut w = BufWriter::with_capacity(256 * 1024, conn.try_clone()?);
    let mut r = BufReader::new(conn);

    // the upload; a write failing here (no reply yet) means no daemon, unless
    // the daemon refused early and said why: then that reply wins
    let upload = (|| -> anyhow::Result<()> {
        proto::send(&mut w, &Msg::Run(opts.req.clone()))?;
        if let Some(dir) = &input {
            for p in proto::send_tree(&mut w, dir, Links::Send)? {
                eprintln!("wok: left out of the upload: {p}");
            }
        }
        proto::send(&mut w, &Msg::End)?;
        w.flush()?;
        Ok(())
    })();
    let upload_err = upload.err();

    let first = match first_reply(&mut r) {
        Ok(f) => f,
        Err(Failure::NoDaemon(why)) => {
            let why = upload_err.map(|e| format!("{e:#}")).unwrap_or(why);
            return Err(Failure::NoDaemon(why));
        }
        Err(f) => return Err(f),
    };
    if let (Some(e), false) = (&upload_err, matches!(first, Frame::Msg(Msg::Error { .. }))) {
        return Err(Failure::Wok(format!("uploading: {e:#}")));
    }

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr();
    let mut id = String::new();
    let mut frame = Some(first);
    let exit: ExitInfo = loop {
        let f = match frame.take() {
            Some(f) => f,
            None => proto::read_frame(&mut r)?
                .ok_or_else(|| Failure::Wok("the daemon closed the connection".into()))?,
        };
        match f {
            Frame::Msg(Msg::Queued { id: i, ahead }) => {
                id = i;
                if ahead > 0 {
                    eprintln!("wok: job {id} queued behind {ahead}");
                }
            }
            Frame::Msg(Msg::Started { id: i }) => id = i,
            Frame::Stdout(b) => {
                stdout.write_all(&b)?;
                stdout.flush()?;
            }
            Frame::Stderr(b) => stderr.write_all(&b)?,
            Frame::Msg(Msg::Exit(e)) => break e,
            Frame::Msg(Msg::Error { msg }) => return Err(Failure::Wok(msg)),
            f => return Err(Failure::Wok(format!("protocol error: {f:?}"))),
        }
    };

    receive_results(&mut r, opts.out, &id)?;

    Ok(if exit.timed_out {
        eprintln!("wok: job {id} timed out after {}s", opts.req.timeout_s);
        EXIT_TIMEOUT
    } else if let Some(c) = exit.code {
        c
    } else {
        let sig = exit.signal.unwrap_or(0);
        eprintln!("wok: job {id} killed by signal {sig}");
        128 + sig
    })
}

/// Results go to `--out`, else `wok-out/<id>/`; a dir that would stay empty
/// is not left behind.
fn receive_results(r: &mut impl io::Read, out: Option<PathBuf>, id: &str) -> Result<(), Failure> {
    let (dir, made_parent) = match out {
        Some(d) => (d, None),
        None => {
            let parent = PathBuf::from("wok-out");
            let made = !parent.exists();
            (parent.join(id), made.then_some(parent))
        }
    };
    let existed = dir.exists();
    fs::create_dir_all(&dir).map_err(|e| Failure::Wok(format!("creating {}: {e}", dir.display())))?;
    let lim = Limits {
        max_bytes: MAX_RESULT_BYTES,
        max_entries: MAX_RESULT_ENTRIES,
        links: false,
    };
    let got = proto::recv_tree(r, &Dir::open(&dir)?, &lim);
    let empty = fs::read_dir(&dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if empty && !existed {
        let _ = fs::remove_dir(&dir);
        if let Some(p) = made_parent {
            let _ = fs::remove_dir(p);
        }
    }
    let notes = got.map_err(|e| Failure::Wok(format!("receiving results: {e:#}")))?;
    for (path, why) in notes {
        eprintln!("wok: left out of the results: {path} ({why})");
    }
    if !empty {
        eprintln!("wok: results in {}", dir.display());
    }
    Ok(())
}
