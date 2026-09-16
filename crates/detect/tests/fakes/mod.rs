//! Building blocks shared by the watchers that exercise the contract.
//!
//! Sources and readings only. No watcher lives here, because the two test
//! binaries that use these need very different ones: one that satisfies every
//! clause and thirteen that each break one.

use std::time::Duration;

use benshi_core::clock::Timestamp;
use benshi_core::path::RawPath;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use benshi_detect::SourceInfo;

/// How far a fake clock moves between polls.
pub const TICK: Duration = Duration::from_secs(5);

/// The deadline the fakes promise, and which the suite measures against.
pub const DEADLINE: Duration = Duration::from_millis(500);

/// A Japanese filename in Shift-JIS, which is not valid UTF-8.
///
/// Used rather than an ASCII name so that the fakes exercise the same path a
/// real file out of a Japanese archive takes.
pub const SHIFT_JIS_NAME: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.mkv";

/// The address the streaming fake reports.
pub const STREAM_ADDRESS: &str = "https://example.invalid/episode-3.m3u8";

/// The identity of the source that opens a local file.
const LOCAL_PLAYER: &str = "mpv.instance1701";

/// The identity of the source that opens a remote address.
const STREAMING_PLAYER: &str = "mpv.instance1702";

/// A source that reports everything and opens a local file.
pub fn a_full_player() -> SourceInfo {
    SourceInfo {
        player: PlayerId(LOCAL_PLAYER.to_owned()),
        app: AppName("mpv".to_owned()),
        capabilities: Capabilities {
            position: true,
            duration: true,
            paused: true,
            location: true,
        },
        state: PlayState::Playing,
    }
}

/// A second window of that player, open on a stream rather than a file.
///
/// Declares the same capabilities as the first, because naming what is open is
/// a property of the player and not of what it happens to hold. The reading is
/// what differs, and the contract has to accept both.
pub fn a_streaming_player() -> SourceInfo {
    SourceInfo {
        player: PlayerId(STREAMING_PLAYER.to_owned()),
        ..a_full_player()
    }
}

/// A source that reports a title and nothing else, as a browser does.
pub fn a_title_only_player() -> SourceInfo {
    SourceInfo {
        player: PlayerId("firefox.instance30062".to_owned()),
        app: AppName("firefox".to_owned()),
        capabilities: Capabilities {
            position: false,
            duration: false,
            paused: true,
            location: false,
        },
        state: PlayState::Playing,
    }
}

/// Build the reading a source of these capabilities would produce.
///
/// Every field follows the declaration: a source that cannot report a position
/// reports `Unsupported` rather than zero, and one that declares a location
/// names what it opened. Which kind of name it gives is fixture data, because
/// the declaration deliberately does not say: that is the distinction between
/// a capability and a reading.
pub fn reading(source: &SourceInfo, round: u32, at: Timestamp) -> PlayerSnapshot {
    let position = if source.capabilities.position {
        Known::Value(TICK * round)
    } else {
        Known::Unsupported
    };

    let duration = if source.capabilities.duration {
        Known::Value(Duration::from_mins(23))
    } else {
        Known::Unsupported
    };

    let media = match source.player.0.as_str() {
        LOCAL_PLAYER => MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec())),
        STREAMING_PLAYER => MediaRef::Remote(STREAM_ADDRESS.to_owned()),
        _ => MediaRef::Title("Episode 3 - Some Streaming Site".to_owned()),
    };

    PlayerSnapshot {
        player: source.player.clone(),
        media,
        state: source.state,
        position,
        duration,
        observed_at: at,
    }
}
