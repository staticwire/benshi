//! The client: speaks to a running daemon over a local socket.
//!
//! Every command exists to make a decision inspectable. `benshi sources` lists
//! each known source with its policy, state and capabilities, which retires the
//! whole class of "why does it not see my player". `benshi policy` changes what
//! the daemon does with one of them, and takes effect without a restart.
//! `benshi watch` prints the daemon's own event stream, so the log a user can
//! read is the one the program acts on rather than a second one written for the
//! occasion. `benshi why` prints the most recent recognition decision: what the
//! name spelled, what was answered, and for a match which stage decided it and
//! by what. `benshi login` has the daemon ask AniList whose a token is and keep
//! it in the secret store, and `benshi logout` has it removed. `benshi map`
//! makes a name mean one entry of the list, for a name the list spells for
//! several entries or for none, and keeps episodes of a show as episodes of
//! an entry; alone, it lists the names given and the episodes mapped.
//!
//! The client renders; it never decides. Anything it shows came from the daemon
//! in that answer, so two runs of `benshi sources` a second apart can differ and
//! neither is the client's opinion.

pub mod client;

pub mod commands;

pub mod prompt;

pub mod render;

use std::fmt;
use std::io::{self, Write};
use std::os::fd::AsFd;
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

/// Says a line on stderr, and goes on where it cannot be written.
///
/// `eprintln!` panics where the write fails, as one does to a pipe whose
/// reader has gone. A client that ended there would answer nothing, and
/// would end with the code of a panic where it had failed.
fn say(line: fmt::Arguments<'_>) {
    drop(writeln!(io::stderr(), "benshi: {line}"));
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
    // The XDG Base Directory Specification asks for a warning where the
    // runtime directory is replaced. A socket named on the command line is
    // looked for where it was named, whatever the session sets.
    if arguments.socket.is_none()
        && let Some(fallback) = benshi_daemon::ipc::fallback()
    {
        say(format_args!("{fallback}"));
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(unbuilt) => {
            say(format_args!("runtime could not be started: {unbuilt}"));
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(commands::run(
        arguments.command,
        &socket,
        || prompt::hidden(io::stdin().as_fd()),
        &mut io::stdout(),
        &mut io::stderr(),
    ))
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use crate::commands::{Command, EntryArgument, Ep, PolicyArgument};
    use benshi_core::pin::Episodes;
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
        assert!(Cli::try_parse_from(["benshi", "recognize"]).is_err());
    }

    #[test]
    fn why_takes_no_argument() {
        // The daemon keeps one decision, the most recent, so there is nothing
        // to name: the question is always about that one.
        let asked = Cli::try_parse_from(["benshi", "why"]).expect("parses");

        assert_eq!(asked.command, Command::Why);
        assert!(Cli::try_parse_from(["benshi", "why", "some.mkv"]).is_err());
    }

    #[test]
    fn login_and_logout_take_no_argument() {
        // A token is pasted at a prompt. One typed on the command line would
        // stay in the shell's history.
        let login = Cli::try_parse_from(["benshi", "login"]).expect("parses");
        let logout = Cli::try_parse_from(["benshi", "logout"]).expect("parses");

        assert_eq!(login.command, Command::Login);
        assert_eq!(logout.command, Command::Logout);
        assert!(Cli::try_parse_from(["benshi", "login", "a.pasted.token"]).is_err());
        assert!(Cli::try_parse_from(["benshi", "logout", "anilist"]).is_err());
    }

    #[test]
    fn map_alone_lists_the_names_given() {
        let asked = Cli::try_parse_from(["benshi", "map"]).expect("parses");

        assert_eq!(
            asked.command,
            Command::Map {
                name: None,
                ep: None,
                episodes: None,
                anilist: None,
                remove: false,
            }
        );
    }

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run from its first episode to its last")
    }

    #[test]
    fn a_name_is_mapped_to_an_entry_by_its_number_on_anilist() {
        let asked = Cli::try_parse_from(["benshi", "map", "Show Title", "--anilist", "12345"])
            .expect("parses");

        assert_eq!(
            asked.command,
            Command::Map {
                name: Some("Show Title".to_owned()),
                ep: None,
                episodes: None,
                anilist: Some(EntryArgument {
                    entry: 12345,
                    episodes: None,
                }),
                remove: false,
            }
        );
    }

    #[test]
    fn a_name_is_taken_back_with_remove() {
        let asked =
            Cli::try_parse_from(["benshi", "map", "Show Title", "--remove"]).expect("parses");

        assert_eq!(
            asked.command,
            Command::Map {
                name: Some("Show Title".to_owned()),
                ep: None,
                episodes: None,
                anilist: None,
                remove: true,
            }
        );
    }

    #[test]
    fn episodes_of_a_show_are_mapped_after_ep_to_episodes_of_an_entry_after_a_colon() {
        let asked = Cli::try_parse_from([
            "benshi",
            "map",
            "Show Title",
            "ep",
            "14-26",
            "--anilist",
            "12345:1-13",
        ])
        .expect("parses");

        assert_eq!(
            asked.command,
            Command::Map {
                name: Some("Show Title".to_owned()),
                ep: Some(Ep::Ep),
                episodes: Some(run(14, 26)),
                anilist: Some(EntryArgument {
                    entry: 12345,
                    episodes: Some(run(1, 13)),
                }),
                remove: false,
            }
        );
    }

    #[test]
    fn one_episode_is_typed_as_its_number() {
        let asked = Cli::try_parse_from([
            "benshi",
            "map",
            "Show Title",
            "ep",
            "2",
            "--anilist",
            "678:1",
        ])
        .expect("parses");

        let Command::Map {
            episodes, anilist, ..
        } = asked.command
        else {
            panic!("expected a map, got {:?}", asked.command);
        };
        assert_eq!(episodes, Some(run(2, 2)));
        assert_eq!(
            anilist,
            Some(EntryArgument {
                entry: 678,
                episodes: Some(run(1, 1)),
            })
        );
    }

    #[test]
    fn episodes_mapped_are_taken_back_with_remove() {
        let asked = Cli::try_parse_from(["benshi", "map", "Show Title", "ep", "14-26", "--remove"])
            .expect("parses");

        assert_eq!(
            asked.command,
            Command::Map {
                name: Some("Show Title".to_owned()),
                ep: Some(Ep::Ep),
                episodes: Some(run(14, 26)),
                anilist: None,
                remove: true,
            }
        );
    }

    #[test]
    fn a_map_that_asks_for_no_one_thing_is_refused() {
        // A name alone could be either request, a flag alone acts on nothing,
        // and episodes are one number or two joined by a dash, in order.
        for refused in [
            &["benshi", "map", "Show Title"][..],
            &["benshi", "map", "--anilist", "12345"],
            &["benshi", "map", "--remove"],
            &[
                "benshi",
                "map",
                "Show Title",
                "--anilist",
                "12345",
                "--remove",
            ],
            &["benshi", "map", "Show Title", "--anilist", "soon"],
            &["benshi", "map", "Show Title", "ep", "14-26"],
            &["benshi", "map", "Show Title", "ep", "--remove"],
            &["benshi", "map", "Show Title", "14-26", "--remove"],
            &["benshi", "map", "Show Title", "ep", "26-14", "--remove"],
            &["benshi", "map", "Show Title", "ep", "two", "--remove"],
            &["benshi", "map", "Show Title", "ep", "-2", "--remove"],
            &["benshi", "map", "Show Title", "--anilist", "12345:13-1"],
            &["benshi", "map", "Show Title", "--anilist", "12345:"],
            &["benshi", "map", "Show Title", "--anilist", ":1-13"],
            &[
                "benshi",
                "map",
                "Show Title",
                "ep",
                "14-26",
                "--anilist",
                "12345:1-13",
                "--remove",
            ],
        ] {
            assert!(Cli::try_parse_from(refused).is_err(), "{refused:?}");
        }
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
