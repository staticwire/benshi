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
use std::fmt;
use std::io::{self, Write};
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::task::Poll;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

use benshi_cli::Cli;
#[cfg(target_os = "linux")]
use benshi_daemon::voice::Voice;
use clap::Parser;
#[cfg(target_os = "linux")]
use tokio::signal::unix::{Signal, SignalKind, signal};

/// The flag that asks for a daemon without a tray icon.
const NO_TRAY: &str = "--no-tray";

/// How long the process waits to end, at the most, once `serve` has
/// returned: at a signal, or over a failure.
///
/// It waits for a call into the store that is under way, and for its voice
/// to write what it holds, which waits for whoever reads. Where nobody does
/// it would end only when it was killed, since no signal that stops the
/// daemon ends the process by itself once it is taken. What is still under
/// way when the time is up is cut.
#[cfg(target_os = "linux")]
const LAST_WORDS: Duration = Duration::from_secs(5);

/// The code a daemon ends with that a signal stopped.
#[cfg(target_os = "linux")]
const STOPPED: u8 = 0;

/// The code a daemon ends with that could not start, or that has no task
/// left running.
#[cfg(target_os = "linux")]
const FAILED: u8 = 1;

/// What one invocation of this binary is for.
///
/// Decided here rather than by the argument parser, because the two halves
/// disagree about what a bare `benshi` means: to the client it is a missing
/// subcommand and a usage message, and to a user it is "run the thing".
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    /// Run the daemon.
    Daemon {
        /// Whether a tray was asked for.
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

/// Says a line on stderr for a process that has no voice to say it through,
/// and goes on where it cannot be written.
///
/// A daemon says what it says through its voice, which waits for nobody to
/// read it. This is for the line of a process that ends with it.
/// `eprintln!` panics where the write fails, which would end a run that
/// failed with the code of a panic.
fn say(line: fmt::Arguments<'_>) {
    drop(writeln!(io::stderr(), "benshi: {line}"));
}

fn main() -> ExitCode {
    match Invocation::of(std::env::args_os()) {
        Invocation::Daemon { tray } => daemon(tray),
        Invocation::Client => benshi_cli::main(Cli::parse()),
    }
}

/// The signals that stop the daemon, each with the name it is said under.
#[cfg(target_os = "linux")]
const STOPS: &[(SignalKind, &str)] = &[
    (SignalKind::terminate(), "SIGTERM"),
    (SignalKind::interrupt(), "SIGINT"),
    (SignalKind::hangup(), "SIGHUP"),
];

/// Where the system says which signals this process ignores.
#[cfg(target_os = "linux")]
const STATUS: &str = "/proc/self/status";

/// The signals this process ignores: bit `n - 1` is set for signal `n`.
///
/// Read from procfs: the standard library has no call for it.
#[cfg(target_os = "linux")]
fn ignored() -> io::Result<u64> {
    ignored_in(&std::fs::read_to_string(STATUS)?)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no SigIgn line in it"))
}

/// The mask in the `SigIgn` line of a `status` of procfs.
#[cfg(target_os = "linux")]
fn ignored_in(status: &str) -> Option<u64> {
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))?;

    u64::from_str_radix(mask.trim(), 16).ok()
}

/// The signals among [`STOPS`] that stop a daemon started with `ignored`.
#[cfg(target_os = "linux")]
fn stopping(ignored: u64) -> impl Iterator<Item = (SignalKind, &'static str)> {
    STOPS
        .iter()
        .copied()
        .filter(move |(kind, _name)| ignored & (1 << (kind.as_raw_value() - 1)) == 0)
}

/// The signals that stop this daemon, as they are waited for.
#[cfg(target_os = "linux")]
struct Stops(Vec<(Signal, &'static str)>);

#[cfg(target_os = "linux")]
impl Stops {
    /// Takes from here on each signal of [`STOPS`] that `ignored` does not
    /// hold. One that is sent before it is waited for is kept until it is.
    ///
    /// A signal that was ignored as the daemon started stays ignored: `nohup`
    /// starts a process that way, and so does a shell script that runs a job
    /// in the background.
    ///
    /// None of those taken ends the process by itself after this, for as
    /// long as the process runs: the handler tokio installs stays in place
    /// once every `Signal` has gone.
    fn taken(ignored: u64) -> io::Result<Self> {
        stopping(ignored)
            .map(|(kind, name)| Ok((signal(kind)?, name)))
            .collect::<io::Result<_>>()
            .map(Self)
    }

    /// The name of the first of them to come.
    async fn first(&mut self) -> &'static str {
        std::future::poll_fn(|context| {
            for (signal, name) in &mut self.0 {
                if signal.poll_recv(context).is_ready() {
                    return Poll::Ready(*name);
                }
            }

            Poll::Pending
        })
        .await
    }
}

/// What the daemon waits for a signal with, made by the two steps it is
/// handed: `ignored` reads which signals the process ignores, and `taken`
/// takes the others. What either could not do is said through `say`.
///
/// `ignored` is called ahead of `taken`: a signal that is taken is ignored no
/// longer. Where `ignored` cannot say, `taken` is handed every signal as
/// ignored, so that none is taken and one that is ignored stays so.
///
/// # Errors
///
/// Returns the code the daemon ends with where `taken` fails.
#[cfg(target_os = "linux")]
fn waited_for<S>(
    ignored: impl FnOnce() -> io::Result<u64>,
    taken: impl FnOnce(u64) -> io::Result<S>,
    say: impl Fn(fmt::Arguments<'_>),
) -> Result<S, u8> {
    let ignored = ignored().unwrap_or_else(|untold| {
        say(format_args!(
            "{STATUS} does not say which signals are ignored, so none is waited for: {untold}"
        ));
        u64::MAX
    });

    taken(ignored).map_err(|untaken| {
        say(format_args!("cannot wait for a signal: {untaken}"));
        FAILED
    })
}

/// Ends the process with `code` once `patience` has passed, whatever it
/// waits for by then.
#[cfg(target_os = "linux")]
fn gives_up_after(patience: Duration, code: u8) {
    let giving_up = thread::Builder::new()
        .name("last words".to_owned())
        .spawn(move || {
            thread::sleep(patience);
            std::process::exit(i32::from(code));
        });
    // Where the thread cannot be started the process ends once everything
    // it waits for has come.
    drop(giving_up);
}

/// Run the daemon, and hand this thread to the runtime.
///
/// The runtime is built rather than declared with an attribute, because the
/// attribute takes the main thread and does not give it back. A tray has to own
/// the main thread on macOS and on Windows, so the thread is spent here
/// deliberately: a tray would take this thread and the runtime would keep the
/// workers it already has. There is no tray, so the main thread has nothing
/// else to do, and blocking on the runtime is the whole of the difference.
///
/// Gated on the platform whose adapter exists rather than on the family of
/// platforms whose socket exists. macOS is a unix and has no detection adapter,
/// so a daemon there would have a socket, a supervisor, and nothing to watch.
#[cfg(target_os = "linux")]
fn daemon(tray: bool) -> ExitCode {
    // The first thing made and so the last to go: it goes once it has
    // written what the daemon said as it ended. Panics are said through it:
    // one written by the thread that panicked waits where stderr is full and
    // nobody reads, and holds its task for as long.
    let voice = match Voice::of_the_process(io::stderr()) {
        Ok(voice) => voice,
        Err(unstarted) => {
            say(format_args!(
                "thread that writes to stderr could not be started: {unstarted}"
            ));
            return ExitCode::FAILURE;
        }
    };

    if tray {
        // Said rather than passed over. A tray that was asked for and is
        // silently absent looks exactly like a tray that failed to appear, and
        // the two are fixed differently.
        voice.say(format_args!("there is no tray yet, running without one"));
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(unbuilt) => {
            voice.say(format_args!("runtime could not be started: {unbuilt}"));
            return ExitCode::FAILURE;
        }
    };

    let ended = runtime.block_on(serve(&voice));
    // The runtime goes next and the voice after it, and each waits: one for
    // a call into the store that is under way, the other for whoever reads.
    gives_up_after(LAST_WORDS, ended);

    ExitCode::from(ended)
}

/// Bind the socket, open the database and supervise the daemon's tasks, until
/// every one of them has stopped or a signal stops the daemon. The answer is
/// the code the process ends with.
#[cfg(target_os = "linux")]
async fn serve(voice: &Arc<Voice>) -> u8 {
    use std::sync::Mutex;
    use std::time::SystemTime;

    use benshi_core::clock::SystemClock;
    use benshi_daemon::paths::{DATABASE, Role, directory};
    use benshi_daemon::recognition::Recogniser;
    use benshi_detect::mpris::{MprisWatcher, POLL_INTERVAL, SOURCE_DEADLINE};
    use benshi_store::Store;

    // Taken ahead of the socket and the database, so that a signal sent
    // while one is bound and the other opened stops the daemon once its
    // tasks run. A signal sent ahead of this, while the runtime is built, is
    // the system's to deal with, and where it ends the daemon nothing says
    // so.
    let mut stops = match waited_for(ignored, Stops::taken, |line| voice.say(line)) {
        Ok(stops) => stops,
        Err(code) => return code,
    };
    let socket = benshi_daemon::ipc::socket_path();
    // The XDG Base Directory Specification asks for a warning where the
    // runtime directory is replaced.
    if let Some(fallback) = benshi_daemon::ipc::fallback() {
        voice.say(format_args!("{fallback}"));
    }
    // The socket is what says a daemon is running, so it is bound ahead of
    // the database. A second daemon that works out the same socket ends here,
    // and the database of the first is not brought to the schema of another
    // build while it is written to.
    let listener = match benshi_daemon::ipc::bind(&socket) {
        Ok(listener) => Arc::new(listener),
        Err(unbound) => {
            voice.say(format_args!(
                "{} cannot be used: {unbound}",
                socket.display()
            ));
            return FAILED;
        }
    };
    let database = match directory(Role::Data) {
        Ok(data) => data.join(DATABASE),
        Err(nowhere) => {
            voice.say(format_args!("{nowhere}"));
            return FAILED;
        }
    };
    let store = match Store::open(&database) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(unopened) => {
            voice.say(format_args!(
                "{} cannot be used: {unopened}",
                database.display()
            ));
            return FAILED;
        }
    };
    voice.say(format_args!("listening on {}", socket.display()));
    voice.say(format_args!("recording in {}", database.display()));

    // A factory rather than a watcher: a restart has to reconnect to the
    // session bus, which is the failure it exists to recover from.
    let running = benshi_daemon::run(
        || MprisWatcher::connect(SystemClock::new(), SOURCE_DEADLINE),
        listener,
        // No list to match a name against, so every file is refused and
        // `benshi why` says so.
        Recogniser::empty(),
        store,
        SystemTime::now,
        POLL_INTERVAL,
        // Said as it happens: the socket goes on answering once detection
        // has stopped, and the process does not end to say so. The
        // supervisor deals with no task that ended until this returns, so it
        // hands the line over and does not write it.
        {
            let voice = Arc::clone(voice);
            move |notice| voice.say(format_args!("{notice}"))
        },
    );
    let signal = tokio::select! {
        _records = running => {
            // Reached only when nothing is left running, which for a daemon
            // is a failure however tidily each task arrived at it. What
            // stopped each task was said as it stopped.
            return FAILED;
        }
        signal = stops.first() => signal,
    };

    // The supervisor went with `running`, and its `JoinSet` aborts every
    // task as it is dropped. A task that is aborted ends at an await: the
    // one it waits at, or the next it reaches. The runtime waits for that
    // as it is dropped, and for a call into the store that is under way.
    voice.say(format_args!("stopped by {signal}"));

    STOPPED
}

/// There is no adapter for this platform, so there is no daemon to run.
///
/// Not async, and no runtime is built. A runtime with nothing to watch would
/// start, find one task, and report that it stopped, which says the daemon
/// failed rather than that this platform has no daemon.
#[cfg(not(target_os = "linux"))]
fn daemon(_tray: bool) -> ExitCode {
    say(format_args!("this platform has no detection adapter yet"));

    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::cell::RefCell;
    #[cfg(target_os = "linux")]
    use std::io;

    use super::Invocation;

    #[cfg(target_os = "linux")]
    #[test]
    fn a_status_that_does_not_say_what_is_ignored_is_said_and_read_as_all_ignored() {
        let said = RefCell::new(Vec::new());

        let waited = super::waited_for(
            || Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
            // The mask it is handed, for the test to read.
            Ok,
            |line| said.borrow_mut().push(line.to_string()),
        );

        // Every signal as though it were ignored: of those, none stops a
        // daemon.
        assert_eq!(waited, Ok(u64::MAX));
        assert_eq!(
            said.into_inner(),
            ["/proc/self/status does not say which signals are ignored, \
              so none is waited for: no such file"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signals_that_cannot_be_taken_are_said_and_the_daemon_fails() {
        let said = RefCell::new(Vec::new());

        let waited = super::waited_for(
            || Ok(0),
            |_ignored| Err::<(), _>(io::Error::other("refused")),
            |line| said.borrow_mut().push(line.to_string()),
        );

        assert_eq!(waited, Err(super::FAILED));
        assert_eq!(said.into_inner(), ["cannot wait for a signal: refused"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_signals_read_as_ignored_are_handed_to_what_takes_the_rest() {
        let said = RefCell::new(Vec::<String>::new());

        // What `nohup` leaves: signal 1, which is `SIGHUP`.
        let waited = super::waited_for(
            || Ok(0b1),
            Ok,
            |line| said.borrow_mut().push(line.to_string()),
        );

        assert_eq!(waited, Ok(0b1));
        assert!(said.into_inner().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_signals_a_process_ignores_are_read_from_its_status() {
        // A `status` as procfs writes one, cut to the lines around the mask.
        let status = "Threads:\t1\nSigQ:\t0/126428\nSigPnd:\t0000000000000000\n\
                      ShdPnd:\t0000000000000000\nSigBlk:\t0000000000000000\n\
                      SigIgn:\t0000000000001001\nSigCgt:\t0000000000004002\n";

        assert_eq!(super::ignored_in(status), Some(0x1001));
        assert_eq!(
            super::ignored_in("Threads:\t1\nSigCgt:\t0000000000004002\n"),
            None
        );
        assert_eq!(super::ignored_in("SigIgn:\tnot a mask\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_signal_that_was_ignored_is_not_one_that_stops_the_daemon() {
        let named = |ignored| {
            super::stopping(ignored)
                .map(|(_kind, name)| name)
                .collect::<Vec<_>>()
        };

        assert_eq!(named(0), ["SIGTERM", "SIGINT", "SIGHUP"]);
        // What `nohup` leaves: signal 1, which is `SIGHUP`.
        assert_eq!(named(0b1), ["SIGTERM", "SIGINT"]);
        // Signal 2, which is `SIGINT`, and signal 15, which is `SIGTERM`.
        assert_eq!(named(0b10), ["SIGTERM", "SIGHUP"]);
        assert_eq!(named(1 << 14), ["SIGINT", "SIGHUP"]);
        // `SIGPIPE`, which every Rust program ignores, is none of them.
        assert_eq!(named(1 << 12), ["SIGTERM", "SIGINT", "SIGHUP"]);
        assert!(named(u64::MAX).is_empty());
    }

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
