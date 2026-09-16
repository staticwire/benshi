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

use benshi_core::encoding::decode;
use benshi_core::policy::Policy;
use benshi_core::{Capabilities, Known, MediaRef, PlayState};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::protocol::SourceListing;

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
    use super::{capabilities, event, listing, moment, state};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::Policy;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_daemon::bus::BusEvent;
    use benshi_daemon::protocol::SourceListing;
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
}
