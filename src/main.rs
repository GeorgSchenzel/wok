mod client;
mod daemon;
mod proto;
mod safefs;
mod wayland;

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};

/// GPU jobs from any box: files go in, the job runs jailed on the GPU
/// machine with a headless display, its output and /out come back.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a job and wait for it. Exits with the job's status; 124 when it
    /// timed out, 69 when no daemon answers, 125 for wok's own errors.
    ///
    /// The job runs as argv (no shell; use `sh -c '…'` for one) in a copy
    /// of --in, ~/job in a scratch home. Whatever it writes to $WOK_OUT
    /// (~/out) comes back to --out, except links and special files. stdout
    /// and stderr stream live. No network; the GPU and, unless --no-display,
    /// a headless Wayland display (no X11), or with --show the desktop's.
    #[command(verbatim_doc_comment)]
    Run {
        /// Upload this dir as the job's cwd. Default: an empty one.
        #[arg(long = "in", value_name = "DIR")]
        input: Option<PathBuf>,
        /// Where $WOK_OUT lands. Default: wok-out/<job id>/ (only made if non-empty).
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// Seconds before the job is killed (1..=3600).
        #[arg(long, default_value_t = 600, value_name = "SECS")]
        timeout: u64,
        /// Run the job in the background, grim $WOK_OUT/screenshot.png after
        /// this many seconds, then stop it.
        #[arg(long, value_name = "SECS")]
        screenshot: Option<f64>,
        /// No headless compositor (compute only).
        #[arg(long, conflicts_with = "screenshot")]
        no_display: bool,
        /// Run on the desktop's sway session instead of a headless display,
        /// to see and play it. Its own lane: neither waits for headless jobs
        /// nor holds them up. Sandboxed there (no screencopy, clipboard
        /// managers, virtual input); still no X11.
        #[arg(long, conflicts_with_all = ["screenshot", "no_display"])]
        show: bool,
        /// Set an env var for the job (repeatable).
        #[arg(short = 'e', long = "env", value_name = "K=V", value_parser = parse_env)]
        env: Vec<(String, String)>,
        /// The command.
        #[arg(last = true, required = true, value_name = "CMD")]
        argv: Vec<String>,
    },
    /// What the daemon is running and how many jobs wait, per lane.
    Status,
    /// The daemon (on the GPU machine). Without --socket it takes the socket
    /// systemd passes (wok.socket).
    Daemon {
        /// Listen here, in the foreground.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// State dir (job dirs, lock). Default: $WOK_STATE, else ~/.local/state/wok.
        #[arg(long, value_name = "DIR")]
        state: Option<PathBuf>,
        /// Exit after this many idle seconds. Default: 600 under systemd,
        /// never with --socket.
        #[arg(long, value_name = "SECS")]
        idle_exit: Option<u64>,
        /// Use this jail script instead of the built-in one (tests).
        #[arg(long, hide = true)]
        jail: Option<PathBuf>,
    },
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.into(), v.into()))
        .ok_or_else(|| format!("{s:?} is not K=V"))
}

fn main() {
    let code = match Cli::parse().cmd {
        Cmd::Run {
            input,
            out,
            timeout,
            screenshot,
            no_display,
            show,
            env,
            argv,
        } => client::run(client::RunOpts {
            input,
            out,
            req: proto::RunReq {
                v: proto::VERSION,
                argv,
                env,
                timeout_s: timeout,
                display: !no_display,
                screenshot_s: screenshot,
                show,
            },
        }),
        Cmd::Status => client::status(),
        Cmd::Daemon {
            socket,
            state,
            idle_exit,
            jail,
        } => {
            let idle_exit = match (idle_exit, &socket) {
                (Some(0), _) => None,
                (Some(s), _) => Some(Duration::from_secs(s)),
                (None, None) => Some(Duration::from_secs(600)),
                (None, Some(_)) => None,
            };
            let res = daemon::default_state().and_then(|default| {
                daemon::run(daemon::Config {
                    socket,
                    state: state.unwrap_or(default),
                    jail,
                    idle_exit,
                })
            });
            match res {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("wok daemon: {e:#}");
                    1
                }
            }
        }
    };
    std::process::exit(code);
}
