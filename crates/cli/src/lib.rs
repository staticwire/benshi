//! The client: speaks to a running daemon over a local socket.
//!
//! Every command exists to make a decision inspectable. `benshi sources` lists
//! each known source with its policy, state and capabilities, which retires the
//! whole class of "why does it not see my player". `benshi policy` changes what
//! the daemon does with one of them, and takes effect without a restart.
//! `benshi watch` prints the daemon's own event stream, so the log a user can
//! read is the one the program acts on rather than a second one written for the
//! occasion.
//!
//! The client renders; it never decides. Anything it shows came from the daemon
//! in that answer, so two runs of `benshi sources` a second apart can differ and
//! neither is the client's opinion.

pub mod client;

pub mod commands;

pub mod render;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::commands::Command;

/// What benshi was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "benshi", version, about = "A passive scrobbler")]
pub struct Cli {
    /// What to ask the daemon for.
    #[command(subcommand)]
    pub command: Command,

    /// The daemon's socket, when it is not where this session would put it.
    #[arg(long, value_name = "PATH", global = true)]
    pub socket: Option<PathBuf>,
}

impl Cli {
    /// Where to look for the daemon.
    ///
    /// The session's own path unless one was given, so that the client and the
    /// daemon agree without either being configured.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(benshi_daemon::ipc::socket_path)
    }
}

/// Run the client from a process that has no runtime yet.
///
/// A current-thread runtime, because a client is one connection awaited one
/// message at a time, whether that is a single answer or the whole of `watch`.
/// There is nothing for a second thread to do, and a pool would cost more to
/// start than the work it would be given.
#[must_use]
pub fn main(arguments: Cli) -> ExitCode {
    let socket = arguments.socket();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(failure) => {
            eprintln!("benshi: {failure}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(commands::run(
        arguments.command,
        &socket,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use crate::commands::{Command, PolicyArgument};
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn the_command_line_is_well_formed() {
        // clap's own check for a definition that would panic at runtime: a
        // repeated flag, a conflicting short, an argument with no name.
        Cli::command().debug_assert();
    }

    #[test]
    fn sources_takes_no_argument() {
        let asked = Cli::try_parse_from(["benshi", "sources"]).expect("parses");

        assert_eq!(asked.command, Command::Sources);
    }

    #[test]
    fn a_policy_is_typed_as_an_application_and_a_word() {
        let asked = Cli::try_parse_from(["benshi", "policy", "firefox", "allow"]).expect("parses");

        assert_eq!(
            asked.command,
            Command::Policy {
                app: "firefox".to_owned(),
                policy: PolicyArgument::Allow,
            }
        );
    }

    #[test]
    fn a_policy_that_is_not_one_of_the_three_is_refused() {
        // Three policies and no fourth. Accepting a word we do not know and
        // falling back to a default would set a policy the user did not ask
        // for and never told them.
        assert!(Cli::try_parse_from(["benshi", "policy", "firefox", "maybe"]).is_err());
    }

    #[test]
    fn a_command_that_does_not_exist_is_refused() {
        assert!(Cli::try_parse_from(["benshi", "recognise"]).is_err());
    }

    #[test]
    fn a_recording_is_typed_as_a_span_of_time_and_a_path() {
        let asked = Cli::try_parse_from([
            "benshi",
            "record",
            "--for",
            "90s",
            "--output",
            "one-episode.jsonl",
        ])
        .expect("parses");

        assert_eq!(
            asked.command,
            Command::Record {
                duration: Duration::from_secs(90),
                output: PathBuf::from("one-episode.jsonl"),
            }
        );
    }

    #[test]
    fn a_span_of_time_may_be_written_the_way_a_person_says_it() {
        // `--for 3600` would be seconds by an unwritten convention, and a
        // recording is a thing people describe in minutes.
        let asked =
            Cli::try_parse_from(["benshi", "record", "--for", "1h 30min", "-o", "long.jsonl"])
                .expect("parses");

        let Command::Record { duration, .. } = asked.command else {
            panic!("expected a recording, got {:?}", asked.command);
        };
        assert_eq!(duration, Duration::from_mins(90));
    }

    #[test]
    fn a_recording_with_no_span_or_no_path_is_refused() {
        // Neither has a default worth guessing: a recording of the wrong
        // length costs the time it took, and a path we chose is a file the
        // user has to go looking for.
        assert!(Cli::try_parse_from(["benshi", "record", "--for", "60s"]).is_err());
        assert!(Cli::try_parse_from(["benshi", "record", "--output", "t.jsonl"]).is_err());
        assert!(
            Cli::try_parse_from(["benshi", "record", "--for", "soon", "-o", "t.jsonl"]).is_err()
        );
    }

    #[test]
    fn without_a_socket_argument_the_client_looks_where_the_daemon_listens() {
        // The two agree without either being configured, which is the whole
        // reason the path is computed rather than written down.
        let asked = Cli::try_parse_from(["benshi", "sources"]).expect("parses");

        assert_eq!(asked.socket(), benshi_daemon::ipc::socket_path());
    }

    #[test]
    fn a_socket_named_on_the_command_line_is_the_one_used() {
        let asked = Cli::try_parse_from(["benshi", "--socket", "/tmp/elsewhere.sock", "sources"])
            .expect("parses");

        assert_eq!(asked.socket(), PathBuf::from("/tmp/elsewhere.sock"));
    }

    #[test]
    fn the_socket_argument_may_follow_the_subcommand() {
        // It is global, so a user who reaches for it after typing the command
        // does not have to retype the line.
        let asked = Cli::try_parse_from(["benshi", "sources", "--socket", "/tmp/elsewhere.sock"])
            .expect("parses");

        assert_eq!(asked.socket(), PathBuf::from("/tmp/elsewhere.sock"));
    }
}
