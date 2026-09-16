//! What each command does, and what the process should exit with.
//!
//! A command opens one connection, sends one request and renders what comes
//! back: one answer for `sources` and `policy`, and a stream for `watch` until
//! the daemon or the user ends it.
//!
//! A failure is reported here rather than returned. This is where a `Result`
//! stops being something a caller can act on and becomes a line on the error
//! stream and a status a shell can test.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use benshi_core::AppName;
use benshi_core::policy::Policy;
use benshi_daemon::protocol::{Request, Response};
use clap::{Subcommand, ValueEnum};

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

#[cfg(test)]
mod tests {
    use super::{Command, run};
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::{AppName, Capabilities, PlayState, PlayerId};
    use benshi_daemon::bus::{BusEvent, EventBus};
    use benshi_daemon::detection::Seen;
    use benshi_daemon::ipc::{Server, bind};
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
        _home: TempDir,
        socket: PathBuf,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
    }

    impl Daemon {
        fn listening(sources: Vec<SourceInfo>) -> Self {
            let home = tempfile::tempdir().expect("a directory of our own");
            let socket = home.path().join("run").join("benshi.sock");
            let listener = bind(&socket).expect("the socket binds");

            let bus = Arc::new(EventBus::new());
            let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
            let seen = Seen::new();
            seen.record(sources);
            let server = Arc::new(Server::new(Arc::clone(&bus), Arc::clone(&policy), seen));

            drop(tokio::spawn(server.listen(listener)));

            Self {
                _home: home,
                socket,
                bus,
                policy,
            }
        }

        fn policy_for(&self, app: &str) -> Policy {
            self.policy
                .read()
                .expect("the table")
                .policy_for(&AppName(app.to_owned()))
        }

        /// A `watch` against this daemon, with the streams it is writing to.
        ///
        /// Returns once the daemon has the subscription, so that an event
        /// published afterwards cannot be missed by a client that had not
        /// arrived yet.
        async fn watching(&self) -> (JoinHandle<ExitCode>, Shared, Shared) {
            let out = Shared::default();
            let err = Shared::default();
            let socket = self.socket.clone();
            let watching = tokio::spawn({
                let mut out = out.clone();
                let mut err = err.clone();
                async move { run(Command::Watch, &socket, &mut out, &mut err).await }
            });

            timeout(PATIENCE, async {
                while self.bus.subscribers() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the client subscribed");

            (watching, out, err)
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
    async fn a_failure_ends_with_a_newline_so_a_shell_prompt_starts_on_its_own_line() {
        let (_home, socket) = nowhere();

        let (_outcome, _shown, complained) = go(Command::Sources, &socket).await;

        assert!(complained.ends_with('\n'), "got {complained:?}");
    }
}
