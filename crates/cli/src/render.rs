//! Turning what the daemon said into what a person reads.
//!
//! Every function here is pure, so what the daemon's answers look like on a
//! terminal is testable without a socket, a daemon or a platform. Nothing in
//! this module talks to anything.
//!
//! This is also where bytes become text. A filename travels as bytes from the
//! adapter that read it, through the bus and the socket, to here; only
//! something showing one to a person may guess what encoding those bytes were
//! in.

use std::fmt::Write as _;
use std::time::Duration;

use benshi_core::encoding::{Confidence, decode};
use benshi_core::policy::Policy;
use benshi_core::recognise::parse::{Episode, Parsed};
use benshi_core::{Capabilities, Known, MediaRef, PlayState};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::protocol::{DecidedBy, Explanation, Outcome, SourceListing};

/// What is shown for a source that cannot name what it opened.
///
/// A source that publishes no location gives recognition nothing but what the
/// page or window calls itself. That is a different problem from recognising a
/// filename, and a user deciding whether to allow such a source has to be told
/// which one they are getting.
const TITLE_ONLY: &str = "title only";

/// What is shown where a value could have been reported and was not.
const NOT_REPORTED: &str = "--:--";

/// What is shown where the source cannot carry the value at all.
///
/// Different from [`NOT_REPORTED`] on purpose. The domain keeps "did not say"
/// apart from "cannot say" from the adapter down; collapsing them here would
/// throw the distinction away at the last place it could still be seen.
const UNSUPPORTED: &str = "n/a";

/// What is shown when no reading has been decided yet.
const NOTHING_DECIDED: &str = "nothing has been decided yet\n";

/// What a source is able to report, and what that leaves recognition to work
/// from.
///
/// Two facts rather than one. The tokens say where playback has reached; the
/// trailing note says what is playing, and it is spelled out rather than left
/// to be inferred from an absent `loc`.
#[must_use]
pub fn capabilities(of: Capabilities) -> String {
    let declared: Vec<&str> = [
        (of.position, "pos"),
        (of.duration, "dur"),
        (of.paused, "pause"),
        (of.location, "loc"),
    ]
    .into_iter()
    .filter_map(|(declared, name)| declared.then_some(name))
    .collect();
    let tokens = declared.join("+");

    if of.location {
        // `loc` is already among the tokens, and the note would contradict it.
        return tokens;
    }
    if tokens.is_empty() {
        return TITLE_ONLY.to_owned();
    }

    format!("{tokens}, {TITLE_ONLY}")
}

/// What a source is doing, as far as it will say.
#[must_use]
pub fn state(of: PlayState) -> &'static str {
    match of {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        // Not "stopped", which reads as playback having been stopped. The fact
        // this state carries is that the source has nothing open at all.
        PlayState::Stopped => "idle",
    }
}

/// What the daemon does with readings from a source.
#[must_use]
pub fn policy(of: Policy) -> &'static str {
    match of {
        Policy::Auto => "auto",
        Policy::Allow => "allow",
        Policy::Deny => "deny",
    }
}

/// A moment within the media, or why there is none.
#[must_use]
pub fn moment(of: Known<Duration>) -> String {
    match of {
        Known::Value(reached) => {
            let seconds = reached.as_secs();
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        }
        Known::NotReported => NOT_REPORTED.to_owned(),
        Known::Unsupported => UNSUPPORTED.to_owned(),
    }
}

/// What a source has open, as text.
///
/// A path is bytes and is decoded here and nowhere earlier. A name that is not
/// valid UTF-8 is read through the encoding [`decode`] infers rather than
/// replaced, because a row of replacement characters is exactly what a user
/// watching this stream is trying to get behind.
#[must_use]
pub fn media(of: &MediaRef) -> String {
    match of {
        MediaRef::LocalFile(path) => decode(path.as_bytes()).text,
        MediaRef::Remote(address) => address.clone(),
        MediaRef::Title(title) => title.clone(),
    }
}

/// Every source the daemon knows about, one to a line.
///
/// Columns are padded to the widest entry rather than to a fixed width: an
/// identity carries whatever the platform put in it, and truncating it would
/// hide the part that tells two windows of one player apart.
#[must_use]
pub fn listing(sources: &[SourceListing]) -> String {
    if sources.is_empty() {
        return "no source is open\n".to_owned();
    }

    let identity = width(sources.iter().map(|source| source.player.0.as_str()));
    let application = width(sources.iter().map(|source| source.app.0.as_str()));
    let mut shown = String::new();

    for source in sources {
        writeln!(
            shown,
            "{:identity$}  {:application$}  {:5}  {:7}  {}",
            source.player.0,
            source.app.0,
            policy(source.policy),
            state(source.state),
            capabilities(source.capabilities)
        )
        .expect("a String accepts every write");
    }

    shown
}

/// The widest of a column's entries, in bytes.
///
/// Bytes, while `{:width$}` pads in characters. That makes a column wider than
/// it needs to be and never misaligns one: a name has at most as many
/// characters as bytes, so every entry is padded to the same count. What would
/// misalign a column is a glyph the terminal draws double width, which neither
/// count sees. A D-Bus bus name is ASCII by the specification, so on the one
/// platform benshi reads today neither arises.
fn width<'a>(entries: impl Iterator<Item = &'a str>) -> usize {
    entries.map(str::len).max().unwrap_or_default()
}

/// The most recent decision, or that there is none yet.
#[must_use]
pub fn why(of: Option<&Explanation>) -> String {
    of.map_or_else(|| NOTHING_DECIDED.to_owned(), explanation)
}

/// One decision, as a person reads it.
///
/// Whose file, what its name spelled, what was answered, and for a match the
/// stage that decided and what it decided by. The stage is printed by its name
/// and never by a number: a number computed from a position in the sequence is
/// the thing that moves when a stage is inserted.
#[must_use]
pub fn explanation(of: &Explanation) -> String {
    format!(
        "{}  {}\nspelled: {}\n{}",
        of.player,
        media(&of.media),
        spelled(&of.spelled),
        answer(&of.outcome)
    )
}

/// What recognition answered, as the lines below the parse.
///
/// Each line ends in a newline, and a match takes two: what it was recognised
/// as, and the stage that decided.
fn answer(of: &Outcome) -> String {
    match of {
        Outcome::Recognised {
            title,
            episode,
            stage,
        } => format!(
            "recognised: \"{title}\", {}\nstage: {}\n",
            episode_words(*episode),
            decided_by(stage)
        ),
        Outcome::Ambiguous { candidates } => {
            let quoted: Vec<String> = candidates
                .iter()
                .map(|candidate| format!("\"{candidate}\""))
                .collect();
            format!("ambiguous: {}\n", quoted.join(", "))
        }
        Outcome::Unrecognised { best: Some(score) } => {
            format!("unrecognised: the closest candidate scored {score:.2}\n")
        }
        Outcome::Unrecognised { best: None } => {
            "unrecognised: nothing to score against\n".to_owned()
        }
    }
}

/// What a name spelled, in words, and whether its bytes had to be inferred.
///
/// The encoding is mentioned only where it was inferred. A title recovered by
/// inference is a weaker input than one that could not have been anything
/// else, and the person reading this is the one who can tell whether the
/// guess was right.
fn spelled(of: &Parsed) -> String {
    let mut parts = vec![of.title.as_ref().map_or_else(
        || "no title".to_owned(),
        |title| format!("title \"{title}\""),
    )];
    if let Some(season) = of.season {
        parts.push(format!("season {season}"));
    }
    if let Some(part) = of.part {
        parts.push(format!("part {part}"));
    }
    parts.push(episode_words(of.episode));
    if let Some(year) = of.year {
        parts.push(format!("year {year}"));
    }
    if let Some(group) = &of.release_group {
        parts.push(format!("group \"{group}\""));
    }
    if of.confidence == Confidence::Detected {
        parts.push("encoding inferred".to_owned());
    }

    parts.join(", ")
}

/// An episode in words.
///
/// Only the first answer is a number. Several, a half and none are said as
/// such, because a number printed for any of them would read as an episode
/// somebody could record progress against.
fn episode_words(of: Episode) -> String {
    match of {
        Episode::Only(number) => format!("episode {number}"),
        Episode::Several => "several episodes".to_owned(),
        Episode::NotWhole => "an episode that is not a whole number".to_owned(),
        Episode::Absent => "no episode".to_owned(),
    }
}

/// The stage that decided, by its name, and what it decided by.
fn decided_by(of: &DecidedBy) -> String {
    match of {
        DecidedBy::Altname(assignment) => {
            let mut shown = format!("altname, assigned \"{}\"", assignment.spelled);
            if let Some(season) = assignment.season {
                write!(shown, ", season {season}").expect("a String accepts every write");
            }
            if let Some(part) = assignment.part {
                write!(shown, ", part {part}").expect("a String accepts every write");
            }
            shown
        }
        DecidedBy::Key => "key".to_owned(),
        DecidedBy::Scored(score) => format!("score {score:.2}"),
    }
}

/// One event from the daemon, as a line for a person.
///
/// The machine-readable form is what the socket already carries, so nothing is
/// lost by rendering here: `socat - UNIX-CONNECT:...` gives the JSON unchanged.
#[must_use]
pub fn event(of: &BusEvent) -> String {
    match of {
        BusEvent::Snapshot(snapshot) => format!(
            "{}  {}  {} / {}  {}",
            snapshot.player.0,
            state(snapshot.state),
            moment(snapshot.position),
            moment(snapshot.duration),
            media(&snapshot.media)
        ),
        BusEvent::SourcesChanged { sources } if sources.is_empty() => {
            "sources: no source is open".to_owned()
        }
        BusEvent::SourcesChanged { sources } => {
            let named: Vec<&str> = sources.iter().map(|player| player.0.as_str()).collect();
            format!("sources: {}", named.join(", "))
        }
        BusEvent::SourceFailed { player, reason } => format!("{}  failed: {reason}", player.0),
        BusEvent::State(Some(session)) => format!("state: {}", session.snapshot.player.0),
        BusEvent::State(None) => "state: nothing is playing".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{capabilities, event, explanation, listing, moment, state, why};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::Policy;
    use benshi_core::recognise::altname::Assignment;
    use benshi_core::recognise::parse::Episode;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_daemon::bus::BusEvent;
    use benshi_daemon::protocol::{DecidedBy, Explanation, Outcome, SourceListing};
    use benshi_daemon::recognition::Recogniser;
    // The reason a failure carries comes from here in the daemon, so the tests
    // that check how one reads take it from here too rather than writing one.
    use benshi_detect::WatchError;
    use std::time::Duration;

    /// A filename in Shift-JIS, which is not valid UTF-8.
    const SHIFT_JIS_NAME: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.mkv";

    fn everything() -> Capabilities {
        Capabilities {
            position: true,
            duration: true,
            paused: true,
            location: true,
        }
    }

    fn nothing() -> Capabilities {
        Capabilities {
            position: false,
            duration: false,
            paused: false,
            location: false,
        }
    }

    fn a_listing(identity: &str, app: &str, policy: Policy, of: Capabilities) -> SourceListing {
        SourceListing {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            policy,
            capabilities: of,
            state: PlayState::Playing,
        }
    }

    #[test]
    fn a_source_that_reports_everything_names_all_four() {
        assert_eq!(capabilities(everything()), "pos+dur+pause+loc");
    }

    #[test]
    fn a_source_that_cannot_name_what_it_opened_says_so_beside_the_rest() {
        // The column answers two questions: where playback has reached, and
        // what is playing. A source with no location can still report progress,
        // and a user deciding whether to allow it needs to see both.
        let streaming = Capabilities {
            location: false,
            ..everything()
        };

        assert_eq!(capabilities(streaming), "pos+dur+pause, title only");
    }

    #[test]
    fn a_source_that_declares_nothing_says_only_that_it_has_a_title() {
        // No leading comma and no empty column. An empty cell reads as a bug in
        // the renderer rather than as a fact about the source.
        assert_eq!(capabilities(nothing()), "title only");
    }

    #[test]
    fn a_source_that_cannot_report_a_position_does_not_claim_one() {
        let no_position = Capabilities {
            position: false,
            ..everything()
        };

        let shown = capabilities(no_position);

        assert!(!shown.contains("pos"), "claimed a position: {shown}");
        assert_eq!(shown, "dur+pause+loc");
    }

    #[test]
    fn a_source_that_names_what_it_opened_is_never_title_only() {
        let named = Capabilities {
            position: false,
            duration: false,
            paused: false,
            location: true,
        };

        assert_eq!(capabilities(named), "loc");
    }

    #[test]
    fn nothing_open_reads_as_idle_rather_than_stopped() {
        // The state answers what the source is doing. "Stopped" reads as
        // playback having been stopped; the fact is that nothing is open.
        assert_eq!(state(PlayState::Stopped), "idle");
        assert_eq!(state(PlayState::Playing), "playing");
        assert_eq!(state(PlayState::Paused), "paused");
    }

    #[test]
    fn a_listing_shows_the_identity_and_the_application_as_two_columns() {
        // Two windows of one player are two identities and one application.
        // Policy is set on the application, so the name to type has to be on
        // screen beside the identity that distinguishes the windows.
        let shown = listing(&[
            a_listing("mpv.instance1701", "mpv", Policy::Auto, everything()),
            a_listing("mpv.instance1702", "mpv", Policy::Auto, everything()),
        ]);

        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines.len(), 2, "one line per source: {shown}");
        assert!(lines[0].contains("mpv.instance1701"), "{shown}");
        assert!(lines[1].contains("mpv.instance1702"), "{shown}");
        for line in lines {
            assert!(line.contains("mpv "), "no application column: {line}");
        }
    }

    #[test]
    fn a_listing_shows_the_policy_in_force_for_each_source() {
        let shown = listing(&[
            a_listing("mpv", "mpv", Policy::Auto, everything()),
            a_listing("firefox.instance30062", "firefox", Policy::Deny, nothing()),
            a_listing("vlc", "vlc", Policy::Allow, everything()),
        ]);

        assert!(shown.contains("auto"), "{shown}");
        assert!(shown.contains("deny"), "{shown}");
        assert!(shown.contains("allow"), "{shown}");
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_printing_nothing() {
        // Nothing on screen cannot be told from a command that failed quietly.
        let shown = listing(&[]);

        assert!(!shown.trim().is_empty(), "an empty listing printed nothing");
        assert!(shown.contains("no source"), "got {shown:?}");
    }

    #[test]
    fn a_reported_moment_reads_as_minutes_and_seconds() {
        assert_eq!(moment(Known::Value(Duration::from_secs(0))), "00:00");
        assert_eq!(moment(Known::Value(Duration::from_secs(5))), "00:05");
        assert_eq!(moment(Known::Value(Duration::from_secs(1_383))), "23:03");
        assert_eq!(moment(Known::Value(Duration::from_secs(3_661))), "61:01");
    }

    #[test]
    fn a_moment_that_was_not_reported_reads_differently_from_one_that_cannot_be() {
        // The whole reason the domain carries three values rather than two. A
        // renderer that collapses them here throws the distinction away at the
        // last possible moment, where nobody downstream can recover it.
        let not_reported = moment(Known::NotReported);
        let unsupported = moment(Known::Unsupported);

        assert_ne!(not_reported, unsupported);
        assert_ne!(not_reported, "00:00", "an absence rendered as the start");
        assert_ne!(unsupported, "00:00", "an absence rendered as the start");
    }

    #[test]
    fn a_reading_shows_where_playback_is_and_what_is_playing() {
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        };

        let shown = event(&BusEvent::Snapshot(snapshot));

        assert!(shown.contains("mpv"), "{shown}");
        assert!(shown.contains("00:05"), "{shown}");
        assert!(shown.contains("23:00"), "{shown}");
        assert!(shown.contains("/anime/ep 03.mkv"), "{shown}");
    }

    #[test]
    fn a_filename_that_is_not_utf8_is_shown_rather_than_refused() {
        // A name out of a Japanese archive unpacked with the wrong encoding is
        // exactly what a user watching this stream is trying to diagnose.
        // Refusing it, or printing a row of replacement characters, hides the
        // one thing they came to see.
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::NotReported,
            observed_at: Timestamp::epoch(),
        };

        let shown = event(&BusEvent::Snapshot(snapshot));

        assert!(shown.contains(" - 03.mkv"), "the name was lost: {shown}");
        assert!(
            !shown.contains('\u{fffd}'),
            "the name was replaced rather than read: {shown}"
        );
    }

    #[test]
    fn a_membership_change_names_the_sources_rather_than_counting_them() {
        let shown = event(&BusEvent::SourcesChanged {
            sources: vec![PlayerId("mpv".to_owned()), PlayerId("Feishin".to_owned())],
        });

        assert!(shown.contains("mpv"), "{shown}");
        assert!(shown.contains("Feishin"), "{shown}");
    }

    #[test]
    fn a_membership_that_went_empty_says_so() {
        // An event whose list is empty must not render as an event with no
        // content: every player closing is a thing that happened.
        let shown = event(&BusEvent::SourcesChanged {
            sources: Vec::new(),
        });

        assert!(
            !shown.trim().is_empty(),
            "an empty membership printed nothing"
        );
        assert!(shown.contains("no source"), "got {shown:?}");
    }

    #[test]
    fn a_failure_names_the_source_and_the_reason() {
        let shown = event(&BusEvent::SourceFailed {
            player: PlayerId("vlc".to_owned()),
            reason: "did not answer within 500ms".to_owned(),
        });

        assert!(shown.contains("vlc"), "{shown}");
        assert!(shown.contains("did not answer within 500ms"), "{shown}");
    }

    #[test]
    fn a_failure_line_built_from_a_real_error_reads_as_one_sentence() {
        // The test above supplies a reason of its own, which is how this got
        // past a full acceptance pass and 222 tests. What `benshi watch`
        // actually printed on 2026-09-18, with three players suspended:
        //
        //   mpv.instance-X  failed: PlayerId("mpv.instance-X") did not answer within 500ms
        //
        // The identity belongs to the event and not to the reason:
        // `BusEvent::SourceFailed` carries both so that this function lays them
        // out, and a reason that repeats the subject prints it twice wherever a
        // person reads it. A test that writes its own reason cannot see that,
        // so both cases below take the reason from a real error.
        //
        // The whole line is pinned rather than picked at. Every weaker check
        // written for this passed for a line that was still wrong: counting the
        // identity passes for a bare fragment, and looking for the absence of
        // `PlayerId` passes for a reason that names the source in any other
        // form. One sentence, read end to end, is the thing being claimed.
        let player = PlayerId("mpv.instance-NvZEKsqR".to_owned());
        let cases = [
            (
                WatchError::Timeout {
                    player: player.clone(),
                    deadline: Duration::from_millis(500),
                },
                "mpv.instance-NvZEKsqR  failed: did not answer within 500ms",
            ),
            (
                WatchError::Unavailable(player.clone()),
                "mpv.instance-NvZEKsqR  failed: is no longer present",
            ),
        ];

        for (failure, expected) in cases {
            let shown = event(&BusEvent::SourceFailed {
                player: player.clone(),
                reason: failure.to_string(),
            });

            assert_eq!(shown, expected);
        }
    }

    /// An explanation of the file at this path, answered this way.
    ///
    /// The name is parsed the way a round parses it, so what is shown is what
    /// the daemon would have decided about.
    fn explained(path: &[u8], outcome: Outcome) -> Explanation {
        let path = RawPath::from_bytes(path.to_vec());
        let (spelled, _refused) = Recogniser::empty().decide(&path);

        Explanation {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(path),
            spelled,
            outcome,
        }
    }

    /// A match of this title by this stage, at episode three.
    fn found(title: &str, stage: DecidedBy) -> Outcome {
        Outcome::Recognised {
            title: title.to_owned(),
            episode: Episode::Only(3),
            stage,
        }
    }

    /// The line of an explanation that begins this way.
    fn line_of<'a>(shown: &'a str, beginning: &str) -> &'a str {
        shown
            .lines()
            .find(|line| line.starts_with(beginning))
            .unwrap_or_else(|| panic!("no line begins with {beginning:?}: {shown:?}"))
    }

    #[test]
    fn nothing_decided_says_so_rather_than_printing_nothing() {
        // Nothing on screen cannot be told from a command that failed quietly,
        // and a client asking before the first reading is the ordinary case.
        let shown = why(None);

        assert!(shown.contains("nothing has been decided"), "got {shown:?}");
    }

    #[test]
    fn an_explanation_names_the_player_and_the_file_first() {
        let shown = explanation(&explained(
            b"/anime/[Group] Show Title - 03.mkv",
            Outcome::Unrecognised { best: None },
        ));

        let first = shown.lines().next().expect("a first line");
        assert!(first.contains("mpv"), "{first}");
        assert!(
            first.contains("/anime/[Group] Show Title - 03.mkv"),
            "{first}"
        );
    }

    #[test]
    fn an_explanation_shows_what_the_name_spelled() {
        // In words, and the season among them: a user comparing the file that
        // matched with the one that did not is looking for exactly the thing
        // the parser cut out of the title.
        let shown = explanation(&explained(
            b"[Group] Show Title S02E03 [1080p].mkv",
            Outcome::Unrecognised { best: None },
        ));

        let spelled = line_of(&shown, "spelled:");
        assert!(spelled.contains("\"Show Title\""), "{spelled}");
        assert!(spelled.contains("season 2"), "{spelled}");
        assert!(spelled.contains("episode 3"), "{spelled}");
        assert!(spelled.contains("\"Group\""), "{spelled}");
    }

    #[test]
    fn a_name_that_spelled_no_title_says_so() {
        let shown = explanation(&explained(
            b"/anime/ep 03.mkv",
            Outcome::Unrecognised { best: None },
        ));

        let spelled = line_of(&shown, "spelled:");
        assert!(spelled.contains("no title"), "{spelled}");
        assert!(spelled.contains("episode 3"), "{spelled}");
    }

    #[test]
    fn a_name_that_was_not_utf8_is_shown_read_and_said_to_be_inferred() {
        // The confidence travels with the parse so that this line can say it:
        // a title recovered by inference is a weaker input than one that could
        // not have been anything else, and the user is the one who can tell
        // whether the guess was right.
        let shown = explanation(&explained(
            SHIFT_JIS_NAME,
            Outcome::Unrecognised { best: None },
        ));

        assert!(shown.contains("ソレッタ"), "the name was not read: {shown}");
        assert!(
            !shown.contains('\u{fffd}'),
            "the name was replaced rather than read: {shown}"
        );
        assert!(line_of(&shown, "spelled:").contains("inferred"), "{shown}");
    }

    #[test]
    fn a_match_by_an_assignment_shows_the_text_the_user_gave_and_the_season_it_was_bound_to() {
        // What a user whose first-season file did not follow their assignment
        // needs to see: the text they typed, and the season the assignment
        // was bound to, which the text itself does not spell.
        let shown = explanation(&explained(
            b"Kaguya-sama.Love.is.War.S03E03.1080p.WEB-DL.mkv",
            found(
                "Kaguya-sama: Love is War -Ultra Romantic-",
                DecidedBy::Altname(Assignment {
                    spelled: "Kaguya-sama Love is War".to_owned(),
                    season: Some(3),
                    part: None,
                    title: "Kaguya-sama: Love is War -Ultra Romantic-".to_owned(),
                }),
            ),
        ));

        assert!(
            line_of(&shown, "recognised:").contains("Kaguya-sama: Love is War -Ultra Romantic-"),
            "{shown}"
        );
        let stage = line_of(&shown, "stage:");
        assert!(stage.contains("\"Kaguya-sama Love is War\""), "{stage}");
        assert!(stage.contains("season 3"), "{stage}");
    }

    #[test]
    fn a_stage_is_named_rather_than_numbered() {
        // The criterion speaks of "stage one" and the type names its stages,
        // because a number computed from a position moves when a stage is
        // inserted. What is printed is the name.
        let cases = [
            (
                DecidedBy::Altname(Assignment {
                    spelled: "SnK".to_owned(),
                    season: None,
                    part: None,
                    title: "Show Title".to_owned(),
                }),
                "stage: altname",
            ),
            (DecidedBy::Key, "stage: key"),
            (DecidedBy::Scored(0.83), "stage: score"),
        ];

        for (stage, expected) in cases {
            let shown = explanation(&explained(
                b"[Group] Show Title - 03.mkv",
                found("Show Title", stage),
            ));

            assert!(
                line_of(&shown, "stage:").starts_with(expected),
                "expected {expected:?} in {shown:?}"
            );
        }
    }

    #[test]
    fn a_match_by_score_shows_the_score_it_won_with() {
        let shown = explanation(&explained(
            b"[Group] Show Title - 03.mkv",
            found("Show Title", DecidedBy::Scored(0.83)),
        ));

        assert!(line_of(&shown, "stage:").contains("0.83"), "{shown}");
    }

    #[test]
    fn an_ambiguity_names_every_candidate() {
        let shown = explanation(&explained(
            b"Fruits Basket - 01.mkv",
            Outcome::Ambiguous {
                candidates: vec![
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned(),
                ],
            },
        ));

        let ambiguous = line_of(&shown, "ambiguous:");
        assert!(ambiguous.contains("\"Fruits Basket\""), "{ambiguous}");
        assert!(
            ambiguous.contains("\"Fruits Basket (2019)\""),
            "{ambiguous}"
        );
    }

    #[test]
    fn a_refusal_says_how_close_the_best_came_or_that_nothing_was_scored() {
        // Absent and low are different answers and lead different places: a
        // score says the list was searched and the name is the problem, an
        // absence says the list is.
        let close = explanation(&explained(
            b"Some Other Show - 03.mkv",
            Outcome::Unrecognised { best: Some(0.4) },
        ));
        let nothing = explanation(&explained(
            b"Some Other Show - 03.mkv",
            Outcome::Unrecognised { best: None },
        ));

        assert!(line_of(&close, "unrecognised:").contains("0.40"), "{close}");
        let unscored = line_of(&nothing, "unrecognised:");
        assert!(unscored.contains("nothing"), "{unscored}");
        assert!(
            !unscored.contains("0.00"),
            "an absence rendered as a score of nought: {unscored}"
        );
    }

    #[test]
    fn an_episode_that_is_not_one_number_is_said_in_words() {
        // Several, a half, and none are three answers a consumer must not
        // read as a number, so none of them is printed as one.
        let cases = [
            (Episode::Several, "several episodes"),
            (Episode::NotWhole, "not a whole number"),
            (Episode::Absent, "no episode"),
        ];

        for (episode, words) in cases {
            let shown = explanation(&explained(
                b"[Group] Show Title - 03.mkv",
                Outcome::Recognised {
                    title: "Show Title".to_owned(),
                    episode,
                    stage: DecidedBy::Key,
                },
            ));

            let recognised = line_of(&shown, "recognised:");
            assert!(recognised.contains(words), "{recognised}");
            assert!(
                !recognised.contains("episode 0"),
                "an absence rendered as a number: {recognised}"
            );
        }
    }
}
