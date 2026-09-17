//! What each command does, and what the process should exit with.
//!
//! A command opens one connection, sends one request and renders what comes
//! back: one answer for `sources` and `policy`, and a stream for `watch` until
//! the daemon or the user ends it.
//!
//! A failure is reported here rather than returned. This is where a `Result`
//! stops being something a caller can act on and becomes a line on the error
//! stream and a status a shell can test.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use benshi_core::AppName;
use benshi_core::policy::Policy;
use benshi_core::trace::{TraceHeader, header_line, snapshot_line};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::protocol::{Request, Response};
use clap::{Subcommand, ValueEnum};
use humantime::format_rfc3339_seconds;

use crate::client::Daemon;
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
    out: &mut impl Write,
    err: &mut impl Write,
) -> ExitCode {
    match carry_out(command, socket, out).await {
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
async fn carry_out(command: Command, socket: &Path, out: &mut impl Write) -> Result<()> {
    let mut daemon = Daemon::connect(socket).await?;

    match command {
        Command::Sources => sources(&mut daemon, out).await,
        Command::Watch => watch(&mut daemon, out).await,
        Command::Policy { app, policy } => set_policy(&mut daemon, &app, policy.into()).await,
        Command::Record { duration, output } => record(&mut daemon, duration, &output, out).await,
    }
}

/// Print every source the daemon has seen.
async fn sources(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    daemon.ask(&Request::Sources).await?;

    match daemon.expect_answer().await? {
        Response::Sources(listed) => {
            write!(out, "{}", render::listing(&listed)).context("the listing could not be printed")
        }
        Response::Error { message } => bail!("{message}"),
        unexpected => bail!("the daemon answered a listing with {unexpected:?}"),
    }
}

/// Print the daemon's events until it stops sending them.
///
/// Ends when the daemon closes the connection. A client run from a terminal is
/// ended by the user instead, which the daemon sees as the connection going
/// away and treats as the ordinary end of a stream.
async fn watch(daemon: &mut Daemon, out: &mut impl Write) -> Result<()> {
    daemon.ask(&Request::Watch).await?;

    while let Some(response) = daemon.answer().await? {
        let line = match response {
            Response::Event(happened) => render::event(&happened),
            // Said rather than passed over. A gap nobody reports reads as
            // nothing having happened, which on this stream is the one wrong
            // answer it can give.
            Response::Lagged { missed } => format!("... {missed} events missed ..."),
            Response::Error { message } => bail!("{message}"),
            unexpected => bail!("the daemon sent {unexpected:?} on a stream"),
        };

        writeln!(out, "{line}").context("the stream could not be printed")?;
        out.flush().context("the stream could not be printed")?;
    }

    Ok(())
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
        unexpected => bail!("the daemon answered a policy change with {unexpected:?}"),
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
    duration: Duration,
    output: &Path,
    out: &mut impl Write,
) -> Result<()> {
    daemon.ask(&Request::Watch).await?;

    // The subscription goes out before the file is made, so a request that
    // cannot be sent leaves no empty trace behind. A socket with nothing
    // listening on it never gets this far: `carry_out` connects before it runs
    // any command at all.
    let header = TraceHeader::new(format_rfc3339_seconds(SystemTime::now()).to_string());
    let mut file =
        File::create(output).with_context(|| format!("{} is not writable", output.display()))?;
    append(&mut file, &header_line(&header)?, output)?;

    let mut readings = 0_usize;
    // One deadline over the whole recording rather than one per answer. A
    // quiet desktop sends nothing for minutes, and a per-answer timeout would
    // end the recording instead of recording the quiet.
    let recording = tokio::time::timeout(duration, async {
        while let Some(response) = daemon.answer().await? {
            match response {
                Response::Event(BusEvent::Snapshot(reading)) => {
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
                unexpected => bail!("the daemon sent {unexpected:?} on a stream"),
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
        " - the daemon closed the connection before the time was up"
    } else {
        ""
    };

    writeln!(
        out,
        "recorded {readings} readings to {}{why}",
        output.display()
    )
    .context("the summary could not be printed")
}

/// Put one line in the trace, where a reader finds it before the next arrives.
///
/// Written out a line at a time rather than gathered up and written at the end,
/// so that a recording interrupted after fifty seconds holds fifty seconds. A
/// `File` does not buffer in this process, so `write_all` alone already has
/// that property and the flush does nothing today. It is kept because deleting
/// it would leave the property resting on the type this argument happens to
/// have, and a `BufWriter` put here later would take it away silently.
fn append(file: &mut File, line: &str, output: &Path) -> Result<()> {
    file.write_all(line.as_bytes())
        .and_then(|()| file.flush())
        .with_context(|| format!("{} could not be written", output.display()))
}

#[cfg(test)]
mod tests {
    use super::{Command, run};
    use benshi_core::clock::{Clock, TestClock};
    use benshi_core::path::RawPath;
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::trace::Trace;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, SessionState,
    };
    use benshi_daemon::bus::{BusEvent, EventBus};
    use benshi_daemon::detection::Seen;
    use benshi_daemon::ipc::{Server, bind};
    use benshi_daemon::supervisor::TaskError;
    use benshi_detect::SourceInfo;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    /// How long a test waits before calling an answer lost.
    ///
    /// Only ever spent on a failure. What matters is that it exists: a command
    /// that never answers has to fail the run rather than hang it.
    const PATIENCE: Duration = Duration::from_secs(5);

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

    /// The `index`th reading of a recording, distinguishable from the others.
    fn a_reading(index: u64) -> PlayerSnapshot {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(index));

        PlayerSnapshot {
            player: PlayerId("mpv.instance1701".to_owned()),
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
                    tokio::task::yield_now().await;
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

    /// A daemon listening on a socket of its own.
    struct Daemon {
        home: TempDir,
        socket: PathBuf,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
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
            let home = tempfile::tempdir().expect("a directory of our own");
            let socket = home.path().join("run").join("benshi.sock");
            let listener = bind(&socket).expect("the socket binds");

            let bus = Arc::new(bus);
            let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
            let seen = Seen::new();
            seen.record(sources);
            let server = Arc::new(Server::new(Arc::clone(&bus), Arc::clone(&policy), seen));

            Self {
                home,
                socket,
                bus,
                policy,
                serving: tokio::spawn(server.listen(listener)),
            }
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
                    tokio::task::yield_now().await;
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
                async move { run(Command::Watch, &socket, &mut out, &mut err).await }
            });

            self.subscribed().await;

            (watching, out, err)
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
                async move { run(command, &socket, &mut out, &mut err).await }
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
                    tokio::task::yield_now().await;
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

    async fn go(command: Command, socket: &Path) -> (ExitCode, String, String) {
        let mut out = Shared::default();
        let mut err = Shared::default();
        let outcome = run(command, socket, &mut out, &mut err).await;

        (outcome, out.text(), err.text())
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
    async fn every_command_fails_when_no_daemon_is_listening() {
        let (_home, socket) = nowhere();

        for command in [
            Command::Sources,
            Command::Watch,
            Command::Policy {
                app: "mpv".to_owned(),
                policy: super::PolicyArgument::Deny,
            },
            Command::Record {
                duration: Duration::from_secs(60),
                output: socket.with_file_name("trace.jsonl"),
            },
        ] {
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

    #[tokio::test]
    async fn a_recording_is_a_trace_the_format_can_read() {
        // A recorder that assembled its own lines would write a file only it
        // could read, and nothing would say so until a replay was attempted
        // against a recording that can no longer be taken again.
        let daemon = Daemon::listening(Vec::new());
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
        let daemon = Daemon::listening(Vec::new());
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
        let daemon = Daemon::listening(Vec::new());
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

    #[tokio::test]
    async fn a_recording_ends_with_what_it_has_when_the_daemon_goes_away() {
        // The other way a recording ends, and it is not a failure: what
        // reached the file before the daemon went away is still what the
        // player was doing, and calling it a failure would have the user throw
        // away a recording that cannot be taken again. The deadline is a
        // minute off, so only the connection closing can end this one.
        let daemon = Daemon::listening(Vec::new());
        let recording = daemon.recording(Duration::from_secs(60)).await;
        let output = recording.output.clone();

        daemon.bus.publish(BusEvent::Snapshot(a_reading(0)));
        let caught = recording.holding(1).await;
        daemon.goes_away();

        let (outcome, shown, complained) = recording.until_it_stops().await;

        assert_eq!(outcome, ExitCode::SUCCESS, "stderr said {complained:?}");
        assert!(
            shown.contains("closed the connection"),
            "a recording that ended early reads as a complete one: {shown:?}"
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
            !shown.contains("closed the connection"),
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
        // empty file. That is the ordering this holds; the one inside `record`
        // would need a daemon that went away between the connection and the
        // subscription, which a test cannot stage.
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
}
