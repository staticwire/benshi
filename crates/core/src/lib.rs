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

use std::path::PathBuf;
use std::time::{Duration, Instant};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Playback position within the current media.
    pub position: bool,
    /// Total duration of the current media.
    pub duration: bool,
    /// Whether playback is paused.
    pub paused: bool,
    /// A file path, not merely a window title.
    pub file_path: bool,
}

/// Stable identity of a media source, as provided by the platform.
///
/// The MPRIS bus suffix on Linux, the `AppUserModelId` under SMTC on Windows,
/// the bundle identifier on macOS. Never a process name matched by a regular
/// expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlayerId(pub String);

/// What a media source currently has open.
///
/// Provisional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaRef {
    /// A local file the source opened.
    LocalFile(PathBuf),
    /// A title string with no underlying file, as published by a browser.
    Title(String),
}

/// Coarse playback state of a media source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// When the reading was taken.
    pub observed_at: Instant,
}

/// The state a sink renders.
///
/// Provisional. Derived from playback, never from whether a sync to a list
/// backend succeeded: what a presence card shows depends on what is playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    /// The reading the session was last advanced by.
    pub snapshot: PlayerSnapshot,
}

#[cfg(test)]
mod tests {
    use super::Known;

    #[test]
    fn known_distinguishes_absence_from_incapability() {
        let not_reported: Known<u8> = Known::NotReported;
        let unsupported: Known<u8> = Known::Unsupported;
        assert_ne!(not_reported, unsupported);
    }
}
