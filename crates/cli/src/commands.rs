//! What each command does, and what the process should exit with.
//!
//! A command opens a connection, sends a request and renders what comes back:
//! one answer for `sources`, `policy` and `why`, and a stream for `watch` until
//! the daemon or the user ends it. `record` wants both an answer and a stream, so
//! it takes a second connection for the answer: a stream is the last thing a
//! connection carries. What it records goes to a file, and only a summary of
//! it to the terminal. `login` and `logout` are answered twice: once the
//! daemon has read the request, and with what it came to.
//!
//! A failure is reported here rather than returned. This is where a `Result`
//! stops being something a caller can act on and becomes a line on the error
//! stream and a status a shell can test.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use benshi_core::policy::Policy;
use benshi_core::token::Token;
use benshi_core::trace::{TraceHeader, header_line, snapshot_line};
use benshi_core::{AppName, PlayerId, SourceInfo};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::protocol::{Request, Response, SourceListing};
use clap::{Subcommand, ValueEnum};
use humantime::format_rfc3339_seconds;

use crate::client::Daemon;
use crate::prompt::Typed;
use crate::render;

/// What a client can ask for.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// List every source the daemon has seen, with its policy and capabilities.
    Sources,

    /// Print the daemon's events as they happen.
    Watch,

    /// Set what the daemon does with readings from an application.
    Policy {
        /// The application, as the listing names it.
        app: String,
        /// What to do with its readings.
        policy: PolicyArgument,
    },

    /// Explain the most recent recognition decision.
    ///
    /// Which player, which file, what its name spelled, and what recognition
    /// answered: the title and the stage that decided it, the candidates it
    /// could not choose between, how close the nearest candidate came, or that
    /// there was nothing to compare the name against.
    Why,

    // Every line below is what `benshi record --help` prints, so all of it is
    // written for someone at a terminal: no intra-doc links and no backticks,
    // which clap passes through verbatim brackets and all. Why the daemon has
    // no request for this is a decision record's business, not a user's. This
    // note is `//` for the same reason - a `///` here would print too.
    /// Write what the daemon sees into a trace file.
    ///
    /// Records every reading the daemon publishes, one to a line, until the
    /// time is up. Ends early if the daemon closes the connection, and says so.
    /// A recording that missed readings fails rather than leaving a file with a
    /// gap in it.
    Record {
        /// How long to record for, as 60s, 90min or 1h30m.
        #[arg(long = "for", value_name = "DURATION", value_parser = humantime::parse_duration)]
        duration: Duration,

        /// Where to write the trace. An existing file is replaced.
        #[arg(long, short, value_name = "PATH")]
        output: PathBuf,
    },

    /// Log in to AniList, keeping the token in the secret store.
    ///
    /// Prints the address of the page where AniList gives a token for
    /// benshi, and reads the token pasted from it without showing it. The
    /// daemon asks AniList whose the token is before it keeps it.
    Login,

    /// Remove the AniList token from the secret store.
    Logout,
}

/// A policy as it is typed on the command line.
///
/// Separate from [`Policy`] because that type lives in a crate which takes no
/// dependency on an argument parser, and because the spelling a user types is a
/// property of this interface rather than of the domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PolicyArgument {
    /// Accept the source when what it has open parses as anime.
    Auto,
    /// Accept the source whatever it has open.
    Allow,
    /// Ignore the source's readings, and keep listing it.
    Deny,
}

impl From<PolicyArgument> for Policy {
    fn from(argument: PolicyArgument) -> Self {
        match argument {
            PolicyArgument::Auto => Self::Auto,
            PolicyArgument::Allow => Self::Allow,
            PolicyArgument::Deny => Self::Deny,
        }
    }
}

/// Run one command, reporting a failure on `err` rather than returning it.
///
/// Both streams are taken rather than written to directly, so that what a user
/// sees is what a test reads.
pub async fn run(
    command: Command,
    socket: &Path,
    typed: impl FnOnce() -> io::Result<Typed>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> ExitCode {
    match carry_out(command, socket, typed, out, err).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // `{:#}` puts the cause on the same line as the context, so that
            // "no daemon at /run/user/1000/benshi.sock" arrives with whether
            // the socket was missing or refused the connection. The two are
            // fixed differently.
            //
            // If the error stream itself cannot be written to there is nowhere
            // left to say so, and the process is already failing. The lint
            // against discarding a `Result` is answered by saying that here
            // rather than by silencing it.
            drop(writeln!(err, "benshi: {failure:#}"));
            ExitCode::FAILURE
        }
    }
}

/// Run one command, failing rather than reporting.
async fn carry_out(
    command: Command,
    socket: &Path,
    typed: impl FnOnce() -> io::Result<Typed>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    let mut daemon = Daemon::connect(socket).await?;

    match command {
        Command::Sources => sources(&mut daemon, out).await,
        Command::Watch => watch(&mut daemon, out).await,
        Command::Policy { app, policy } => set_policy(&mut daemon, &app, policy.into()).await,
        Command::Why => why(&mut daemon, out).await,
        Command::Record { duration, output } => {
            record(&mut daemon, socket, duration, &output, out).await
        }
        // The connection says that a daemon is there before a token is asked
        // for. The login goes on one of its own, made once the token is read:
        // the deadline of a connection runs from when it is made, and a
        // person takes longer than that to paste.
        Command::Login => {
            drop(daemon);
            login(socket, typed, out, err).await
        }
        Command::Logout => logout(&mut daemon, out).await,
    }
}

/// Ask for the listing and read the answer.
///
/// The one place the question is asked, so that what a user is shown and what a
/// recording declares cannot drift apart in which answers they accept, or name
/// an unexpected one in two different ways.
async fn ask_for_sources(daemon: &mut Daemon) -> Result<Vec<SourceListing>> {
    daemon.ask(&Request::Sources).await?;

    match daemon.expect_answer().await? {
        Response::Sources(listed) => Ok(listed),
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered a listing with {unexpected:?}"),
    }
}

/// What the daemon has seen, as a recording declares it.
///
/// The policy in each line is left behind: it is the daemon's decision about a
/// source rather than the platform's description of it, and a replay applies
/// whatever table it is running under rather than one a file remembers.
async fn listing(socket: &Path) -> Result<Vec<SourceInfo>> {
    let mut daemon = Daemon::connect(socket).await?;
    let listed = ask_for_sources(&mut daemon).await?;

    Ok(listed
        .into_iter()
        .map(|source| SourceInfo {
            player: source.player,
            app: source.app,
            capabilities: source.capabilities,
            state: source.state,
        })
        .collect())
}

/// Print every source the daemon has seen.
async fn sources(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    let listed = ask_for_sources(daemon).await?;

    write!(out, "{}", render::listing(&listed)).context("listing could not be printed")
}

/// Ask for the stream and read the answer that says it is on.
///
/// The one place a stream is asked for, as [`ask_for_sources`] is the one
/// place a listing is. The answer is what a daemon that does not answer fails
/// to give in time; the events after it come whenever they come.
async fn subscribe(daemon: &mut Daemon) -> Result<()> {
    daemon.ask(&Request::Watch).await?;

    taken(daemon, "a watch").await
}

/// Read the answer that says a request is read, and that what it comes to
/// follows. `asked` names the request in a line about an answer of another
/// kind.
async fn taken(daemon: &mut Daemon, asked: &str) -> Result<()> {
    match daemon.expect_answer().await? {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered {asked} with {unexpected:?}"),
    }
}

/// The page where AniList gives a token for benshi's application.
const AUTHORIZE: &str =
    "https://anilist.co/api/v2/oauth/authorize?client_id=53750&response_type=token";

/// Ask for a token, have the daemon check and keep it, and print whose it is.
async fn login(
    socket: &Path,
    typed: impl FnOnce() -> io::Result<Typed>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> Result<()> {
    write!(
        err,
        "open this address and approve benshi:\n{AUTHORIZE}\n\
         paste the token AniList shows (input is hidden): "
    )
    .and_then(|()| err.flush())
    .context("prompt could not be printed")?;
    // Holds the runtime while a person pastes, and nothing else is running
    // on it: no connection is open.
    let typed = typed();
    // Nothing typed was shown, the end of the line included.
    writeln!(err).context("prompt could not be printed")?;
    let token = match typed.context("token could not be read")? {
        Typed::Cancelled => bail!("login cancelled"),
        Typed::Line(line) => line.secret().trim().to_owned(),
    };
    if token.is_empty() {
        bail!("no token given");
    }

    let mut daemon = Daemon::connect(socket).await?;
    daemon
        .ask(&Request::Login {
            token: Token::new(token),
        })
        .await?;
    taken(&mut daemon, "a login").await?;

    match daemon.expect_outcome().await? {
        Response::LoggedIn { name, expires } => {
            write!(out, "{}", render::logged_in(&name, expires))
                .context("login could not be printed")
        }
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered a login with {unexpected:?}"),
    }
}

/// Have the daemon remove the token, and say that it is gone.
async fn logout(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    daemon.ask(&Request::Logout).await?;
    taken(daemon, "a logout").await?;

    match daemon.expect_outcome().await? {
        Response::LoggedOut => {
            write!(out, "{}", render::LOGGED_OUT).context("logout could not be printed")
        }
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered a logout with {unexpected:?}"),
    }
}

/// Print the daemon's events for as long as the daemon sends them.
///
/// The stream has no end of its own, so the daemon closing the connection is
/// the daemon going away, and that is said and is a failure. A client run
/// from a terminal is ended by the user instead, which the daemon sees as the
/// connection going away and treats as the ordinary end of a stream.
async fn watch(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    subscribe(daemon).await?;

    while let Some(response) = daemon.answer().await? {
        let line = match response {
            Response::Event(happened) => render::event(&happened),
            // Said rather than passed over. A gap nobody reports reads as
            // nothing having happened, which on this stream is the one wrong
            // answer it can give.
            Response::Lagged { missed } => format!("... {missed} events missed ..."),
            Response::Error { message } => bail!("{message}"),
            unexpected => bail!("daemon sent {unexpected:?} on a stream"),
        };

        writeln!(out, "{line}").context("stream could not be printed")?;
        out.flush().context("stream could not be printed")?;
    }

    bail!("daemon closed connection")
}

/// Set what the daemon does with readings from an application.
async fn set_policy(daemon: &mut Daemon, app: &str, policy: Policy) -> Result<()> {
    daemon
        .ask(&Request::SetPolicy {
            app: AppName(app.to_owned()),
            policy,
        })
        .await?;

    match daemon.expect_answer().await? {
        Response::Ok => Ok(()),
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered a policy change with {unexpected:?}"),
    }
}

/// Print the most recent decision and what it was decided by.
async fn why(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    daemon.ask(&Request::Why).await?;

    match daemon.expect_answer().await? {
        Response::Why(explained) => write!(out, "{}", render::why(explained.as_ref()))
            .context("explanation could not be printed"),
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("daemon answered why with {unexpected:?}"),
    }
}

/// Write readings into a trace file until the time is up.
///
/// The recording also ends if the daemon closes the connection, which leaves a
/// shorter trace rather than a failure: what was recorded before the daemon
/// went away is still what the player was doing. The summary says which of the
/// two ended it, because twelve minutes of a ninety-minute recording and a
/// complete one are the same message otherwise, and the difference decides
/// whether the file is worth keeping.
async fn record(
    daemon: &mut Daemon,
    socket: &Path,
    duration: Duration,
    output: &Path,
    out: &mut impl Write,
) -> Result<()> {
    subscribe(daemon).await?;

    // The subscription is answered first, and the listing is asked for
    // afterwards on a connection of its own. A reading published between the
    // two ends up in the trace this way round; the other way round it would
    // be published into the gap between the answer and the subscription, and
    // be lost. A recording with a hole in it replays a timeline that never
    // happened, which is the one failure nothing downstream of the file could
    // notice - the same reason a lagged stream ends the recording below.
    //
    // Not covered by a test. Making the order observable needs a daemon that
    // stalls a listing while readings are published, and a test that cannot
    // fail would be worse than this comment.
    let declared = listing(socket).await?;

    // The subscription goes out before the file is made, so a request that
    // cannot be sent leaves no empty trace behind. A socket with nothing
    // listening on it never gets this far: `carry_out` connects before it runs
    // any command at all.
    let header = TraceHeader::new(
        format_rfc3339_seconds(SystemTime::now()).to_string(),
        declared,
    );
    let mut file =
        File::create(output).with_context(|| format!("{} is not writable", output.display()))?;
    append(&mut file, &header_line(&header)?, output)?;

    let declared: BTreeSet<PlayerId> = header
        .sources
        .iter()
        .map(|source| source.player.clone())
        .collect();

    let mut readings = 0_usize;
    // Sources that produced a reading without being in the header. Ordered and
    // deduplicated because this is read by a person: a player opened during a
    // recording produces one of these every second otherwise.
    let mut outran: BTreeSet<PlayerId> = BTreeSet::new();
    // One deadline over the whole recording rather than one per answer. A
    // quiet desktop sends nothing for minutes, and a per-answer timeout would
    // end the recording instead of recording the quiet.
    let recording = tokio::time::timeout(duration, async {
        while let Some(response) = daemon.answer().await? {
            match response {
                Response::Event(BusEvent::Snapshot(reading)) => {
                    if !declared.contains(&reading.player) {
                        outran.insert(reading.player.clone());
                    }
                    append(&mut file, &snapshot_line(&reading)?, output)?;
                    readings += 1;
                }
                // Not readings. A membership change or a source failing is a
                // fact about the daemon rather than something a player
                // reported, and a state is what a replay works out from the
                // readings rather than reads off the file. Named one by one
                // rather than caught by a wildcard: an event added later then
                // falls to the last arm here and stops the recording, rather
                // than going missing in the same silence a lag would.
                Response::Event(
                    BusEvent::SourcesChanged { .. }
                    | BusEvent::SourceFailed { .. }
                    | BusEvent::State(_),
                ) => {}
                // The one answer a recorder must not carry on from. A trace
                // with a hole in it replays a timeline that never happened,
                // and nothing downstream of the file could tell.
                Response::Lagged { missed } => {
                    bail!("missed {missed} readings, so this recording would have a gap in it")
                }
                Response::Error { message } => bail!("{message}"),
                unexpected => bail!("daemon sent {unexpected:?} on a stream"),
            }
        }

        Ok(())
    });

    // Running out of time is how a recording is meant to end, so only the
    // inner failure is a failure. The stream ending first is not one either -
    // what reached the file is still what the player was doing - but it is a
    // shorter recording than the one that was asked for, and the summary is
    // the only place that can say so.
    let cut_short = match recording.await {
        Err(_the_time_was_up) => false,
        Ok(ended) => {
            ended?;
            true
        }
    };
    let why = if cut_short {
        " - daemon closed connection before time was up"
    } else {
        ""
    };

    writeln!(
        out,
        "recorded {readings} readings to {}{why}",
        output.display()
    )
    .context("summary could not be printed")?;

    // Said because the file is a true record of what was published and parses
    // like any other: nothing about it looks wrong, so a replay days later,
    // against a session that cannot be recorded again, is where this would
    // otherwise be found out.
    //
    // It says what was observed and not why. A player opened during the
    // recording is the usual cause and not the only one - a recording begun
    // before the daemon's first round has nothing to declare, and a source that
    // missed its deadline on the round the listing came from is left out of it -
    // and naming the wrong one of those is worse than naming none.
    if outran.is_empty() {
        return Ok(());
    }

    let named: Vec<&str> = outran.iter().map(|player| player.0.as_str()).collect();
    writeln!(
        out,
        "some readings came from sources this recording never declared, \
         so it holds more than it can replay: {}",
        named.join(", ")
    )
    .context("summary could not be printed")
}

/// Put one line in the trace, where a reader finds it before the next arrives.
///
/// Written out a line at a time rather than gathered up and written at the end,
/// so that a recording interrupted after fifty seconds holds fifty seconds. A
/// `File` does not buffer in this process, so `write_all` alone already has
/// that property and the flush does nothing. It is kept because deleting it
/// would leave the property resting on the type this argument happens to have,
/// and a `BufWriter` put here later would take it away silently.
fn append(file: &mut File, line: &str, output: &Path) -> Result<()> {
    file.write_all(line.as_bytes())
        .and_then(|()| file.flush())
        .with_context(|| format!("{} could not be written", output.display()))
}

#[cfg(test)]
mod tests {
    use super::{Command, run};
    use crate::client::DEADLINE;
    use crate::prompt::Typed;
    use benshi_core::clock::{Clock, TestClock};
    use benshi_core::path::RawPath;
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::token::Token;
    use benshi_core::trace::Trace;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, SessionState,
        SourceInfo,
    };
    use benshi_daemon::accounts::{self, Accounts, Done, LoggedIn};
    use benshi_daemon::bus::{BusEvent, EventBus};
    use benshi_daemon::detection::Seen;
    use benshi_daemon::ipc::{Server, bind};
    use benshi_daemon::protocol::{Request, Response};
    use benshi_daemon::recognition::{Decided, Decision, Recogniser};
    use benshi_daemon::secrets;
    use benshi_daemon::supervisor::TaskError;
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;
    use tokio::time::{Instant, timeout};

    /// How long a test waits before calling an answer lost.
    ///
    /// Only ever spent on a failure. What matters is that it exists: a command
    /// that never answers has to fail the run rather than hang it.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// How often a test looks again for something that arrives on its own.
    ///
    /// A sleep rather than a yield. On the paused clock a look that sleeps
    /// parks the runtime, and the clock moves on to [`PATIENCE`] where what
    /// is looked for never comes; a look that yields keeps the runtime busy,
    /// the clock stands still, and the test holds the run instead of failing.
    const POLL: Duration = Duration::from_millis(1);

    fn a_source(identity: &str, app: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    /// The source every reading in these tests comes from.
    const PLAYER: &str = "mpv.instance1701";

    /// The `index`th reading of a recording, distinguishable from the others.
    fn a_reading(index: u64) -> PlayerSnapshot {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(index));

        PlayerSnapshot {
            player: PlayerId(PLAYER.to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show - 03.mkv".to_vec(),
            )),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(index)),
            duration: Known::Value(Duration::from_secs(1420)),
            observed_at: clock.now(),
        }
    }

    /// A writer a test can read back while a command is still writing to it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("the buffer").clone()).expect("valid utf-8")
        }

        /// Wait until the text satisfies `enough`, or fail.
        async fn until(&self, what: &str, enough: impl Fn(&str) -> bool) -> String {
            timeout(PATIENCE, async {
                loop {
                    let text = self.text();
                    if enough(&text) {
                        return text;
                    }
                    tokio::time::sleep(POLL).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{what}; what arrived was {:?}", self.text()))
        }
    }

    impl Write for Shared {
        fn write(&mut self, written: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the buffer")
                .extend_from_slice(written);
            Ok(written.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A stream that cannot be written to, as a pipe cannot whose reader has
    /// gone.
    struct Unwritable;

    impl Write for Unwritable {
        fn write(&mut self, _written: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    /// A stream that takes what is written to it and cannot be flushed.
    struct Unflushable;

    impl Write for Unflushable {
        fn write(&mut self, written: &[u8]) -> std::io::Result<usize> {
            Ok(written.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    /// A stream that takes one line and cannot be written to after it.
    #[derive(Default)]
    struct WritableForALine {
        taken: bool,
    }

    impl Write for WritableForALine {
        fn write(&mut self, written: &[u8]) -> std::io::Result<usize> {
            if self.taken {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.taken = written.contains(&b'\n');

            Ok(written.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// When the token of a login runs out: 2030-03-17 17:46:40 UTC.
    const EXP: u64 = 1_900_000_000;

    /// Logins and logouts that go through, or fail as they do where the
    /// secret store is locked, with what each was asked.
    #[derive(Debug, Default)]
    struct Logins {
        failing: bool,
        asked: Mutex<Vec<String>>,
    }

    impl Logins {
        fn failing() -> Self {
            Self {
                failing: true,
                ..Self::default()
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked
                .lock()
                .expect("no test panics while it holds this")
                .clone()
        }

        fn ask(&self, what: String) {
            self.asked
                .lock()
                .expect("no test panics while it holds this")
                .push(what);
        }
    }

    impl Accounts for Logins {
        fn login(&self, token: Token) -> Done<'_, LoggedIn> {
            self.ask(format!("login {}", token.secret()));
            let logged = if self.failing {
                Err(accounts::Error::NotKept(secrets::Error::Locked))
            } else {
                Ok(LoggedIn {
                    name: "someone".to_owned(),
                    expires: Some(EXP),
                })
            };

            Box::pin(std::future::ready(logged))
        }

        fn logout(&self) -> Done<'_, ()> {
            self.ask("logout".to_owned());
            let logged_out = if self.failing {
                Err(accounts::Error::NotRemoved(secrets::Error::Locked))
            } else {
                Ok(())
            };

            Box::pin(std::future::ready(logged_out))
        }
    }

    /// A daemon listening on a socket of its own.
    struct Daemon {
        home: TempDir,
        socket: PathBuf,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
        decided: Decided,
        accounts: Arc<Logins>,
        serving: JoinHandle<Result<(), TaskError>>,
    }

    impl Daemon {
        fn listening(sources: Vec<SourceInfo>) -> Self {
            Self::listening_with(sources, EventBus::new())
        }

        /// A daemon whose bus keeps almost nothing, so a client can be made to
        /// fall behind without publishing hundreds of events.
        fn forgetful() -> Self {
            Self::listening_with(Vec::new(), EventBus::with_capacity(2))
        }

        fn listening_with(sources: Vec<SourceInfo>, bus: EventBus) -> Self {
            let (home, socket, listener) = a_socket_bound_and_not_served();

            Self::serving(home, socket, listener, sources, bus)
        }

        /// A daemon whose logins and logouts go as `accounts` makes them.
        fn logging_in(accounts: Logins) -> Self {
            let (home, socket, listener) = a_socket_bound_and_not_served();

            Self::built(
                home,
                socket,
                listener,
                Vec::new(),
                EventBus::new(),
                accounts,
            )
        }

        /// A daemon serving a socket that was bound before, as the daemon's
        /// is once its database is open.
        fn serving(
            home: TempDir,
            socket: PathBuf,
            listener: UnixListener,
            sources: Vec<SourceInfo>,
            bus: EventBus,
        ) -> Self {
            Self::built(home, socket, listener, sources, bus, Logins::default())
        }

        fn built(
            home: TempDir,
            socket: PathBuf,
            listener: UnixListener,
            sources: Vec<SourceInfo>,
            bus: EventBus,
            accounts: Logins,
        ) -> Self {
            let bus = Arc::new(bus);
            let policy = Arc::new(RwLock::new(PolicyTable::allowing_video_players()));
            let seen = Seen::new();
            seen.record(sources);
            let decided = Decided::new();
            let accounts = Arc::new(accounts);
            let server = Arc::new(Server::new(
                Arc::clone(&bus),
                Arc::clone(&policy),
                seen,
                decided.clone(),
                Arc::clone(&accounts) as Arc<dyn Accounts>,
            ));

            Self {
                home,
                socket,
                bus,
                policy,
                decided,
                accounts,
                serving: tokio::spawn(server.listen(Arc::new(listener))),
            }
        }

        /// Leave behind a decision about a file of this name, as a detection
        /// round would.
        fn has_decided(&self, name: &str) {
            let path = RawPath::from_bytes(name.as_bytes().to_vec());
            let (parsed, answer) = Recogniser::empty().decide(&path);

            self.decided.record(Decision {
                player: PlayerId(PLAYER.to_owned()),
                media: MediaRef::LocalFile(path),
                parsed,
                answer,
            });
        }

        /// Stop the daemon, as a restart or a kill would.
        ///
        /// Aborting the accept loop drops the connections it owns, which is
        /// what a client watching one sees when the daemon goes away: the end
        /// of the stream rather than a fault on it.
        fn goes_away(&self) {
            self.serving.abort();
        }

        /// Wait until a client has subscribed to the bus.
        ///
        /// Returns only once the daemon holds the subscription, so that an
        /// event published afterwards cannot be missed by a client that had not
        /// arrived yet.
        async fn subscribed(&self) {
            timeout(PATIENCE, async {
                while self.bus.subscribers() == 0 {
                    tokio::time::sleep(POLL).await;
                }
            })
            .await
            .expect("a client subscribed to the bus");
        }

        fn policy_for(&self, app: &str) -> Policy {
            self.policy
                .read()
                .expect("the table")
                .policy_for(&AppName(app.to_owned()))
        }

        /// A `watch` against this daemon, with the streams it is writing to.
        ///
        /// Returns once the daemon has the subscription, for the reason
        /// [`Daemon::subscribed`] gives.
        async fn watching(&self) -> (JoinHandle<ExitCode>, Shared, Shared) {
            let out = Shared::default();
            let err = Shared::default();
            let socket = self.socket.clone();
            let watching = tokio::spawn({
                let mut out = out.clone();
                let mut err = err.clone();
                async move { run(Command::Watch, &socket, untyped, &mut out, &mut err).await }
            });

            self.subscribed().await;

            (watching, out, err)
        }

        /// A daemon that has seen the source [`a_reading`] comes from.
        ///
        /// A recording declares the sources the daemon listed when it began, so
        /// a daemon that has already seen this player is the ordinary case: the
        /// player was open before the recording started, every reading comes
        /// from a declared source, and the summary has nothing to add about
        /// them. A player that appears during a recording is the other case,
        /// and `a_recording_names_a_source_that_outran_the_listing_it_declared`
        /// is where it is covered.
        fn that_has_seen_the_player() -> Self {
            Self::listening(vec![a_source(PLAYER, "mpv")])
        }

        /// A `record` against this daemon, writing into a file of its own.
        ///
        /// Returns once the daemon has the subscription, for the same reason
        /// [`Daemon::watching`] does.
        async fn recording(&self, over: Duration) -> Recording {
            let out = Shared::default();
            let err = Shared::default();
            let output = self.home.path().join("trace.jsonl");
            let socket = self.socket.clone();
            let command = Command::Record {
                duration: over,
                output: output.clone(),
            };
            let running = tokio::spawn({
                let mut out = out.clone();
                let mut err = err.clone();
                async move { run(command, &socket, untyped, &mut out, &mut err).await }
            });

            self.subscribed().await;

            Recording {
                running,
                output,
                out,
                err,
            }
        }
    }

    /// A `record` in progress, and everywhere it is writing.
    struct Recording {
        running: JoinHandle<ExitCode>,
        output: PathBuf,
        out: Shared,
        err: Shared,
    }

    impl Recording {
        /// Wait until the file on disk holds `readings`.
        ///
        /// Reads the file rather than waiting on the command, so that what the
        /// test checks is what a recorder interrupted at that moment would
        /// have left behind.
        async fn holding(&self, readings: usize) -> Trace {
            timeout(PATIENCE, async {
                loop {
                    if let Some(trace) = self.so_far()
                        && trace.snapshots.len() >= readings
                    {
                        return trace;
                    }
                    tokio::time::sleep(POLL).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the trace never reached {readings} readings; the file holds {:?}",
                    std::fs::read_to_string(&self.output)
                )
            })
        }

        /// Wait until the file on disk holds `readings`, then stop recording.
        async fn once_it_holds(self, readings: usize) -> Trace {
            let trace = self.holding(readings).await;
            self.stop();

            trace
        }

        /// Stop recording, where a user would press Ctrl-C.
        fn stop(self) {
            self.running.abort();
        }

        /// The trace on disk, if there is one that parses.
        fn so_far(&self) -> Option<Trace> {
            let text = std::fs::read_to_string(&self.output).ok()?;

            Trace::from_jsonl(&text).ok()
        }

        /// Wait for the recording to finish on its own.
        async fn until_it_stops(self) -> (ExitCode, String, String) {
            let outcome = timeout(PATIENCE, self.running)
                .await
                .expect("the recording ended")
                .expect("the recording did not panic");

            (outcome, self.out.text(), self.err.text())
        }
    }

    /// A path in a directory of its own where no daemon is listening.
    fn nowhere() -> (TempDir, PathBuf) {
        let home = tempfile::tempdir().expect("a directory of our own");
        let socket = home.path().join("run").join("benshi.sock");

        (home, socket)
    }

    /// A socket bound and not served, as a daemon's is while it opens its
    /// database: a connection to it waits in the backlog.
    fn a_socket_bound_and_not_served() -> (TempDir, PathBuf, UnixListener) {
        let (home, socket) = nowhere();
        let listener = bind(&socket).expect("the socket binds");

        (home, socket, listener)
    }

    /// A socket served by something that answers every request with `answer`
    /// and nothing more. The daemon never answers so; the client is written
    /// for the protocol, and this is what holds it to that.
    fn a_daemon_that_answers_with(answer: &Response) -> (TempDir, PathBuf, JoinHandle<()>) {
        a_daemon_that_writes(line_of(answer))
    }

    /// One answer as the daemon writes it: a line, newline included.
    fn line_of(answer: &Response) -> Vec<u8> {
        let mut line = serde_json::to_vec(answer).expect("serialise");
        line.push(b'\n');

        line
    }

    /// A socket served by something that answers every request with `line`,
    /// byte for byte.
    fn a_daemon_that_writes(line: Vec<u8>) -> (TempDir, PathBuf, JoinHandle<()>) {
        a_daemon_that_answers(move |_request| line.clone())
    }

    /// A socket served by something that answers each request with what
    /// `answer` makes of its line, byte for byte.
    fn a_daemon_that_answers(
        answer: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> (TempDir, PathBuf, JoinHandle<()>) {
        let answer = Arc::new(answer);
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let answering = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.expect("a client arrived");
                let answer = Arc::clone(&answer);
                drop(tokio::spawn(async move {
                    let (reader, mut writer) = tokio::io::split(stream);
                    let mut requests = BufReader::new(reader).lines();
                    while let Ok(Some(request)) = requests.next_line().await {
                        if writer.write_all(&answer(&request)).await.is_err() {
                            break;
                        }
                    }
                }));
            }
        });

        (home, socket, answering)
    }

    /// A socket served by something that answers a listing with no source,
    /// and a subscription with the answer that the stream is on and then
    /// with `carried`.
    fn a_daemon_whose_stream_carries(carried: &Response) -> (TempDir, PathBuf, JoinHandle<()>) {
        let listed = line_of(&Response::Sources(Vec::new()));
        let stream = [line_of(&Response::Ok), line_of(carried)].concat();

        a_daemon_that_answers(move |request| {
            match serde_json::from_str(request).expect("a request") {
                Request::Watch => stream.clone(),
                Request::Sources => listed.clone(),
                other => panic!("{other:?} is asked by no reader of a stream"),
            }
        })
    }

    /// The answers a stream does not carry, each with how it is named.
    fn answers_no_stream_carries() -> [(Response, &'static str); 3] {
        [
            (Response::Ok, "Ok"),
            (Response::Sources(Vec::new()), "Sources([])"),
            (Response::Why(None), "Why(None)"),
        ]
    }

    async fn go(command: Command, socket: &Path) -> (ExitCode, String, String) {
        go_typing(command, socket, Typed::Line(Token::new(PASTED))).await
    }

    /// [`go`], with `typed` typed at a prompt.
    async fn go_typing(
        command: Command,
        socket: &Path,
        typed: Typed,
    ) -> (ExitCode, String, String) {
        let mut out = Shared::default();
        let mut err = Shared::default();
        let outcome = run(command, socket, || Ok(typed), &mut out, &mut err).await;

        (outcome, out.text(), err.text())
    }

    /// [`go`], with the moment the command's deadline runs from: for a
    /// login, the moment its token is typed, since it connects for its
    /// request only then, and for any other command the moment it starts.
    async fn go_timed(command: Command, socket: &Path) -> (Instant, ExitCode, String, String) {
        let from = Arc::new(Mutex::new(Instant::now()));
        let typed_at = Arc::clone(&from);
        let mut out = Shared::default();
        let mut err = Shared::default();
        let outcome = run(
            command,
            socket,
            move || {
                *typed_at.lock().expect("no test panics while it holds this") = Instant::now();
                Ok(Typed::Line(Token::new(PASTED)))
            },
            &mut out,
            &mut err,
        )
        .await;
        let from = *from.lock().expect("no test panics while it holds this");

        (from, outcome, out.text(), err.text())
    }

    /// What a command that reads nothing typed is given to read.
    fn untyped() -> std::io::Result<Typed> {
        unreachable!("this command reads nothing typed")
    }

    /// The token pasted at a login.
    const PASTED: &str = "a.pasted.token";

    /// What a login asks before it reads a token, and the end of the line
    /// the token was pasted on.
    const ASKED: &str = "open this address and approve benshi:\n\
        https://anilist.co/api/v2/oauth/authorize?client_id=53750&response_type=token\n\
        paste the token AniList shows (input is hidden): \n";

    /// What `command` says before it is answered: a login asks for a token.
    fn asked_first(command: &Command) -> &'static str {
        if *command == Command::Login {
            ASKED
        } else {
            ""
        }
    }

    #[tokio::test]
    async fn sources_prints_what_the_daemon_listed() {
        let daemon = Daemon::listening(vec![
            a_source("mpv.instance1701", "mpv"),
            a_source("firefox.instance30062", "firefox"),
        ]);

        let (outcome, shown, complained) = go(Command::Sources, &daemon.socket).await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(shown.contains("mpv.instance1701"), "{shown}");
        assert!(shown.contains("firefox.instance30062"), "{shown}");
        assert!(complained.is_empty(), "{complained}");
    }

    #[tokio::test]
    async fn sources_shows_the_policy_the_daemon_has_in_force() {
        // The listing exists to answer "why is my player being ignored", so the
        // policy it shows has to be the daemon's rather than a default the
        // client assumed.
        let daemon = Daemon::listening(vec![
            a_source("mpv", "mpv"),
            a_source("firefox.instance30062", "firefox"),
        ]);

        let (_outcome, shown, _complained) = go(Command::Sources, &daemon.socket).await;

        let firefox = shown
            .lines()
            .find(|line| line.contains("firefox"))
            .expect("firefox is listed");
        assert!(firefox.contains("deny"), "{firefox}");
        let mpv = shown
            .lines()
            .find(|line| line.starts_with("mpv "))
            .expect("mpv is listed");
        assert!(mpv.contains("auto"), "{mpv}");
    }

    #[tokio::test]
    async fn a_policy_set_from_the_client_is_the_one_the_daemon_holds() {
        let daemon = Daemon::listening(vec![a_source("mpv", "mpv")]);
        assert_eq!(daemon.policy_for("mpv"), Policy::Auto);

        let (outcome, _shown, complained) = go(
            Command::Policy {
                app: "mpv".to_owned(),
                policy: super::PolicyArgument::Deny,
            },
            &daemon.socket,
        )
        .await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(daemon.policy_for("mpv"), Policy::Deny);
    }

    #[tokio::test]
    async fn a_policy_is_set_on_the_application_rather_than_on_one_window() {
        // Two windows of one player are two identities and one application. A
        // deny typed once has to cover both, and has to survive the player
        // being restarted under a new process id.
        let daemon = Daemon::listening(vec![
            a_source("chromium.instance16481", "chromium"),
            a_source("chromium.instance30062", "chromium"),
        ]);

        let (outcome, _shown, _complained) = go(
            Command::Policy {
                app: "chromium".to_owned(),
                policy: super::PolicyArgument::Allow,
            },
            &daemon.socket,
        )
        .await;

        assert_eq!(outcome, ExitCode::SUCCESS);
        let (_outcome, shown, _complained) = go(Command::Sources, &daemon.socket).await;
        assert_eq!(
            shown.lines().filter(|line| line.contains("allow")).count(),
            2,
            "one window was covered and the other was not: {shown}"
        );
    }

    #[tokio::test]
    async fn watch_prints_events_as_the_daemon_publishes_them() {
        let daemon = Daemon::listening(Vec::new());
        let (watching, out, err) = daemon.watching().await;

        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId("mpv".to_owned())],
        });
        daemon.bus.publish(BusEvent::SourceFailed {
            player: PlayerId("vlc".to_owned()),
            reason: "did not answer within 500ms".to_owned(),
        });

        let shown = out
            .until("two events reached the terminal", |text| {
                text.lines().count() >= 2
            })
            .await;
        watching.abort();

        assert!(shown.contains("mpv"), "{shown}");
        assert!(shown.contains("did not answer within 500ms"), "{shown}");
        assert!(err.text().is_empty(), "{}", err.text());
    }

    #[tokio::test]
    async fn watch_prints_nothing_of_its_own_before_an_event_arrives() {
        // The stream starts at the present. A banner or a heading would be the
        // first line of a recording nobody asked for.
        let daemon = Daemon::listening(Vec::new());
        let (watching, out, _err) = daemon.watching().await;

        watching.abort();

        assert!(out.text().is_empty(), "printed {:?}", out.text());
    }

    #[tokio::test]
    async fn watch_says_that_the_daemon_closed_the_connection_and_fails() {
        // The stream has no end of its own, so the daemon closing it is the
        // daemon going away. A watch that ended as a success there would
        // read as a desktop that went quiet, to a person and to a script.
        let daemon = Daemon::listening(Vec::new());
        let (watching, out, err) = daemon.watching().await;

        daemon.goes_away();
        let outcome = timeout(PATIENCE, watching)
            .await
            .expect("the watch ends once the daemon is gone")
            .expect("the watch ran");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(err.text(), "benshi: daemon closed connection\n");
        assert!(out.text().is_empty(), "printed {:?}", out.text());
    }

    #[tokio::test]
    async fn why_prints_the_most_recent_decision() {
        let daemon = Daemon::listening(vec![a_source(PLAYER, "mpv")]);
        daemon.has_decided("/anime/[Group] Show Title - 03.mkv");

        let (outcome, shown, complained) = go(Command::Why, &daemon.socket).await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(shown.contains(PLAYER), "{shown}");
        assert!(shown.contains("\"Show Title\""), "{shown}");
        assert!(shown.contains("unrecognised"), "{shown}");
        assert!(complained.is_empty(), "{complained}");
    }

    #[tokio::test]
    async fn why_says_so_when_nothing_has_been_decided() {
        // Asking before the first reading is the ordinary case, and a client
        // that printed nothing for it could not be told from one that failed.
        let daemon = Daemon::listening(Vec::new());

        let (outcome, shown, complained) = go(Command::Why, &daemon.socket).await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(shown.contains("nothing has been decided"), "{shown}");
    }

    #[tokio::test]
    async fn every_command_fails_when_no_daemon_is_listening() {
        let (_home, socket) = nowhere();

        for command in every_command(&socket) {
            let (outcome, shown, complained) = go(command, &socket).await;

            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without a daemon");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert!(
                complained.starts_with("benshi: no daemon at "),
                "got {complained:?}"
            );
            assert!(
                complained.contains(&socket.display().to_string()),
                "the message does not say where it looked: {complained:?}"
            );
        }
    }

    /// Every command, with the trace of a recording beside `socket`.
    fn every_command(socket: &Path) -> [Command; 7] {
        [
            Command::Sources,
            Command::Watch,
            Command::Why,
            Command::Policy {
                app: "mpv".to_owned(),
                policy: super::PolicyArgument::Deny,
            },
            Command::Record {
                duration: Duration::from_secs(60),
                output: socket.with_file_name("trace.jsonl"),
            },
            Command::Login,
            Command::Logout,
        ]
    }

    /// How long a test on the paused clock gives a command to end, which is
    /// longer than the client waits for an answer by far. On the paused clock
    /// it costs nothing: time moves to the next timer whenever nothing can
    /// run.
    const ENDED_BY: Duration = Duration::from_secs(60);

    /// A command running as a task of its own, for [`settled`] to wait for.
    fn started(command: Command, socket: &Path) -> JoinHandle<(ExitCode, String, String)> {
        let socket = socket.to_path_buf();

        tokio::spawn(async move { go(command, &socket).await })
    }

    /// What a command running on the paused clock ended with, waited for in
    /// steps of [`POLL`].
    ///
    /// Every time the runtime parks, it polls the sockets and then moves the
    /// paused clock to the next timer, even where that poll woke a task. An
    /// answer over a socket takes up to three parks: one for the server to
    /// accept, one for it to read the request, and one for the client to
    /// read the answer. Where the client's deadline is the next timer, those
    /// parks move the clock toward it in the steps of tokio's timer wheel,
    /// and for some deadlines the clock reaches it first and the client gives
    /// up on an answer that was on its way. A sleep of [`POLL`] is always the
    /// next timer, so each park moves the clock by one step.
    async fn settled(
        running: JoinHandle<(ExitCode, String, String)>,
    ) -> (ExitCode, String, String) {
        timeout(ENDED_BY, async {
            while !running.is_finished() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the client ended");

        running.await.expect("the client did not panic")
    }

    /// What a client says of a daemon that did not answer in time.
    fn did_not_answer(socket: &Path) -> String {
        format!(
            "benshi: daemon at {} did not answer within 5s\n",
            socket.display()
        )
    }

    #[tokio::test(start_paused = true)]
    async fn every_command_gives_up_on_a_daemon_that_accepts_nothing() {
        // A daemon whose socket task has stopped while the process goes on:
        // the connection waits in the backlog and no answer comes.
        let (_home, socket, listener) = a_socket_bound_and_not_served();

        for command in every_command(&socket) {
            let asked = asked_first(&command);
            let named = format!("{command:?}");
            let (from, outcome, shown, complained) = timeout(ENDED_BY, go_timed(command, &socket))
                .await
                .expect("the client gave up");

            assert_eq!(from.elapsed(), DEADLINE, "{named} gave up at another time");
            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without an answer");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert_eq!(complained, format!("{asked}{}", did_not_answer(&socket)));
        }

        assert!(
            !socket.with_file_name("trace.jsonl").exists(),
            "left a trace of a recording never taken"
        );
        drop(listener);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_the_daemon_does_not_read_is_given_up_on() {
        // A request longer than the socket's send buffer is sent only as the
        // daemon reads it. A daemon that accepts nothing reads nothing, and
        // the send waits under the deadline as an answer does.
        let (_home, socket, listener) = a_socket_bound_and_not_served();
        let long = Command::Policy {
            app: "\"".repeat(200_000),
            policy: super::PolicyArgument::Deny,
        };

        let (outcome, shown, complained) = timeout(ENDED_BY, go(long, &socket))
            .await
            .expect("the client gave up");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert_eq!(complained, did_not_answer(&socket));
        drop(listener);
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_waits_for_a_daemon_that_listens_late() {
        // A daemon between binding its socket and serving it, which is where
        // it opens its database: the connection waits in the backlog, and the
        // answer comes once the daemon listens, which is within the deadline.
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let asking = started(Command::Sources, &socket);

        tokio::time::sleep(DEADLINE / 2).await;
        let daemon = Daemon::serving(
            home,
            socket,
            listener,
            vec![a_source(PLAYER, "mpv")],
            EventBus::new(),
        );

        let (outcome, shown, complained) = settled(asking).await;
        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(shown.contains(PLAYER), "{shown}");
        drop(daemon);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_outlasts_the_deadline_once_it_is_on() {
        // The deadline is for the first answer alone. A desktop with nothing
        // playing sends no event for as long as that lasts, and `watch` stays.
        let daemon = Daemon::listening(Vec::new());
        let (watching, out, err) = daemon.watching().await;

        tokio::time::sleep(DEADLINE * 3).await;

        assert!(
            !watching.is_finished(),
            "the stream gave up: {}",
            err.text()
        );
        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId(PLAYER.to_owned())],
        });
        let shown = out
            .until("the event reached the terminal", |text| {
                text.contains(PLAYER)
            })
            .await;
        watching.abort();
        assert!(err.text().is_empty(), "{}", err.text());
        assert!(shown.contains(PLAYER), "{shown}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_the_daemon_refuses_is_a_failure_that_says_why() {
        // A daemon that cannot stream answers the watch with an error, and
        // what it said is what the user reads. A client that took the error
        // for the stream being on would wait for events for ever, so the
        // test gives it a deadline of its own.
        let (_home, socket, answering) = a_daemon_that_answers_with(&Response::Error {
            message: "no stream today".to_owned(),
        });

        let (outcome, shown, complained) = settled(started(Command::Watch, &socket)).await;
        answering.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert_eq!(complained, "benshi: no stream today\n");
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_answered_with_an_event_is_a_failure_that_says_so() {
        // The answer that the stream is on comes before the first event. A
        // daemon that sends an event in its place speaks another protocol,
        // and the event is reported rather than printed as the stream's.
        let (_home, socket, answering) =
            a_daemon_that_answers_with(&Response::Event(BusEvent::SourcesChanged {
                sources: vec![PlayerId(PLAYER.to_owned())],
            }));

        let (outcome, shown, complained) = settled(started(Command::Watch, &socket)).await;
        answering.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(
            shown.is_empty(),
            "printed an event it was not streaming: {shown}"
        );
        assert!(
            complained.starts_with("benshi: daemon answered a watch with Event"),
            "got {complained:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_command_gives_up_on_a_daemon_that_never_answers() {
        // The connection is accepted and read, and nothing is ever written
        // back.
        let (_home, socket, listener) = a_socket_bound_and_not_served();
        let silent = tokio::spawn(async move {
            loop {
                let (mut stream, _peer) = listener.accept().await.expect("a client arrived");
                drop(tokio::spawn(async move {
                    tokio::io::copy(&mut stream, &mut tokio::io::sink()).await
                }));
            }
        });

        for command in every_command(&socket) {
            let asked = asked_first(&command);
            let named = format!("{command:?}");
            let (from, outcome, shown, complained) = timeout(ENDED_BY, go_timed(command, &socket))
                .await
                .expect("the client gave up");

            assert_eq!(from.elapsed(), DEADLINE, "{named} gave up at another time");
            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without an answer");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert_eq!(complained, format!("{asked}{}", did_not_answer(&socket)));
        }

        assert!(
            !socket.with_file_name("trace.jsonl").exists(),
            "left a trace of a recording never taken"
        );
        silent.abort();
    }

    #[tokio::test]
    async fn every_command_says_that_a_daemon_with_no_room_is_not_accepting() {
        // The connections made to a socket nobody accepts on fill its
        // backlog. The client's connect does not block, so where the backlog
        // has no room it is answered at once that there is none. Something
        // listens there, and a client that said there was no daemon would
        // send the user to start a second one. A backlog of 0 takes one
        // connection on Linux, and the one made here fills it.
        let (_home, socket) = nowhere();
        std::fs::create_dir(socket.parent().expect("the socket is in a directory"))
            .expect("a directory for the socket");
        let full = Socket::new(Domain::UNIX, Type::STREAM, None).expect("a socket");
        full.bind(&SockAddr::unix(&socket).expect("an address"))
            .expect("the socket binds");
        full.listen(0).expect("the socket listens");
        let queued =
            std::os::unix::net::UnixStream::connect(&socket).expect("one connection is queued");
        // What the socket answers a connect that does not block, which is
        // the cause the client's line ends with.
        let probe = Socket::new(Domain::UNIX, Type::STREAM, None).expect("a socket");
        probe
            .set_nonblocking(true)
            .expect("a socket that does not block");
        let no_room = probe
            .connect(&SockAddr::unix(&socket).expect("an address"))
            .expect_err("the backlog has no room");

        for command in every_command(&socket) {
            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client waited for room");

            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without a connection");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert_eq!(
                complained,
                format!(
                    "benshi: daemon at {} is not accepting connections: {no_room}\n",
                    socket.display()
                )
            );
        }

        assert!(
            !socket.with_file_name("trace.jsonl").exists(),
            "left a trace of a recording never taken"
        );
        drop(queued);
        drop(full);
    }

    #[tokio::test]
    async fn every_command_says_that_a_socket_nobody_listens_on_has_no_daemon() {
        // A daemon that is killed leaves its socket behind, and a connect to
        // it is refused. Nothing holds that socket, which is what tells it
        // from one whose backlog has no room.
        let (_home, socket, listener) = a_socket_bound_and_not_served();
        drop(listener);

        for command in every_command(&socket) {
            let (outcome, shown, complained) = go(command, &socket).await;

            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without a daemon");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert!(
                complained.starts_with(&format!("benshi: no daemon at {}: ", socket.display())),
                "got {complained:?}"
            );
            assert!(
                complained.contains("refused"),
                "said without what the socket said: {complained:?}"
            );
        }
    }

    /// What a client says of a daemon that closed the connection before it
    /// answered, without the end of the line.
    const CLOSED_WITHOUT_ANSWERING: &str = "benshi: daemon closed connection without answering";

    /// A socket served by something that takes a connection, waits for the
    /// request to arrive and closes without answering it. Where the request
    /// is left unread the client's end is reset; where it is read, the client
    /// reads the end of the connection.
    fn a_daemon_that_hangs_up(having_read: bool) -> (TempDir, PathBuf, JoinHandle<()>) {
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let hanging_up = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.expect("a client arrived");
                stream.readable().await.expect("the request arrived");
                if having_read {
                    let mut request = String::new();
                    BufReader::new(stream)
                        .read_line(&mut request)
                        .await
                        .expect("the request is read");
                }
            }
        });

        (home, socket, hanging_up)
    }

    #[tokio::test]
    async fn every_command_says_that_a_daemon_which_closed_unread_did_not_answer() {
        // No answer had begun, so the reset is not said as a daemon that
        // stopped in the middle of one. What the client says ends with the
        // reset itself.
        let (_home, socket, hanging_up) = a_daemon_that_hangs_up(false);
        // What a read says of a connection closed with something unread.
        let (mut asking, closing) =
            std::os::unix::net::UnixStream::pair().expect("a pair of sockets");
        asking.write_all(b"unread").expect("the pair takes a write");
        drop(closing);
        let reset = std::io::Read::read(&mut asking, &mut [0]).expect_err("the end is reset");

        for command in every_command(&socket) {
            let asked = asked_first(&command);
            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");

            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without an answer");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert_eq!(
                complained,
                format!("{asked}{CLOSED_WITHOUT_ANSWERING}: {reset}\n")
            );
        }

        assert!(
            !socket.with_file_name("trace.jsonl").exists(),
            "left a trace of a recording never taken"
        );
        hanging_up.abort();
    }

    #[tokio::test]
    async fn every_command_says_that_a_daemon_which_read_and_closed_did_not_answer() {
        let (_home, socket, hanging_up) = a_daemon_that_hangs_up(true);

        for command in every_command(&socket) {
            let asked = asked_first(&command);
            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");

            assert_eq!(outcome, ExitCode::FAILURE, "succeeded without an answer");
            assert!(shown.is_empty(), "printed an answer it never got: {shown}");
            assert_eq!(complained, format!("{asked}{CLOSED_WITHOUT_ANSWERING}\n"));
        }

        assert!(
            !socket.with_file_name("trace.jsonl").exists(),
            "left a trace of a recording never taken"
        );
        hanging_up.abort();
    }

    #[tokio::test]
    async fn a_request_the_daemon_closes_on_half_sent_says_that_it_was_not_sent() {
        // A request longer than the socket's send buffer is still being sent
        // when the daemon closes, and it is the send that fails.
        let (_home, socket, hanging_up) = a_daemon_that_hangs_up(false);
        let long = Command::Policy {
            app: "\"".repeat(200_000),
            policy: super::PolicyArgument::Deny,
        };

        let (outcome, shown, complained) = timeout(PATIENCE, go(long, &socket))
            .await
            .expect("the client ended");
        hanging_up.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert!(
            complained.starts_with("benshi: request could not be sent: "),
            "got {complained:?}"
        );
        assert!(
            complained.contains("os error"),
            "said without what the socket said: {complained:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_text_is_a_failure_that_names_it() {
        // Only a reset is said as a daemon that closed the connection. A
        // first read that fails another way is said as it was.
        let (_home, socket, answering) = a_daemon_that_writes(vec![0xff, b'\n']);

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Sources, &socket))
            .await
            .expect("the client ended");
        answering.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert!(
            complained.starts_with("benshi: daemon stopped mid-answer: "),
            "got {complained:?}"
        );
        assert!(complained.contains("UTF-8"), "got {complained:?}");
    }

    #[tokio::test]
    async fn an_answer_that_is_not_one_is_a_failure_that_quotes_it() {
        // A line that is text and is no answer is quoted, with what the
        // parser said of it after.
        let line = "not an answer";
        let (_home, socket, answering) = a_daemon_that_writes(format!("{line}\n").into_bytes());
        let unread = serde_json::from_str::<Response>(line).expect_err("the line is no answer");

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Sources, &socket))
            .await
            .expect("the client ended");
        answering.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert_eq!(
            complained,
            format!("benshi: daemon sent a line this client cannot read: {line}: {unread}\n")
        );
    }

    #[tokio::test]
    async fn an_answer_of_another_kind_is_a_failure_that_names_it() {
        // The daemon never answers so. The client is written for the
        // protocol, and says what it was sent where that is not what it
        // asked for.
        let policy = Command::Policy {
            app: "mpv".to_owned(),
            policy: super::PolicyArgument::Deny,
        };

        for (command, answer, said) in [
            (
                Command::Sources,
                Response::Ok,
                "benshi: daemon answered a listing with Ok\n",
            ),
            (
                Command::Why,
                Response::Ok,
                "benshi: daemon answered why with Ok\n",
            ),
            (
                policy,
                Response::Why(None),
                "benshi: daemon answered a policy change with Why(None)\n",
            ),
        ] {
            let (_home, socket, answering) = a_daemon_that_answers_with(&answer);

            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE);
            assert!(shown.is_empty(), "printed {shown:?}");
            assert_eq!(complained, said);
        }
    }

    #[tokio::test]
    async fn a_stream_that_carries_another_answer_is_a_failure_that_names_it() {
        // After the answer that it is on, a stream carries events, and word
        // of a lag or of an error. Any other answer is named and not printed.
        for (answer, named) in answers_no_stream_carries() {
            let (_home, socket, answering) = a_daemon_whose_stream_carries(&answer);

            let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Watch, &socket))
                .await
                .expect("the watch ends at the answer");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE, "went on past {named}");
            assert!(shown.is_empty(), "printed {shown:?}");
            assert_eq!(
                complained,
                format!("benshi: daemon sent {named} on a stream\n")
            );
        }
    }

    #[tokio::test]
    async fn a_recording_whose_stream_carries_another_answer_is_a_failure_that_names_it() {
        // A recorder that passed the answer over would go on recording, and
        // neither the file nor the summary would say what the daemon had
        // sent.
        for (answer, named) in answers_no_stream_carries() {
            let (home, socket, answering) = a_daemon_whose_stream_carries(&answer);
            let recording = Command::Record {
                duration: Duration::from_secs(60),
                output: home.path().join("trace.jsonl"),
            };

            let (outcome, shown, complained) = timeout(PATIENCE, go(recording, &socket))
                .await
                .expect("the recording ends at the answer");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE, "went on past {named}");
            assert!(shown.is_empty(), "printed {shown:?}");
            assert_eq!(
                complained,
                format!("benshi: daemon sent {named} on a stream\n")
            );
        }
    }

    #[tokio::test]
    async fn a_stream_that_carries_a_line_the_client_cannot_read_is_a_failure_that_names_it() {
        // A line that is no response at all, after the answer that the
        // stream is on. Passed over, or taken for the end of the stream, the
        // line and what was wrong with it would be lost.
        let line = "not a response";
        let mut on = line_of(&Response::Ok);
        on.extend_from_slice(format!("{line}\n").as_bytes());
        let (_home, socket, answering) = a_daemon_that_writes(on);
        let unread = serde_json::from_str::<Response>(line).expect_err("the line is no response");

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Watch, &socket))
            .await
            .expect("the client ended");
        answering.abort();

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "printed {shown:?}");
        assert_eq!(
            complained,
            format!("benshi: daemon sent a line this client cannot read: {line}: {unread}\n")
        );
    }

    #[tokio::test]
    async fn a_recording_is_a_trace_the_format_can_read() {
        // A recorder that assembled its own lines would write a file only it
        // could read, and nothing would say so until a replay was attempted
        // against a recording that can no longer be taken again.
        let daemon = Daemon::that_has_seen_the_player();
        let recording = daemon.recording(Duration::from_secs(60)).await;

        let published: Vec<_> = (0..3).map(a_reading).collect();
        for reading in &published {
            daemon.bus.publish(BusEvent::Snapshot(reading.clone()));
        }

        let trace = recording.once_it_holds(3).await;

        assert_eq!(trace.snapshots, published);
    }

    #[tokio::test]
    async fn a_reading_is_on_disk_before_the_recording_ends() {
        // A recorder killed after fifty seconds must leave fifty seconds of
        // recording, not an empty file. Everything else in these tests reads
        // the file while the command is still running, so this is the one that
        // says why that is allowed.
        let daemon = Daemon::that_has_seen_the_player();
        let recording = daemon.recording(Duration::from_secs(60)).await;

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));

        let trace = recording.holding(1).await;
        // Sampled after the wait rather than before it, so that what passes is
        // the file holding a reading while the command is still running, not
        // merely holding one by the time it had stopped.
        let still_going = !recording.running.is_finished();
        recording.stop();

        assert!(still_going, "the recording had already ended");
        assert_eq!(trace.snapshots, vec![a_reading(0)]);
    }

    #[tokio::test]
    async fn a_recording_keeps_readings_and_nothing_else() {
        // A trace is a sequence of readings. A membership change or a failure
        // is a fact about the daemon, not something a player reported, and a
        // replay has nothing to do with it. The state is the one to watch: it
        // carries a reading inside it, and writing that one down as well would
        // put a moment in the file the player only ever reported once.
        let daemon = Daemon::that_has_seen_the_player();
        let recording = daemon.recording(Duration::from_secs(60)).await;

        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId("mpv.instance1701".to_owned())],
        });
        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        daemon.bus.publish(BusEvent::SourceFailed {
            player: PlayerId("vlc".to_owned()),
            reason: "did not answer within 500ms".to_owned(),
        });
        daemon.bus.publish(BusEvent::State(Some(SessionState {
            snapshot: a_reading(9),
        })));
        daemon.bus.publish(BusEvent::Snapshot(a_reading(1)));

        let trace = recording.once_it_holds(2).await;

        assert_eq!(trace.snapshots, vec![a_reading(0), a_reading(1)]);
    }

    #[tokio::test]
    async fn a_recording_says_when_it_was_taken_in_a_form_that_reads_back() {
        // The header calls this an ISO-8601 instant. Nothing on the way in
        // enforces that, so the one place it can be held is here, where the
        // value is produced.
        let daemon = Daemon::listening(Vec::new());
        let recording = daemon.recording(Duration::from_millis(1)).await;
        let output = recording.output.clone();

        let (_outcome, _shown, _complained) = recording.until_it_stops().await;

        let text = std::fs::read_to_string(&output).expect("a trace was written");
        let trace = Trace::from_jsonl(&text).expect("a trace was written");
        humantime::parse_rfc3339(&trace.header.recorded_at)
            .unwrap_or_else(|_| panic!("not a readable instant: {:?}", trace.header.recorded_at));
    }

    #[tokio::test]
    async fn a_recording_declares_the_sources_the_daemon_had_when_it_started() {
        // The half of an observation a reading cannot carry. A reading says
        // what one source reported; only the daemon's listing says what that
        // source was able to report, and a replay has to answer both. A
        // recorder that wrote readings alone would leave every later replay to
        // work capabilities out from the readings, which is a guess.
        //
        // Feishin is the point of the second source: it produced nothing and is
        // declared anyway, because the listing records what was there rather
        // than what the file has readings from.
        let declared = vec![
            a_source("mpv.instance1701", "mpv"),
            a_source("Feishin", "Feishin"),
        ];
        let daemon = Daemon::listening(declared.clone());
        let recording = daemon.recording(Duration::from_secs(60)).await;

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        let trace = recording.once_it_holds(1).await;

        assert_eq!(trace.header.sources, declared);
        assert_eq!(trace.snapshots, vec![a_reading(0)]);
    }

    #[tokio::test]
    async fn a_recording_names_a_source_that_outran_the_listing_it_declared() {
        // The header is written once, before the first reading, so a player
        // opened during a recording is never declared. The file is a true
        // record of what the daemon published and it parses, so the summary is
        // the only place that can say it is not one a replay can use. Saying
        // nothing would leave that to be discovered by a replay days later,
        // against a recording that cannot be taken again.
        let daemon = Daemon::listening(vec![a_source(PLAYER, "mpv")]);
        let recording = daemon.recording(Duration::from_secs(60)).await;
        let opened_later = PlayerSnapshot {
            player: PlayerId("vlc".to_owned()),
            ..a_reading(1)
        };

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        daemon.bus.publish(BusEvent::Snapshot(opened_later.clone()));
        let trace = recording.holding(2).await;
        daemon.goes_away();

        let (outcome, shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(trace.snapshots, vec![a_reading(0), opened_later]);
        // Read off the line that reports it rather than the whole output:
        // `tempfile` picks six random alphanumerics for the path, and a bare
        // `contains` over everything would pass on one run in sixty thousand
        // without the line being printed at all.
        let line = shown
            .lines()
            .find(|line| line.contains("more than it can replay"))
            .unwrap_or_else(|| panic!("nothing reported the undeclared source: {shown:?}"));

        assert!(
            line.contains("vlc"),
            "the source that was never declared is not named: {line:?}"
        );
        // The absence half: a declared source must not be named as one that
        // was not, or the line says nothing when it does appear.
        assert!(
            !line.contains(PLAYER),
            "a declared source was named as undeclared: {line:?}"
        );
    }

    #[tokio::test]
    async fn a_recording_stops_when_its_time_is_up() {
        // With nothing published, only the deadline can end it. A recorder
        // that waited for an event would never return on a quiet desktop,
        // which is exactly when a recording is most worth taking.
        let daemon = Daemon::listening(Vec::new());
        let recording = daemon.recording(Duration::from_millis(1)).await;
        let output = recording.output.clone();

        let (outcome, _shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        let text = std::fs::read_to_string(&output).expect("a trace was written");
        let trace = Trace::from_jsonl(&text).expect("a header alone is a trace");
        assert!(trace.snapshots.is_empty(), "{:?}", trace.snapshots);
    }

    /// How a summary ends where the daemon closed the connection before the
    /// recording's time was up. The test that this is said and the test that
    /// it is absent read the one text, because a test of absence passes on
    /// any text a summary never carries.
    const ENDED_EARLY: &str = " - daemon closed connection before time was up";

    #[tokio::test]
    async fn a_recording_ends_with_what_it_has_when_the_daemon_goes_away() {
        // The other way a recording ends, and it is not a failure: what
        // reached the file before the daemon went away is still what the
        // player was doing, and calling it a failure would have the user throw
        // away a recording that cannot be taken again. The deadline is a
        // minute off, so only the connection closing can end this one.
        let daemon = Daemon::that_has_seen_the_player();
        let recording = daemon.recording(Duration::from_secs(60)).await;
        let output = recording.output.clone();

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        let caught = recording.holding(1).await;
        daemon.goes_away();

        let (outcome, shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(
            shown.contains(ENDED_EARLY),
            "a recording that ended early reads as a complete one: {shown:?}"
        );
        // The absence half of the line a recording prints when a source outran
        // its listing. Here every reading came from the declared source, so
        // nothing may be said; a line that appears on every recording says
        // nothing on the one it was written for.
        assert!(
            !shown.contains("more than it can replay"),
            "a recording of a declared source apologised for an undeclared one: {shown:?}"
        );
        assert_eq!(caught.snapshots, vec![a_reading(0)]);
        let text = std::fs::read_to_string(&output).expect("a trace was written");
        assert_eq!(
            Trace::from_jsonl(&text).expect("a shorter trace is still a trace"),
            caught,
            "the file changed after the daemon went away"
        );
    }

    #[tokio::test]
    async fn a_recording_that_missed_readings_fails_rather_than_leaving_a_gap() {
        // The worst thing a recorder can produce is a trace with a hole in it,
        // because replay would then prove a timeline that never happened and
        // nothing anywhere would say so.
        let daemon = Daemon::forgetful();
        let recording = daemon.recording(Duration::from_secs(60)).await;

        for index in 0..8 {
            daemon.bus.publish(BusEvent::Snapshot(a_reading(index)));
        }

        let (outcome, _shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::FAILURE, "a gap was recorded in silence");
        assert!(complained.contains("missed"), "got {complained:?}");
    }

    #[tokio::test]
    async fn a_recording_reports_what_it_wrote_and_where() {
        // A recording that caught nothing and one that caught an episode look
        // the same until the file is opened, so the count is what says which
        // was taken. Matched with its word rather than as a bare digit, which
        // a temporary path can supply on its own.
        let daemon = Daemon::listening(Vec::new());
        let recording = daemon.recording(Duration::from_millis(1)).await;
        let output = recording.output.clone();

        let (_outcome, shown, _complained) = recording.until_it_stops().await;

        assert!(
            shown.contains("0 readings"),
            "the count is not shown: {shown:?}"
        );
        // The absence half. A recording that ran its full length must not
        // carry the caveat, or the caveat says nothing when it does appear.
        assert!(
            !shown.contains(ENDED_EARLY),
            "a complete recording apologised for ending early: {shown:?}"
        );
        assert!(
            shown.contains(&output.display().to_string()),
            "the path is not shown: {shown:?}"
        );
    }

    #[tokio::test]
    async fn a_recording_that_cannot_reach_a_daemon_leaves_no_file_behind() {
        // `carry_out` connects before it runs any command, so a socket with
        // nothing behind it fails before `record` is reached and leaves no
        // empty file.
        let (home, socket) = nowhere();
        let output = home.path().join("trace.jsonl");

        let (outcome, _shown, _complained) = go(
            Command::Record {
                duration: Duration::from_secs(60),
                output: output.clone(),
            },
            &socket,
        )
        .await;

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(!output.exists(), "left a trace of a recording never taken");
    }

    #[tokio::test]
    async fn a_failure_ends_with_a_newline_so_a_shell_prompt_starts_on_its_own_line() {
        let (_home, socket) = nowhere();

        let (_outcome, _shown, complained) = go(Command::Sources, &socket).await;

        assert!(complained.ends_with('\n'), "got {complained:?}");
    }

    #[tokio::test]
    async fn a_failure_that_cannot_be_said_is_a_failure_all_the_same() {
        let (_home, socket) = nowhere();
        let mut out = Shared::default();

        let outcome = run(
            Command::Sources,
            &socket,
            untyped,
            &mut out,
            &mut Unwritable,
        )
        .await;

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(out.text().is_empty(), "printed {:?}", out.text());
    }

    #[tokio::test]
    async fn a_listing_that_cannot_be_printed_is_a_failure_that_says_so() {
        let daemon = Daemon::listening(vec![a_source(PLAYER, "mpv")]);
        let mut err = Shared::default();

        let outcome = run(
            Command::Sources,
            &daemon.socket,
            untyped,
            &mut Unwritable,
            &mut err,
        )
        .await;

        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            err.text(),
            "benshi: listing could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn an_explanation_that_cannot_be_printed_is_a_failure_that_says_so() {
        let daemon = Daemon::listening(Vec::new());
        let mut err = Shared::default();

        let outcome = run(
            Command::Why,
            &daemon.socket,
            untyped,
            &mut Unwritable,
            &mut err,
        )
        .await;

        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            err.text(),
            "benshi: explanation could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn an_event_that_cannot_be_printed_ends_the_stream_as_a_failure_that_says_so() {
        let daemon = Daemon::listening(Vec::new());
        let err = Shared::default();
        let socket = daemon.socket.clone();
        let watching = tokio::spawn({
            let mut err = err.clone();
            async move { run(Command::Watch, &socket, untyped, &mut Unwritable, &mut err).await }
        });
        daemon.subscribed().await;

        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId(PLAYER.to_owned())],
        });

        let outcome = timeout(PATIENCE, watching)
            .await
            .expect("the stream ended")
            .expect("the stream did not panic");
        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            err.text(),
            "benshi: stream could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn an_event_that_cannot_be_flushed_ends_the_stream_as_a_failure_that_says_so() {
        let daemon = Daemon::listening(Vec::new());
        let err = Shared::default();
        let socket = daemon.socket.clone();
        let watching = tokio::spawn({
            let mut err = err.clone();
            async move { run(Command::Watch, &socket, untyped, &mut Unflushable, &mut err).await }
        });
        daemon.subscribed().await;

        daemon.bus.publish(BusEvent::SourcesChanged {
            sources: vec![PlayerId(PLAYER.to_owned())],
        });

        let outcome = timeout(PATIENCE, watching)
            .await
            .expect("the stream ended")
            .expect("the stream did not panic");
        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            err.text(),
            "benshi: stream could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn a_second_line_of_a_summary_that_cannot_be_printed_is_a_failure_that_says_so() {
        // No source is declared, so the one reading comes from a source that
        // outran the listing, and the summary has a second line to say so.
        let daemon = Daemon::listening(Vec::new());
        let err = Shared::default();
        let output = daemon.home.path().join("trace.jsonl");
        let socket = daemon.socket.clone();
        let command = Command::Record {
            duration: Duration::from_secs(60),
            output: output.clone(),
        };
        let running = tokio::spawn({
            let mut err = err.clone();
            async move {
                let mut out = WritableForALine::default();
                run(command, &socket, untyped, &mut out, &mut err).await
            }
        });
        daemon.subscribed().await;
        let recording = Recording {
            running,
            output,
            out: Shared::default(),
            err,
        };

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        recording.holding(1).await;
        daemon.goes_away();
        let (outcome, _shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            complained,
            "benshi: summary could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn a_summary_that_cannot_be_printed_is_a_failure_that_says_so() {
        let daemon = Daemon::listening(Vec::new());
        let mut err = Shared::default();
        let recording = Command::Record {
            duration: Duration::from_millis(1),
            output: daemon.home.path().join("trace.jsonl"),
        };

        let outcome = timeout(
            PATIENCE,
            run(
                recording,
                &daemon.socket,
                untyped,
                &mut Unwritable,
                &mut err,
            ),
        )
        .await
        .expect("the recording ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert_eq!(
            err.text(),
            "benshi: summary could not be printed: broken pipe\n"
        );
    }

    #[tokio::test]
    async fn a_login_asks_for_a_token_and_says_whose_it_is() {
        let daemon = Daemon::logging_in(Logins::default());

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Login, &daemon.socket))
            .await
            .expect("the client ended");

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(complained, ASKED);
        assert_eq!(
            shown,
            "logged in to AniList as someone, token valid until 2030-03-17 17:46 UTC\n"
        );
        assert_eq!(daemon.accounts.asked(), [format!("login {PASTED}")]);
    }

    #[tokio::test]
    async fn a_token_is_sent_without_the_space_pasted_around_it() {
        let daemon = Daemon::logging_in(Logins::default());

        let (outcome, _shown, complained) = timeout(
            PATIENCE,
            go_typing(
                Command::Login,
                &daemon.socket,
                Typed::Line(Token::new(format!(" \t{PASTED} \r"))),
            ),
        )
        .await
        .expect("the client ended");

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(daemon.accounts.asked(), [format!("login {PASTED}")]);
    }

    #[tokio::test]
    async fn nothing_pasted_is_no_login() {
        let daemon = Daemon::logging_in(Logins::default());

        for nothing in ["", " \t "] {
            let (outcome, shown, complained) = timeout(
                PATIENCE,
                go_typing(
                    Command::Login,
                    &daemon.socket,
                    Typed::Line(Token::new(nothing)),
                ),
            )
            .await
            .expect("the client ended");

            assert_eq!(outcome, ExitCode::FAILURE, "{nothing:?}");
            assert!(shown.is_empty(), "{shown}");
            assert_eq!(complained, format!("{ASKED}benshi: no token given\n"));
        }
        assert!(daemon.accounts.asked().is_empty());
    }

    #[tokio::test]
    async fn a_login_cancelled_at_the_prompt_is_no_login() {
        let daemon = Daemon::logging_in(Logins::default());

        let (outcome, shown, complained) = timeout(
            PATIENCE,
            go_typing(Command::Login, &daemon.socket, Typed::Cancelled),
        )
        .await
        .expect("the client ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "{shown}");
        assert_eq!(complained, format!("{ASKED}benshi: login cancelled\n"));
        assert!(daemon.accounts.asked().is_empty());
    }

    #[tokio::test]
    async fn a_token_that_cannot_be_read_is_no_login_and_says_why() {
        let daemon = Daemon::logging_in(Logins::default());
        let mut out = Shared::default();
        let mut err = Shared::default();

        let outcome = timeout(
            PATIENCE,
            run(
                Command::Login,
                &daemon.socket,
                || Err(std::io::Error::other("the terminal went away")),
                &mut out,
                &mut err,
            ),
        )
        .await
        .expect("the client ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(out.text().is_empty(), "{}", out.text());
        assert_eq!(
            err.text(),
            format!("{ASKED}benshi: token could not be read: the terminal went away\n")
        );
        assert!(daemon.accounts.asked().is_empty());
    }

    #[tokio::test]
    async fn a_login_the_daemon_could_not_carry_out_says_why() {
        let daemon = Daemon::logging_in(Logins::failing());

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Login, &daemon.socket))
            .await
            .expect("the client ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "{shown}");
        assert_eq!(
            complained,
            format!("{ASKED}benshi: token could not be kept: secret store is locked\n")
        );
    }

    /// A socket served by something that answers each request it reads with
    /// the answer that it is read, and with `then` once `after` has passed.
    fn a_daemon_that_answers_late(
        after: Duration,
        then: &Response,
    ) -> (TempDir, PathBuf, JoinHandle<()>) {
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let first = line_of(&Response::Ok);
        let then = line_of(then);
        let answering = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.expect("a client arrived");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut requests = BufReader::new(reader).lines();
                while let Ok(Some(_request)) = requests.next_line().await {
                    writer.write_all(&first).await.expect("the client reads");
                    tokio::time::sleep(after).await;
                    writer.write_all(&then).await.expect("the client reads");
                }
            }
        });

        (home, socket, answering)
    }

    #[tokio::test(start_paused = true)]
    async fn a_login_waits_past_the_deadline_for_what_it_comes_to() {
        // The secret store may ask a person to unlock it, and the daemon
        // waits for them. The answer that the login is read is what comes
        // within the deadline.
        let (_home, socket, answering) = a_daemon_that_answers_late(
            DEADLINE * 3,
            &Response::LoggedIn {
                name: "someone".to_owned(),
                expires: None,
            },
        );

        let (outcome, shown, complained) = settled(started(Command::Login, &socket)).await;
        answering.abort();

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(
            shown,
            "logged in to AniList as someone; token does not say when it runs out\n"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_logout_waits_past_the_deadline_for_what_it_comes_to() {
        let (_home, socket, answering) =
            a_daemon_that_answers_late(DEADLINE * 3, &Response::LoggedOut);

        let (outcome, shown, complained) = settled(started(Command::Logout, &socket)).await;
        answering.abort();

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(shown, "AniList token removed\n");
    }

    #[tokio::test]
    async fn a_login_or_a_logout_answered_with_another_kind_of_answer_says_so() {
        for command in [Command::Login, Command::Logout] {
            let asked = asked_first(&command);
            let named = if command == Command::Login {
                "a login"
            } else {
                "a logout"
            };
            let (_home, socket, answering) =
                a_daemon_that_answers_late(Duration::ZERO, &Response::Why(None));

            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE);
            assert!(shown.is_empty(), "{shown}");
            assert_eq!(
                complained,
                format!("{asked}benshi: daemon answered {named} with Why(None)\n")
            );
        }
    }

    #[tokio::test]
    async fn a_login_or_a_logout_refused_at_once_says_why() {
        for command in [Command::Login, Command::Logout] {
            let asked = asked_first(&command);
            let (_home, socket, answering) = a_daemon_that_answers_with(&Response::Error {
                message: "not today".to_owned(),
            });

            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE);
            assert!(shown.is_empty(), "{shown}");
            assert_eq!(complained, format!("{asked}benshi: not today\n"));
        }
    }

    #[tokio::test]
    async fn a_logout_says_the_token_is_removed() {
        let daemon = Daemon::logging_in(Logins::default());

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Logout, &daemon.socket))
            .await
            .expect("the client ended");

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(complained.is_empty(), "{complained}");
        assert_eq!(shown, "AniList token removed\n");
        assert_eq!(daemon.accounts.asked(), ["logout"]);
    }

    #[tokio::test]
    async fn a_logout_the_daemon_could_not_carry_out_says_why() {
        let daemon = Daemon::logging_in(Logins::failing());

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Logout, &daemon.socket))
            .await
            .expect("the client ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(shown.is_empty(), "{shown}");
        assert_eq!(
            complained,
            "benshi: token could not be removed: secret store is locked\n"
        );
    }

    /// A socket served by something that answers each request it reads with
    /// the answer that it is read, and closes.
    fn a_daemon_that_reads_and_goes() -> (TempDir, PathBuf, JoinHandle<()>) {
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let first = line_of(&Response::Ok);
        let answering = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await.expect("a client arrived");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut requests = BufReader::new(reader).lines();
                if let Ok(Some(_request)) = requests.next_line().await {
                    writer.write_all(&first).await.expect("the client reads");
                }
            }
        });

        (home, socket, answering)
    }

    #[tokio::test]
    async fn a_login_or_a_logout_the_daemon_reads_and_goes_away_from_says_so() {
        for command in [Command::Login, Command::Logout] {
            let asked = asked_first(&command);
            let (_home, socket, answering) = a_daemon_that_reads_and_goes();

            let (outcome, shown, complained) = timeout(PATIENCE, go(command, &socket))
                .await
                .expect("the client ended");
            answering.abort();

            assert_eq!(outcome, ExitCode::FAILURE);
            assert!(shown.is_empty(), "{shown}");
            assert_eq!(complained, format!("{asked}{CLOSED_WITHOUT_ANSWERING}\n"));
        }
    }

    #[tokio::test]
    async fn a_login_or_a_logout_that_cannot_be_printed_says_so() {
        let daemon = Daemon::logging_in(Logins::default());

        for (command, said) in [
            (
                Command::Login,
                format!("{ASKED}benshi: login could not be printed: broken pipe\n"),
            ),
            (
                Command::Logout,
                "benshi: logout could not be printed: broken pipe\n".to_owned(),
            ),
        ] {
            let mut err = Shared::default();
            let outcome = timeout(
                PATIENCE,
                run(
                    command,
                    &daemon.socket,
                    || Ok(Typed::Line(Token::new(PASTED))),
                    &mut Unwritable,
                    &mut err,
                ),
            )
            .await
            .expect("the client ended");

            assert_eq!(outcome, ExitCode::FAILURE);
            assert_eq!(err.text(), said);
        }
    }

    #[tokio::test]
    async fn a_login_whose_prompt_cannot_be_printed_sends_nothing() {
        // Nobody can be told where to find a token, so nothing pasted can be
        // taken for one.
        let daemon = Daemon::logging_in(Logins::default());

        let outcome = timeout(
            PATIENCE,
            run(
                Command::Login,
                &daemon.socket,
                || Ok(Typed::Line(Token::new(PASTED))),
                &mut Shared::default(),
                &mut Unwritable,
            ),
        )
        .await
        .expect("the client ended");

        assert_eq!(outcome, ExitCode::FAILURE);
        assert!(daemon.accounts.asked().is_empty());
    }

    /// A socket served by something that closes the first connection it takes
    /// without reading from it, and answers each request on the connections
    /// after it with the answer that it is read and with `then`.
    fn a_daemon_that_closes_the_first_connection(
        then: &Response,
    ) -> (TempDir, PathBuf, JoinHandle<()>) {
        let (home, socket, listener) = a_socket_bound_and_not_served();
        let first = line_of(&Response::Ok);
        let then = line_of(then);
        let answering = tokio::spawn(async move {
            let (unread, _peer) = listener.accept().await.expect("a client arrived");
            drop(unread);
            loop {
                let (stream, _peer) = listener.accept().await.expect("a client arrived");
                let (reader, mut writer) = tokio::io::split(stream);
                let mut requests = BufReader::new(reader).lines();
                while let Ok(Some(_request)) = requests.next_line().await {
                    writer.write_all(&first).await.expect("the client reads");
                    writer.write_all(&then).await.expect("the client reads");
                }
            }
        });

        (home, socket, answering)
    }

    #[tokio::test]
    async fn a_login_sends_its_token_on_a_connection_of_its_own() {
        // The first connection says that a daemon is there, and a person
        // takes longer to paste than its deadline gives. This daemon closes
        // it unread.
        let (_home, socket, answering) =
            a_daemon_that_closes_the_first_connection(&Response::LoggedIn {
                name: "someone".to_owned(),
                expires: None,
            });

        let (outcome, shown, complained) = timeout(PATIENCE, go(Command::Login, &socket))
            .await
            .expect("the client ended");
        answering.abort();

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert_eq!(
            shown,
            "logged in to AniList as someone; token does not say when it runs out\n"
        );
    }
}
