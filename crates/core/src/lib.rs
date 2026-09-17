//! Domain types and pure logic for benshi.
//!
//! This crate performs no I/O and contains no platform-specific code, so it can
//! be tested in full without mocking a platform. Detection adapters translate
//! platform readings into the types defined here; all decisions are taken in
//! this crate, never in an adapter.
//!
//! Types marked *provisional* have a name and a purpose but not yet a settled
//! shape; they are expected to change once the logic that consumes them is
//! written.

pub mod clock;
pub mod encoding;
pub mod path;
pub mod policy;
pub mod trace;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::path::RawPath;

/// Provisional error placeholder.
///
/// Each crate gains a concrete error enum once `thiserror` is added as a
/// dependency; until then a boxed error keeps signatures honest without
/// inventing variants that have no call sites yet.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A value a media source may or may not be able to report.
///
/// Three-valued rather than [`Option`], because "the player did not publish a
/// length" and "this detection method cannot carry a length" are different
/// facts that lead to different behaviour: the first may be retried or shown as
/// unknown, the second means the capability is absent and the user should be
/// told so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Known<T> {
    /// The source reported this value.
    Value(T),
    /// The source did not report it, though this detection method can carry it.
    NotReported,
    /// This detection method cannot carry the value at all.
    Unsupported,
}

/// What a detection method is able to report.
///
/// Declared by each adapter rather than guessed by the consumer, so that the
/// interface can say "no progress bar: this detection method cannot report
/// position" instead of showing nothing.
// Four bools trips `clippy::struct_excessive_bools`, whose usual advice is to
// introduce an enum. That advice is wrong here: these are independent
// capabilities that occur in any combination, not states of one variable.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Playback position within the current media.
    pub position: bool,
    /// Total duration of the current media.
    pub duration: bool,
    /// Whether playback is paused.
    pub paused: bool,
    /// A location for what is open: a path or an address, not only a title.
    ///
    /// Answers whether this source names what it opened, which is what
    /// recognition needs. Whether that name turns out to be a local file is a
    /// fact about the media rather than about the source, so it belongs to
    /// [`MediaRef`] and not here.
    pub location: bool,
}

/// Stable identity of a media source, as provided by the platform.
///
/// The MPRIS bus suffix on Linux, the `AppUserModelId` under SMTC on Windows,
/// the bundle identifier on macOS. Never a process name matched by a regular
/// expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlayerId(pub String);

/// The application a source belongs to, as the platform names it.
///
/// Distinct from [`PlayerId`], and the distinction is not academic. A live
/// session bus carries both `org.mpris.MediaPlayer2.Feishin` and
/// `org.mpris.MediaPlayer2.chromium.instance30062`: two windows of one player
/// are two identities but one application. The identity exists to tell them
/// apart; policy and the source listing are keyed on the application.
///
/// The adapter declares both. This crate never derives one from the other,
/// because the rule that relates them - an `.instance` suffix here, something
/// else under SMTC - is platform knowledge and has no place in `core`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AppName(pub String);

/// What a media source currently has open.
///
/// Three cases and not two. A source that names what it opened and a source
/// that can only describe it are different things: mpv playing a stream knows
/// the address, a browser knows only what the page calls itself. Collapsing the
/// first into the second throws away an identifier that recognition can work
/// from, and leaves an adapter no way to report what it actually has.
///
/// Provisional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaRef {
    /// A local file the source opened, as the platform spelled its path.
    LocalFile(RawPath),
    /// An address the source opened, naming no file on this machine.
    Remote(String),
    /// A title with nothing underneath it, as published by a browser.
    Title(String),
}

/// Coarse playback state of a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayState {
    /// Media is advancing.
    Playing,
    /// Media is open but not advancing.
    Paused,
    /// Nothing is open.
    Stopped,
}

/// One reading taken from a media source.
///
/// Adapters emit snapshots and this crate computes differences between them. An
/// event model would lose state across a reconnect; a snapshot does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    /// Which source produced this reading.
    pub player: PlayerId,
    /// What the source has open.
    pub media: MediaRef,
    /// Whether the media is advancing.
    pub state: PlayState,
    /// How far into the media playback has reached.
    pub position: Known<Duration>,
    /// How long the media is, as reported. Advisory: players lie about it.
    pub duration: Known<Duration>,
    /// When the reading was taken, on the clock the watcher was given.
    pub observed_at: Timestamp,
}

/// The state a sink renders.
///
/// Provisional. Derived from playback, never from whether a sync to a list
/// backend succeeded: what a presence card shows depends on what is playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// The reading the session was last advanced by.
    pub snapshot: PlayerSnapshot,
}

#[cfg(test)]
mod tests {
    use super::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use crate::clock::Timestamp;
    use crate::path::RawPath;
    use std::time::Duration;

    #[test]
    fn known_distinguishes_absence_from_incapability() {
        let not_reported: Known<u8> = Known::NotReported;
        let unsupported: Known<u8> = Known::Unsupported;
        assert_ne!(not_reported, unsupported);
    }

    #[test]
    fn a_snapshot_round_trips_through_json_exactly() {
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show - 03.mkv".to_vec(),
            )),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_micros(93_456_789)),
            duration: Known::NotReported,
            observed_at: Timestamp::epoch(),
        };

        let text = serde_json::to_string(&snapshot).expect("serialise");
        let back: PlayerSnapshot = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(snapshot, back);
    }

    #[test]
    fn a_snapshot_with_a_non_utf8_path_round_trips() {
        // A filename from a Japanese archive unpacked with the wrong encoding
        // is not valid UTF-8, and this is exactly the snapshot a trace has to
        // carry. Refusing it, or replacing the bytes it cannot read, would make
        // the file unrecognisable and the trace a lie.
        let broken = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e.mkv".to_vec();
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(broken.clone())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_micros(93_456_789)),
            duration: Known::NotReported,
            observed_at: Timestamp::epoch(),
        };

        let text = serde_json::to_string(&snapshot).expect("serialise");
        let back: PlayerSnapshot = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(snapshot, back);
        match back.media {
            MediaRef::LocalFile(path) => assert_eq!(path.as_bytes(), broken.as_slice()),
            other => panic!("expected a file, got {other:?}"),
        }
    }

    #[test]
    fn a_remote_reference_stays_distinct_from_a_title() {
        // A player streaming over HTTP knows the address it opened; a browser
        // knows only what the page calls itself. Carrying both as a title would
        // lose that difference, and an adapter would have to report an address
        // it holds as though it were a description.
        let address = "https://example.invalid/stream.m3u8".to_owned();
        let remote = MediaRef::Remote(address.clone());
        let title = MediaRef::Title(address);

        assert_ne!(remote, title);
        assert_ne!(
            serde_json::to_string(&remote).expect("serialise"),
            serde_json::to_string(&title).expect("serialise")
        );
    }

    #[test]
    fn unsupported_and_not_reported_stay_distinct_through_json() {
        // A "simplification" to Option would collapse these two into one, and
        // that collapse is the specific bug Known exists to prevent.
        let unsupported: Known<Duration> = Known::Unsupported;
        let not_reported: Known<Duration> = Known::NotReported;

        let a = serde_json::to_string(&unsupported).expect("serialise");
        let b = serde_json::to_string(&not_reported).expect("serialise");

        assert_ne!(a, b);
    }
}
