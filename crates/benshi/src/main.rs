//! The single benshi binary.
//!
//! One binary, one process. Invoked with arguments it is a client speaking to a
//! running daemon over a local socket. Invoked with none, or with `--no-tray`,
//! it is the daemon.
//!
//! The daemon must run inside the user's interactive session on every platform:
//! from Windows 11, the UWP APIs that SMTC detection depends on are unavailable
//! to a non-interactive session, which rules out running as a system service.

use std::ffi::OsStr;
use std::process::ExitCode;

use benshi_cli::Cli;
use clap::Parser;

/// The flag that asks for a daemon without a tray icon.
const NO_TRAY: &str = "--no-tray";

/// What one invocation of this binary is for.
///
/// Decided here rather than by the argument parser, because the two halves
/// disagree about what a bare `benshi` means: to the client it is a missing
/// subcommand and a usage message, and to a user it is "run the thing".
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    /// Run the daemon.
    Daemon {
        /// What the user asked for, which is not yet what exists.
        tray: bool,
    },
    /// Run the client, which parses the rest of the line itself.
    Client,
}

impl Invocation {
    /// What a command line asks for, program name included.
    fn of<A, S>(arguments: A) -> Self
    where
        A: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut asked = arguments.into_iter().skip(1);

        match (asked.next(), asked.next()) {
            (None, _) => Self::Daemon { tray: true },
            (Some(only), None) if only.as_ref() == NO_TRAY => Self::Daemon { tray: false },
            // Anything else, including `--no-tray` with a command after it.
            // The client gives a contradictory line a better answer than this
            // function could: clap names the argument it did not expect.
            _ => Self::Client,
        }
    }
}

fn main() -> ExitCode {
    match Invocation::of(std::env::args_os()) {
        Invocation::Daemon { tray } => daemon(tray),
        Invocation::Client => benshi_cli::main(Cli::parse()),
    }
}

/// Run the daemon, and hand this thread to the runtime.
///
/// The runtime is built rather than declared with an attribute, because the
/// attribute takes the main thread and does not give it back. A tray has to own
/// the main thread on macOS and on Windows, so the thread is spent here
/// deliberately: when the tray lands it takes this thread and the runtime keeps
/// the workers it already has. Until then the main thread has nothing else to
/// do, and blocking on the runtime is the whole of the difference.
///
/// Gated on the platform whose adapter exists rather than on the family of
/// platforms whose socket exists. macOS is a unix and has no detection adapter,
/// so a daemon there would have a socket, a supervisor, and nothing to watch.
#[cfg(target_os = "linux")]
fn daemon(tray: bool) -> ExitCode {
    if tray {
        // Said rather than passed over. A tray that was asked for and is
        // silently absent looks exactly like a tray that failed to appear, and
        // the two are fixed differently.
        eprintln!("benshi: there is no tray yet, running without one");
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(unbuilt) => {
            eprintln!("benshi: the runtime could not be started: {unbuilt}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(serve())
}

/// Bind the socket, supervise the daemon's tasks, and report what stopped them.
#[cfg(target_os = "linux")]
async fn serve() -> ExitCode {
    use std::sync::Arc;

    use benshi_core::clock::SystemClock;
    use benshi_detect::mpris::{MprisWatcher, POLL_INTERVAL, SOURCE_DEADLINE};

    let socket = benshi_daemon::ipc::socket_path();
    let listener = match benshi_daemon::ipc::bind(&socket) {
        Ok(listener) => Arc::new(listener),
        Err(unbound) => {
            eprintln!("benshi: {} cannot be used: {unbound}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    eprintln!("benshi: listening on {}", socket.display());

    // A factory rather than a watcher: a restart has to reconnect to the
    // session bus, which is the failure it exists to recover from.
    let stopped = benshi_daemon::run(
        || MprisWatcher::connect(SystemClock::new(), SOURCE_DEADLINE),
        listener,
        POLL_INTERVAL,
    )
    .await;

    // Reached only when nothing is left running, which for a daemon is a
    // failure however tidily each task arrived at it.
    for task in &stopped {
        eprintln!("benshi: {} {}", task.name, task.stopped_by);
    }

    ExitCode::FAILURE
}

/// There is no adapter for this platform, so there is no daemon to run.
///
/// Not async, and no runtime is built. A runtime with nothing to watch would
/// start, find one task, and report that it stopped, which says the daemon
/// failed rather than that this platform has no daemon yet.
#[cfg(not(target_os = "linux"))]
fn daemon(_tray: bool) -> ExitCode {
    eprintln!("benshi: this platform has no detection adapter yet");

    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::Invocation;

    #[test]
    fn no_arguments_means_run_the_daemon() {
        assert_eq!(
            Invocation::of(["benshi"]),
            Invocation::Daemon { tray: true }
        );
    }

    #[test]
    fn asking_for_no_tray_is_still_the_daemon() {
        assert_eq!(
            Invocation::of(["benshi", "--no-tray"]),
            Invocation::Daemon { tray: false }
        );
    }

    #[test]
    fn any_other_argument_means_the_client() {
        // The one that matters. `--no-tray` is a daemon flag and `sources` is a
        // client command; reading one as the other would have `benshi sources`
        // start a second daemon, or `benshi --no-tray` try to connect to a
        // daemon that does not exist because it is the thing that was asked
        // for.
        for line in [
            vec!["benshi", "sources"],
            vec!["benshi", "watch"],
            vec!["benshi", "--socket", "/tmp/elsewhere.sock", "sources"],
            vec!["benshi", "record", "--for", "60s", "-o", "trace.jsonl"],
            vec!["benshi", "--help"],
        ] {
            assert_eq!(Invocation::of(line.clone()), Invocation::Client, "{line:?}");
        }
    }

    #[test]
    fn a_daemon_flag_followed_by_a_command_goes_to_the_client() {
        // Contradictory, and the client is where it gets a usable answer: clap
        // names the argument it did not expect. Deciding it here would mean
        // guessing which half of the line the user meant.
        assert_eq!(
            Invocation::of(["benshi", "--no-tray", "sources"]),
            Invocation::Client
        );
    }
}
