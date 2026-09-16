//! Building blocks shared by the watchers that exercise the contract.
//!
//! Sources and readings only. No watcher lives here, because the two test
//! binaries that use these need very different ones: one that satisfies every
//! clause and eleven that each break one.

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

/// A source that reports everything, as a local player does.
pub fn a_full_player() -> SourceInfo {
    SourceInfo {
        player: PlayerId("mpv.instance1701".to_owned()),
        app: AppName("mpv".to_owned()),
        capabilities: Capabilities {
            position: true,
            duration: true,
            paused: true,
            file_path: true,
        },
        state: PlayState::Playing,
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
            file_path: false,
        },
        state: PlayState::Playing,
    }
}

/// Build the reading a source of these capabilities would produce.
///
/// Every field follows the declaration: a source that cannot report a position
/// reports `Unsupported` rather than zero, and one that can report a path
/// reports a path.
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

    let media = if source.capabilities.file_path {
        MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec()))
    } else {
        MediaRef::Title("Episode 3 - Some Streaming Site".to_owned())
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
