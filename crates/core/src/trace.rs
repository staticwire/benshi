//! The file a recording is written to and replayed from.
//!
//! JSON Lines: a header object on the first line, then one [`PlayerSnapshot`]
//! per line in the order they were taken. Line-oriented for two reasons, and
//! both of them are properties this module has to keep.
//!
//! **A trace can be cut short.** `head -n 12` on a recording that caught
//! something at line 11 leaves a file that still opens, so reducing a case
//! costs a shell command rather than an afternoon of editing JSON. That holds
//! only while the header is sufficient on its own: nothing a reading carries
//! may be needed to interpret it, which rules out a count, a source list, or
//! anything else the cut would falsify.
//!
//! **A trace can be appended to.** A recorder writes the header once and then a
//! line per reading, never holding the file in memory and never rewriting it.
//! Every line therefore ends with a newline, including the last: without that,
//! the next append would join two readings into one line, and an interrupted
//! recording would be unreadable from the interruption rather than merely
//! short. [`header_line`] and [`snapshot_line`] are the only two things that
//! produce a line, and [`Trace::to_jsonl`] is built from them, so a recorder
//! that appends cannot drift from the reader that parses.
//!
//! Values are written in their own serde form and are never tidied. A duration
//! rounded to milliseconds or a timestamp rounded to seconds would still look
//! like a recording and would replay a timeline that never happened. For the
//! same reason the file is UTF-8 even when a path is not: [`RawPath`] escapes
//! the bytes it cannot spell rather than replacing them.
//!
//! [`RawPath`]: crate::path::RawPath

use serde::{Deserialize, Serialize};

use crate::PlayerSnapshot;

/// The version this build writes, and the only one it reads.
pub const VERSION: u32 = 1;

/// The first line of a trace.
///
/// Read on its own, before any reading is parsed, so that a file this build
/// does not understand is named rather than half-read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceHeader {
    /// Which version of this format the file is written in.
    pub version: u32,

    /// When the recording was taken, as the recorder spelled it.
    ///
    /// A note to whoever opens the file, and nothing else: no code parses it
    /// and replay never consults it. An ISO-8601 instant by convention, and
    /// nothing checks that, because a value nothing reads cannot be validated
    /// into being true. It earns its place because a fixture outlives the
    /// player it was taken from, and "which mpv was this?" is otherwise
    /// unanswerable.
    ///
    /// Text rather than a moment because this crate has no calendar and needs
    /// none. The timeline a replay follows is in the readings, whose
    /// timestamps are monotonic from the recording clock's arbitrary epoch and
    /// so have no relation to any wall clock.
    pub recorded_at: String,
}

impl TraceHeader {
    /// A header for a recording taken at `recorded_at`, in this version.
    #[must_use]
    pub fn new(recorded_at: String) -> Self {
        Self {
            version: VERSION,
            recorded_at,
        }
    }
}

/// A recording: a header, and the readings taken under it.
///
/// Not serialisable as one value, deliberately. A trace is a file of lines
/// rather than a JSON document, and a derived `Serialize` would put a second
/// representation - one object with an array inside it - beside the format,
/// free to drift from it and easy to reach for by mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    /// What the file says about itself.
    pub header: TraceHeader,

    /// The readings, in the order they were taken.
    pub snapshots: Vec<PlayerSnapshot>,
}

impl Trace {
    /// The whole recording, as the contents of a file.
    ///
    /// # Errors
    ///
    /// [`TraceError::NotWritten`] if the header or a reading cannot be
    /// serialised.
    pub fn to_jsonl(&self) -> Result<String, TraceError> {
        let mut text = header_line(&self.header)?;
        for snapshot in &self.snapshots {
            text.push_str(&snapshot_line(snapshot)?);
        }

        Ok(text)
    }

    /// A recording, read from the contents of a file.
    ///
    /// # Errors
    ///
    /// [`TraceError::Empty`] for text with no lines, [`TraceError::Header`]
    /// when the first line is not a header, [`TraceError::UnsupportedVersion`]
    /// when it is a header this build does not read, and
    /// [`TraceError::Snapshot`] naming the line when a later one is not a
    /// reading.
    pub fn from_jsonl(text: &str) -> Result<Self, TraceError> {
        let mut lines = text.lines();

        let first = lines.next().ok_or(TraceError::Empty)?;
        let header: TraceHeader =
            serde_json::from_str(first).map_err(|source| TraceError::Header { source })?;
        if header.version != VERSION {
            return Err(TraceError::UnsupportedVersion {
                found: header.version,
            });
        }

        let snapshots = lines
            .enumerate()
            .map(|(index, reading)| {
                serde_json::from_str(reading).map_err(|source| TraceError::Snapshot {
                    // The header is line 1, so the first reading is line 2.
                    // A line number that does not match what an editor shows
                    // is worse than no line number at all.
                    line: index + 2,
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { header, snapshots })
    }
}

/// The header as the first line of a trace, newline included.
///
/// # Errors
///
/// [`TraceError::NotWritten`] if the header cannot be serialised.
pub fn header_line(header: &TraceHeader) -> Result<String, TraceError> {
    line(header)
}

/// One reading as a line of a trace, newline included.
///
/// What a recorder appends as each reading arrives.
///
/// # Errors
///
/// [`TraceError::NotWritten`] if the reading cannot be serialised.
pub fn snapshot_line(snapshot: &PlayerSnapshot) -> Result<String, TraceError> {
    line(snapshot)
}

/// One value as a line, newline included.
///
/// The single place a line is made, so that the newline every line ends with is
/// one decision rather than one per caller.
fn line<T: Serialize>(value: &T) -> Result<String, TraceError> {
    let mut text =
        serde_json::to_string(value).map_err(|source| TraceError::NotWritten { source })?;
    text.push('\n');

    Ok(text)
}

/// Why a trace could not be read or written.
///
/// A fault that could be on any line says which one, because a trace is a
/// fixture people edit by hand and a recording is a file a killed process
/// leaves half-written. "expected value at line 1 column 1" from underneath
/// names a position within one line, which is not the line it was in. A fault
/// that can only be on the header needs no number.
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    /// The text held no lines, so there was no header to read.
    #[error("a trace begins with a header line, and this text has no lines")]
    Empty,

    /// The first line is not a header.
    #[error("line 1 is not a trace header: {source}")]
    Header {
        /// What the parser made of it.
        #[source]
        source: serde_json::Error,
    },

    /// The header is one this build does not read.
    #[error("this trace is version {found}, and this build reads version {VERSION}")]
    UnsupportedVersion {
        /// The version the header declares.
        found: u32,
    },

    /// A line after the first is not a reading.
    #[error("line {line} is not a reading: {source}")]
    Snapshot {
        /// Which line, counting the header as line 1.
        line: usize,
        /// What the parser made of it.
        #[source]
        source: serde_json::Error,
    },

    /// A line could not be produced.
    #[error("a trace line could not be written: {source}")]
    NotWritten {
        /// What the serialiser said.
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::{Trace, TraceError, TraceHeader, VERSION, header_line, snapshot_line};
    use crate::clock::{Clock, TestClock, Timestamp};
    use crate::path::RawPath;
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;

    /// A filename in Shift-JIS, which is not valid UTF-8.
    const SHIFT_JIS_NAME: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.mkv";

    /// When a recording was taken, as a recorder would write it.
    const RECORDED_AT: &str = "2026-09-17T09:12:33Z";

    /// A moment `after` seconds past a clock's epoch.
    ///
    /// [`Timestamp`] has no constructor from a value on purpose: it comes from
    /// a clock or it comes from a file. A test that needs a particular moment
    /// asks a clock for it.
    fn a_moment(after: u64) -> Timestamp {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(after));
        clock.now()
    }

    /// The `index`th reading of a recording, distinguishable from the others.
    fn a_snapshot(index: u64) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv.instance1701".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/[Group] Show - 03.mkv".to_vec(),
            )),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(index)),
            duration: Known::Value(Duration::from_secs(1420)),
            observed_at: a_moment(index),
        }
    }

    /// A recording of `count` readings.
    fn a_trace_of(count: u64) -> Trace {
        Trace {
            header: TraceHeader::new(RECORDED_AT.to_owned()),
            snapshots: (0..count).map(a_snapshot).collect(),
        }
    }

    #[test]
    fn a_trace_round_trips_through_its_file_format_exactly() {
        let trace = a_trace_of(2);

        let text = trace.to_jsonl().expect("a trace is written");
        let back = Trace::from_jsonl(&text).expect("a trace is read");

        assert_eq!(trace, back);
    }

    #[test]
    fn a_trace_file_is_one_line_per_reading_after_the_header() {
        for count in 0..=3 {
            let text = a_trace_of(count).to_jsonl().expect("a trace is written");

            assert_eq!(
                text.lines().count() as u64,
                count + 1,
                "a header and {count} readings came out as {text:?}"
            );
        }
    }

    #[test]
    fn a_trace_cut_short_with_head_is_a_shorter_trace() {
        // The reason the format is line-oriented. `head -n 3` on a recording
        // that caught a bug at line 40 has to leave a trace that still opens,
        // or reducing a case means editing JSON by hand.
        let whole = a_trace_of(4).to_jsonl().expect("a trace is written");
        let lines: Vec<&str> = whole.lines().collect();

        for readings in 0..=4 {
            // What `head -n` leaves, measured: the first n lines, each still
            // terminated, and nothing of the line after them. Line 0 is the
            // header, so this keeps that many readings after it.
            let cut = lines[..=readings].join("\n") + "\n";

            let shorter = Trace::from_jsonl(&cut)
                .unwrap_or_else(|failure| panic!("{readings} readings did not parse: {failure}"));

            assert_eq!(shorter, a_trace_of(readings as u64));
        }
    }

    #[test]
    fn a_header_alone_is_a_trace_with_no_readings() {
        // The shortest cut `head` can leave is the header alone, and it has to
        // parse. Nothing that a reading carries may be needed to interpret the
        // header - no count, no source list - because a cut file has fewer
        // readings than the header was written beside.
        let header =
            header_line(&TraceHeader::new(RECORDED_AT.to_owned())).expect("a header is written");

        let trace = Trace::from_jsonl(&header).expect("a header alone is a trace");

        assert_eq!(trace.header.recorded_at, RECORDED_AT);
        assert_eq!(trace.header.version, VERSION);
        assert!(trace.snapshots.is_empty(), "{:?}", trace.snapshots);
    }

    #[test]
    fn every_line_ends_with_a_newline_so_a_trace_can_be_appended_to() {
        // A recorder appends a line per reading. If the last line carried no
        // newline the next append would join two readings into one line, and
        // the file would be unreadable from the point the recorder was
        // interrupted rather than from the end.
        assert!(
            header_line(&TraceHeader::new(RECORDED_AT.to_owned()))
                .expect("a header is written")
                .ends_with('\n')
        );
        assert!(
            snapshot_line(&a_snapshot(0))
                .expect("a reading is written")
                .ends_with('\n')
        );
        for count in 0..=3 {
            let text = a_trace_of(count).to_jsonl().expect("a trace is written");
            assert!(
                text.ends_with('\n'),
                "{count} readings came out as {text:?}"
            );
        }
    }

    #[test]
    fn the_lines_a_recorder_writes_are_the_lines_the_parser_reads() {
        // A recorder never holds a whole trace: it writes the header once and
        // then a line per reading as it arrives. These two functions are the
        // only things that produce a line, so a recorder cannot drift from the
        // parser by assembling one itself.
        let trace = a_trace_of(3);

        let mut appended = header_line(&trace.header).expect("a header is written");
        for snapshot in &trace.snapshots {
            appended.push_str(&snapshot_line(snapshot).expect("a reading is written"));
        }

        assert_eq!(appended, trace.to_jsonl().expect("a trace is written"));
        assert_eq!(
            Trace::from_jsonl(&appended).expect("a trace is read"),
            trace
        );
    }

    #[test]
    fn a_reading_survives_the_format_whole() {
        // The values most likely to be tidied away. A path that is not UTF-8
        // must arrive byte for byte; a value that was never reported must not
        // come back as one this source cannot carry; and a position must keep
        // its nanoseconds, because a rounded one proves a timeline that never
        // happened.
        let readings = vec![
            PlayerSnapshot {
                player: PlayerId("mpv.instance1701".to_owned()),
                media: MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec())),
                state: PlayState::Paused,
                position: Known::Value(Duration::new(93, 456_789_123)),
                duration: Known::Unsupported,
                observed_at: a_moment(1),
            },
            PlayerSnapshot {
                player: PlayerId("Feishin".to_owned()),
                media: MediaRef::Remote("https://example.invalid/stream.m3u8".to_owned()),
                state: PlayState::Playing,
                position: Known::NotReported,
                duration: Known::NotReported,
                observed_at: a_moment(2),
            },
            PlayerSnapshot {
                player: PlayerId("chromium.instance16481".to_owned()),
                media: MediaRef::Title("Episode 3".to_owned()),
                state: PlayState::Stopped,
                position: Known::Unsupported,
                duration: Known::Unsupported,
                observed_at: a_moment(3),
            },
        ];
        let trace = Trace {
            header: TraceHeader::new(RECORDED_AT.to_owned()),
            snapshots: readings.clone(),
        };

        let text = trace.to_jsonl().expect("a trace is written");
        let back = Trace::from_jsonl(&text).expect("a trace is read");

        assert_eq!(back.snapshots, readings);
        match &back.snapshots[0].media {
            MediaRef::LocalFile(path) => assert_eq!(path.as_bytes(), SHIFT_JIS_NAME),
            other => panic!("expected a file, got {other:?}"),
        }
        assert_eq!(
            back.snapshots[0].position,
            Known::Value(Duration::new(93, 456_789_123))
        );
    }

    #[test]
    fn a_truncated_line_reports_the_line_it_failed_on() {
        // What a recorder killed mid-write leaves behind, and a trace is also
        // a fixture people edit by hand. "expected value at line 1 column 1"
        // with no line of ours makes both painful to repair.
        let whole = a_trace_of(3).to_jsonl().expect("a trace is written");
        let mut kept: Vec<&str> = whole.lines().take(3).collect();
        kept.push(r#"{"player":"mpv.instance1701","media":{"Local"#);
        let cut = kept.join("\n");

        let failure = Trace::from_jsonl(&cut).expect_err("half a reading is not a reading");

        assert!(
            matches!(failure, TraceError::Snapshot { line: 4, .. }),
            "got {failure:?}"
        );
        assert!(failure.to_string().contains("line 4"), "got {failure}");
    }

    #[test]
    fn two_traces_joined_end_to_end_name_the_line_that_is_wrong() {
        // `cat a.jsonl b.jsonl` is the obvious way to make a longer recording
        // and it does not work, because the second header lands where a
        // reading belongs. Saying which line is what turns that into a
        // one-line fix.
        let first = a_trace_of(2).to_jsonl().expect("a trace is written");
        let second = a_trace_of(2).to_jsonl().expect("a trace is written");

        let failure =
            Trace::from_jsonl(&format!("{first}{second}")).expect_err("a header is not a reading");

        assert!(
            matches!(failure, TraceError::Snapshot { line: 4, .. }),
            "got {failure:?}"
        );
    }

    #[test]
    fn a_blank_line_is_a_fault_rather_than_something_to_skip() {
        // Nothing here can tell whether a reading was lost or a newline was
        // added, so passing over it would turn a file that has been damaged
        // into a shorter one that still opens. A shorter trace that opens is
        // supposed to mean one thing: somebody cut it on purpose.
        let whole = a_trace_of(2).to_jsonl().expect("a trace is written");

        let failure = Trace::from_jsonl(&whole.replacen('\n', "\n\n", 1))
            .expect_err("a blank line is not a reading");

        assert!(
            matches!(failure, TraceError::Snapshot { line: 2, .. }),
            "got {failure:?}"
        );
    }

    #[test]
    fn a_trace_of_a_version_this_build_does_not_read_is_refused() {
        // The version exists so that a format we do not understand is named
        // rather than half-read. Parsing it as far as it happens to fit would
        // produce a trace that is wrong in a way nothing reports.
        let future = VERSION + 1;
        let text = format!(r#"{{"version":{future},"recorded_at":"{RECORDED_AT}"}}"#);

        let failure = Trace::from_jsonl(&text).expect_err("a later format is not read");

        assert!(
            matches!(failure, TraceError::UnsupportedVersion { found } if found == future),
            "got {failure:?}"
        );
    }

    #[test]
    fn text_with_no_lines_at_all_is_not_a_trace() {
        let failure = Trace::from_jsonl("").expect_err("nothing is not a trace");

        assert!(matches!(failure, TraceError::Empty), "got {failure:?}");
    }

    #[test]
    fn a_first_line_that_is_not_a_header_is_refused() {
        let text = snapshot_line(&a_snapshot(0)).expect("a reading is written");

        let failure = Trace::from_jsonl(&text).expect_err("a reading is not a header");

        assert!(
            matches!(failure, TraceError::Header { .. }),
            "got {failure:?}"
        );
    }
}
