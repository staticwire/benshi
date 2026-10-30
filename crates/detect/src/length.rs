//! The length a video file records in its own header.
//!
//! A player that reports no length, or one in the wrong unit, leaves a reading
//! without the number that tells a trailer from an episode and sets how much of
//! a file is watched. `smplayer` 25.6 reports none for any file, and Dragon
//! Player 26.08 reports a thousandth of it. The file itself holds it. Matroska
//! and WebM record it in the `Duration` of the `Info` element of their
//! `Segment`, in units of its `TimecodeScale`. MP4 records it in the `mvhd` box
//! inside `moov`, in units of that box's own time scale. AVI records it in the
//! `strh` of its video stream, in units of the stream's scale and rate, and in
//! `avih` as frames of a number of microseconds each.
//!
//! Matroska and MP4 are found by reading the headers of elements and boxes and
//! skipping what they hold, so a file costs a few small reads wherever its
//! length lies, a `moov` written after the media included. `Info` is looked for
//! only ahead of the first `Cluster`. The `hdrl` that opens an AVI file is read
//! whole.
//!
//! A file is read on a thread of its own, under the deadline of the reading
//! that asked, and its length is kept while every round of readings names it.
//! A read that outlives the deadline is left to finish and keeps its answer for
//! a later reading, and no second read of that file starts while it runs. A
//! file on a mount that has stopped answering holds one thread and no more.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use benshi_core::path::RawPath;
use tokio::time::{Instant, timeout_at};

/// The ID of the EBML header a Matroska or WebM file opens with, and so the
/// first four bytes of such a file.
const EBML_HEADER: u32 = 0x1A45_DFA3;

/// The ID of the `Segment`, which holds everything after the EBML header.
const SEGMENT: u32 = 0x1853_8067;

/// The ID of `Info`, which records the length.
const INFO: u32 = 0x1549_A966;

/// The ID of a `Cluster`, the elements the media is stored in.
const CLUSTER: u32 = 0x1F43_B675;

/// The ID of the `TimecodeScale` of `Info`: the nanoseconds of a tick.
const TIMECODE_SCALE: u32 = 0x002A_D7B1;

/// The ID of the `Duration` of `Info`: the length in ticks, as a float.
const DURATION: u32 = 0x4489;

/// The `TimecodeScale` of an `Info` that records none: a millisecond.
const DEFAULT_TIMECODE_SCALE: u64 = 1_000_000;

/// How many elements of a `Segment` are looked at for its `Info`.
///
/// A walk that meets a `Cluster` stops there, so this bounds the walk only over
/// elements of other kinds, as in a broken file. Each costs one read of the
/// header of an element.
const ELEMENTS_LOOKED_AT: usize = 16;

/// The largest `Info` read, in bytes.
///
/// `Info` holds a few numbers and strings, so a larger one is taken for a
/// broken file and left unread.
const INFO_LIMIT: u64 = 64 * 1024;

/// How many boxes of one level of an MP4 file are looked at for the box
/// sought. Each costs one read of the header of a box.
const BOXES_LOOKED_AT: usize = 16;

/// The largest `hdrl` of an AVI file read, in bytes.
///
/// `hdrl` holds the main header and a list for each stream. FFmpeg pads each
/// list to some 4 KiB, so this holds over a dozen streams, and a larger `hdrl`
/// is taken for a broken file and left unread.
const HEADER_LIMIT: u64 = 64 * 1024;

/// What is known of the length of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// A read has started and has not finished.
    Running,
    /// The read has finished, with the length the header records or none.
    Done(Option<Duration>),
}

/// The lengths of the files sources play, each read once.
#[derive(Debug, Default)]
pub(crate) struct Lengths {
    looks: Arc<Mutex<HashMap<RawPath, Look>>>,
}

impl Lengths {
    /// The length the header of `file` records, read once and by `until`.
    ///
    /// None while the read runs, where it runs past `until`, and where the
    /// file cannot be opened, records no length or is no Matroska, WebM, MP4
    /// or AVI file.
    pub(crate) async fn of(&self, file: &RawPath, until: Instant) -> Option<Duration> {
        self.of_with(file, until, read_length).await
    }

    /// [`Lengths::of`], with the read of one file given.
    async fn of_with<F>(&self, file: &RawPath, until: Instant, read: F) -> Option<Duration>
    where
        F: FnOnce(&Path) -> Option<Duration> + Send + 'static,
    {
        match self.looks().entry(file.clone()) {
            Entry::Occupied(known) => {
                return match known.get() {
                    Look::Done(length) => *length,
                    Look::Running => None,
                };
            }
            Entry::Vacant(unknown) => {
                unknown.insert(Look::Running);
            }
        }

        let looks = Arc::clone(&self.looks);
        let file = file.clone();
        let task = tokio::task::spawn_blocking(move || {
            // A read that panics keeps no length, so that its file is not
            // taken for one whose read still runs, and the panic goes on.
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                read(Path::new(OsStr::from_bytes(file.as_bytes())))
            }));
            let length = outcome.as_ref().ok().copied().flatten();
            lock(&looks).insert(file, Look::Done(length));
            match outcome {
                Ok(length) => length,
                Err(failed) => panic::resume_unwind(failed),
            }
        });

        match timeout_at(until, task).await {
            Ok(Ok(length)) => length,
            Ok(Err(failed)) if failed.is_panic() => panic::resume_unwind(failed.into_panic()),
            // The runtime is shutting down, or the deadline has passed. A read
            // left behind keeps its answer when it ends.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Forget every file a round of readings did not name, except one whose
    /// read still runs, so that a file opened again is read again and a file
    /// that does not answer is never read twice.
    pub(crate) fn keep_only<'a>(&self, open: impl IntoIterator<Item = &'a RawPath>) {
        let open: HashSet<&RawPath> = open.into_iter().collect();
        self.looks()
            .retain(|file, look| *look == Look::Running || open.contains(file));
    }

    fn looks(&self) -> MutexGuard<'_, HashMap<RawPath, Look>> {
        lock(&self.looks)
    }
}

/// Lock the looks. Each change to them is a single insert or retain, so a
/// thread that panicked holding the lock leaves them as they were or as it
/// meant them to be, and they are taken as they stand.
fn lock(looks: &Mutex<HashMap<RawPath, Look>>) -> MutexGuard<'_, HashMap<RawPath, Look>> {
    looks.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The length the header of the file at `path` records.
fn read_length(path: &Path) -> Option<Duration> {
    length_in(&mut File::open(path).ok()?)
}

/// The length a Matroska, WebM, MP4 or AVI header records, read from `source`.
///
/// Matroska and WebM are told by the EBML header they open with, MP4 by the
/// `ftyp` box it opens with, AVI by `RIFF` and `AVI ` at its start. A file of
/// any other kind records no length here.
fn length_in(source: &mut (impl Read + Seek)) -> Option<Duration> {
    let opening = read_up_to(source, 0, 12)?;

    if opening.starts_with(&EBML_HEADER.to_be_bytes()) {
        matroska_length(source)
    } else if opening.get(4..8) == Some(b"ftyp") {
        mp4_length(source)
    } else if opening.starts_with(b"RIFF") && opening.get(8..12) == Some(b"AVI ") {
        avi_length(source)
    } else {
        None
    }
}

/// The length an AVI file records in its `hdrl`: the length of the first video
/// stream that records one, in units of that stream's scale and rate, or else
/// the frames of the file at the microseconds of one frame.
///
/// The stream comes first. In a file split into several `RIFF` chunks, FFmpeg
/// writes the frames of the whole file to the `strh` of the video stream and
/// those of the first `RIFF` only to `avih`. `avih` also holds the microseconds
/// of one frame as a whole number.
///
/// A `RIFF` whose size has every bit set was written where its writer could not
/// go back, and its header holds no count at all: FFmpeg writes 2^30 frames to
/// the `strh` of such a file and none to `avih`. It records no length.
fn avi_length(source: &mut (impl Read + Seek)) -> Option<Duration> {
    let opening = read_up_to(source, 0, 24)?;
    if le_u32(&opening, 4)? == u32::MAX
        || opening.get(12..16) != Some(b"LIST")
        || opening.get(20..24) != Some(b"hdrl")
    {
        return None;
    }
    let size = u64::from(le_u32(&opening, 16)?)
        .checked_sub(4)
        .filter(|&size| size <= HEADER_LIMIT)?;
    let header = read_up_to(source, 24, size)?;

    let mut by_frames = None;
    let mut by_stream = None;
    for (id, data) in riff_chunks(&header) {
        match &id {
            b"avih" => by_frames = frames_length(data),
            b"LIST" if by_stream.is_none() => {
                by_stream = data.strip_prefix(b"strl").and_then(video_stream_length);
            }
            _ => {}
        }
    }

    by_stream.or(by_frames)
}

/// The chunks of the data of a RIFF list, each by its code and its data, as
/// far as the data holds whole ones. A chunk of odd size is padded to an even
/// one.
fn riff_chunks(mut data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    std::iter::from_fn(move || {
        let id = data.get(..4)?.try_into().ok()?;
        let size = usize::try_from(le_u32(data, 4)?).ok()?;
        let end = size.checked_add(8)?;
        let chunk = data.get(8..end)?;
        data = data.get(end.checked_add(size % 2)?.min(data.len())..)?;
        Some((id, chunk))
    })
}

/// The length `avih` records: the frames of the file at the microseconds of
/// one frame.
fn frames_length(avih: &[u8]) -> Option<Duration> {
    let micros = u64::from(le_u32(avih, 0)?) * u64::from(le_u32(avih, 16)?);
    (micros > 0).then(|| Duration::from_micros(micros))
}

/// The length a `strl` records where its `strh` is that of a video stream:
/// the stream's length in units of its scale over its rate.
fn video_stream_length(list: &[u8]) -> Option<Duration> {
    let (_, header) = riff_chunks(list).find(|(id, _)| id == b"strh")?;
    if header.get(..4) != Some(b"vids") {
        return None;
    }
    let scale = u128::from(le_u32(header, 20)?);
    let rate = u128::from(le_u32(header, 24)?);
    let units = u128::from(le_u32(header, 32)?) * scale;
    if rate == 0 {
        return None;
    }

    let nanos = units % rate * 1_000_000_000 / rate;
    let length = Duration::new(
        u64::try_from(units / rate).ok()?,
        u32::try_from(nanos).ok()?,
    );
    (length > Duration::ZERO).then_some(length)
}

/// The little-endian `u32` at `at`.
fn le_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Up to `limit` bytes of `source` from `offset`, fewer where it ends sooner.
fn read_up_to(source: &mut (impl Read + Seek), offset: u64, limit: u64) -> Option<Vec<u8>> {
    source.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    source.by_ref().take(limit).read_to_end(&mut bytes).ok()?;

    Some(bytes)
}

/// An EBML element, by its ID and where its data lies.
struct Element {
    /// The ID, with the marker of its length, as the Matroska specification
    /// writes IDs.
    id: u32,
    /// Where the data begins.
    data: u64,
    /// How long the data is, or none where the size is unknown.
    size: Option<u64>,
}

impl Element {
    /// Where the element ends, which an element of unknown size does not say.
    fn end(&self) -> Option<u64> {
        self.data.checked_add(self.size?)
    }
}

/// The length a Matroska or WebM file records in the `Info` of its `Segment`.
fn matroska_length(source: &mut (impl Read + Seek)) -> Option<Duration> {
    let header = element_at(source, 0)?;
    let segment = element_at(source, header.end()?)?;
    if header.id != EBML_HEADER || segment.id != SEGMENT {
        return None;
    }

    let mut at = segment.data;
    for _ in 0..ELEMENTS_LOOKED_AT {
        if segment.end().is_some_and(|end| at >= end) {
            return None;
        }
        let element = element_at(source, at)?;
        match element.id {
            INFO => return info_length(source, &element),
            CLUSTER => return None,
            _ => at = element.end()?,
        }
    }

    None
}

/// The length an `Info` element records: its `Duration` in units of its
/// `TimecodeScale`.
fn info_length(source: &mut (impl Read + Seek), info: &Element) -> Option<Duration> {
    let size = info.size.filter(|&size| size <= INFO_LIMIT)?;
    let body = read_up_to(source, info.data, size)?;

    let mut scale = DEFAULT_TIMECODE_SCALE;
    let mut ticks = None;
    let mut rest = body.as_slice();
    while !rest.is_empty() {
        let (id, id_length) = element_id(rest)?;
        let (size, size_length) = element_size(rest.get(id_length..)?)?;
        let start = id_length + size_length;
        let end = start.checked_add(usize::try_from(size?).ok()?)?;
        let value = rest.get(start..end)?;
        match id {
            TIMECODE_SCALE => scale = unsigned(value)?,
            DURATION => ticks = Some(float(value)?),
            _ => {}
        }
        rest = rest.get(end..)?;
    }

    let seconds = ticks? * f64::from(u32::try_from(scale).ok()?) / 1e9;
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|length| !length.is_zero())
}

/// The element whose header begins at `offset`.
fn element_at(source: &mut (impl Read + Seek), offset: u64) -> Option<Element> {
    // An ID takes at most four bytes and a size at most eight.
    let header = read_up_to(source, offset, 12)?;
    let (id, id_length) = element_id(&header)?;
    let (size, size_length) = element_size(header.get(id_length..)?)?;
    let data = offset.checked_add(u64::try_from(id_length + size_length).ok()?)?;

    Some(Element { id, data, size })
}

/// How many bytes a variable-length integer takes, from its first byte: one
/// more than the zeros ahead of its first set bit. A first byte of zero marks
/// a length beyond eight bytes.
fn vint_length(first: u8) -> Option<usize> {
    if first == 0 {
        return None;
    }

    usize::try_from(first.leading_zeros() + 1).ok()
}

/// An element's ID, with the marker of its length, and how many bytes it takes,
/// at most four.
fn element_id(bytes: &[u8]) -> Option<(u32, usize)> {
    let length = vint_length(*bytes.first()?).filter(|&length| length <= 4)?;
    let id = bytes
        .get(..length)?
        .iter()
        .fold(0, |id, &byte| (id << 8) | u32::from(byte));

    Some((id, length))
}

/// An element's size, with the marker of its length cleared, and how many
/// bytes it takes. A size whose bits are all set is unknown.
fn element_size(bytes: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *bytes.first()?;
    let length = vint_length(first)?;
    let size = bytes
        .get(1..length)?
        .iter()
        .fold(u64::from(first) & (0xFF >> length), |size, &byte| {
            (size << 8) | u64::from(byte)
        });
    let unknown = (1_u64 << (7 * length)) - 1;

    Some(((size != unknown).then_some(size), length))
}

/// An unsigned integer of up to eight bytes, big-endian.
fn unsigned(bytes: &[u8]) -> Option<u64> {
    (bytes.len() <= 8).then(|| {
        bytes
            .iter()
            .fold(0, |value, &byte| (value << 8) | u64::from(byte))
    })
}

/// A float of four or eight bytes, big-endian, and zero where it takes none.
fn float(bytes: &[u8]) -> Option<f64> {
    match bytes.len() {
        0 => Some(0.0),
        4 => Some(f64::from(f32::from_be_bytes(bytes.try_into().ok()?))),
        8 => Some(f64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

/// A box of an MP4 file, by its type and where its data lies.
struct Mp4Box {
    kind: [u8; 4],
    /// Where the data begins.
    data: u64,
    /// Where the box ends.
    end: u64,
}

/// The length an MP4 file records in the `mvhd` box of its `moov`.
fn mp4_length(source: &mut (impl Read + Seek)) -> Option<Duration> {
    let file_end = source.seek(SeekFrom::End(0)).ok()?;
    let moov = box_in(source, 0, file_end, *b"moov")?;
    let mvhd = box_in(source, moov.data, moov.end, *b"mvhd")?;

    mvhd_length(source, &mvhd)
}

/// The first box of the kind sought among the boxes from `start` to `end`.
fn box_in(source: &mut (impl Read + Seek), start: u64, end: u64, kind: [u8; 4]) -> Option<Mp4Box> {
    let mut at = start;
    for _ in 0..BOXES_LOOKED_AT {
        let found = box_at(source, at, end)?;
        if found.kind == kind {
            return Some(found);
        }
        at = found.end;
    }

    None
}

/// The box whose header begins at `offset`, inside a parent that ends at `end`.
///
/// A size of zero runs to the end of the parent, and a size of one is followed
/// by the size in 64 bits. A box that would run past its parent is broken.
fn box_at(source: &mut (impl Read + Seek), offset: u64, end: u64) -> Option<Mp4Box> {
    let header = read_up_to(source, offset, 16)?;
    let kind = header.get(4..8)?.try_into().ok()?;
    let (size, header_length) = match be_u32(&header, 0)? {
        0 => (end.checked_sub(offset)?, 8),
        1 => (be_u64(&header, 8)?, 16),
        size => (u64::from(size), 8),
    };
    let box_end = offset.checked_add(size)?;

    (size >= header_length && box_end <= end).then_some(Mp4Box {
        kind,
        data: offset + header_length,
        end: box_end,
    })
}

/// The length an `mvhd` box records: its duration in units of its time scale.
///
/// Version 0 writes both in 32 bits and version 1 writes the duration in 64,
/// and a duration with every bit set is unknown. A length that rounds to zero
/// is none.
fn mvhd_length(source: &mut (impl Read + Seek), mvhd: &Mp4Box) -> Option<Duration> {
    let body = read_up_to(source, mvhd.data, 32.min(mvhd.end - mvhd.data))?;
    let (scale, duration) = match body.first()? {
        0 => (
            be_u32(&body, 12)?,
            u64::from(be_u32(&body, 16).filter(|&duration| duration != u32::MAX)?),
        ),
        1 => (
            be_u32(&body, 20)?,
            be_u64(&body, 24).filter(|&duration| duration != u64::MAX)?,
        ),
        _ => return None,
    };
    let scale = u64::from(scale);
    if scale == 0 {
        return None;
    }

    let nanos = u128::from(duration % scale) * 1_000_000_000 / u128::from(scale);
    let length = Duration::new(duration / scale, u32::try_from(nanos).ok()?);
    (!length.is_zero()).then_some(length)
}

/// The big-endian `u32` at `at`.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// The big-endian `u64` at `at`.
fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    use benshi_core::path::RawPath;
    use tempfile::TempDir;
    use tokio::time::Instant;

    use super::{Lengths, length_in};

    /// Twenty-four minutes, the length of an episode.
    const EPISODE: Duration = Duration::from_mins(24);

    /// Two minutes, the length of a trailer.
    const TRAILER: Duration = Duration::from_mins(2);

    // The IDs of the Matroska elements these tests write.
    const EBML_HEADER: [u8; 4] = [0x1A, 0x45, 0xDF, 0xA3];
    const DOC_TYPE: [u8; 2] = [0x42, 0x82];
    const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
    const SEEK_HEAD: [u8; 4] = [0x11, 0x4D, 0x9B, 0x74];
    const VOID: [u8; 1] = [0xEC];
    const INFO: [u8; 4] = [0x15, 0x49, 0xA9, 0x66];
    const TIMECODE_SCALE: [u8; 3] = [0x2A, 0xD7, 0xB1];
    const DURATION: [u8; 2] = [0x44, 0x89];
    const MUXING_APP: [u8; 2] = [0x4D, 0x80];
    const TRACKS: [u8; 4] = [0x16, 0x54, 0xAE, 0x6B];
    const CLUSTER: [u8; 4] = [0x1F, 0x43, 0xB6, 0x75];

    /// The size of an element's data, in the shortest form that holds it.
    fn ebml_size(size: usize) -> Vec<u8> {
        let size = u64::try_from(size).expect("a size in 64 bits");
        if size < 0x7F {
            vec![0x80 | u8::try_from(size).expect("one byte")]
        } else if size < 0x3FFF {
            let [.., high, low] = size.to_be_bytes();
            vec![0x40 | high, low]
        } else {
            let mut bytes = vec![0x01];
            bytes.extend_from_slice(&size.to_be_bytes()[1..]);
            bytes
        }
    }

    /// An EBML element: its ID, the size of its data, and the data.
    fn element(id: &[u8], data: &[u8]) -> Vec<u8> {
        [id, &ebml_size(data.len()), data].concat()
    }

    /// The EBML header of a file of the given document type.
    fn ebml_header(doc_type: &str) -> Vec<u8> {
        element(&EBML_HEADER, &element(&DOC_TYPE, doc_type.as_bytes()))
    }

    /// An `Info` element holding a duration in eight bytes and the scale given.
    fn info(scale: u32, ticks: f64) -> Vec<u8> {
        let scale = &scale.to_be_bytes()[1..];
        element(
            &INFO,
            &[
                element(&TIMECODE_SCALE, scale),
                element(&MUXING_APP, b"libebml v1.4.5 + libmatroska v1.7.1"),
                element(&DURATION, &ticks.to_be_bytes()),
            ]
            .concat(),
        )
    }

    /// The elements a muxer writes ahead of `Info`.
    fn ahead_of_info() -> Vec<u8> {
        [
            element(&SEEK_HEAD, &[0x4D, 0xBB, 0x80]),
            element(&VOID, &[0; 32]),
        ]
        .concat()
    }

    /// The media that follows the headers.
    fn media() -> Vec<u8> {
        [element(&TRACKS, &[0; 16]), element(&CLUSTER, &[0; 256])].concat()
    }

    /// A Matroska file whose `Segment` holds the elements given.
    fn matroska(segment: &[u8]) -> Vec<u8> {
        [ebml_header("matroska"), element(&SEGMENT, segment)].concat()
    }

    /// A Matroska file of an episode, written as a muxer writes it.
    fn an_episode() -> Vec<u8> {
        matroska(&[ahead_of_info(), info(1_000_000, 1_440_000.0), media()].concat())
    }

    /// An MP4 box: its size, its type and its data.
    fn mp4_box(kind: [u8; 4], data: &[u8]) -> Vec<u8> {
        let size = u32::try_from(8 + data.len()).expect("a box under 4 GiB");
        [&size.to_be_bytes()[..], &kind, data].concat()
    }

    /// The `ftyp` box an MP4 file opens with.
    fn ftyp() -> Vec<u8> {
        mp4_box(*b"ftyp", b"isom\x00\x00\x02\x00isomiso2avc1mp41")
    }

    /// An `mvhd` box of version 0: 32-bit times, the time scale and the
    /// duration given, and the rest of the box as a muxer writes it.
    fn mvhd(scale: u32, duration: u32) -> Vec<u8> {
        let mut data = vec![0, 0, 0, 0];
        data.extend_from_slice(&[0; 8]);
        data.extend_from_slice(&scale.to_be_bytes());
        data.extend_from_slice(&duration.to_be_bytes());
        data.extend_from_slice(&[0; 80]);
        mp4_box(*b"mvhd", &data)
    }

    /// An `mvhd` box of version 1: 64-bit times and duration.
    fn mvhd_v1(scale: u32, duration: u64) -> Vec<u8> {
        let mut data = vec![1, 0, 0, 0];
        data.extend_from_slice(&[0; 16]);
        data.extend_from_slice(&scale.to_be_bytes());
        data.extend_from_slice(&duration.to_be_bytes());
        data.extend_from_slice(&[0; 80]);
        mp4_box(*b"mvhd", &data)
    }

    /// A `moov` box holding an `mvhd` and a track.
    fn moov(mvhd: &[u8]) -> Vec<u8> {
        mp4_box(*b"moov", &[mvhd, &mp4_box(*b"trak", &[0; 64])].concat())
    }

    /// The media data of an MP4 file.
    fn mdat() -> Vec<u8> {
        mp4_box(*b"mdat", &[0; 512])
    }

    fn length_of(bytes: &[u8]) -> Option<Duration> {
        length_in(&mut Cursor::new(bytes))
    }

    #[test]
    fn a_matroska_file_records_its_length_in_info() {
        assert_eq!(length_of(&an_episode()), Some(EPISODE));
    }

    #[test]
    fn a_webm_file_is_read_as_matroska() {
        let webm = [
            ebml_header("webm"),
            element(&SEGMENT, &[info(1_000_000, 1_440_000.0), media()].concat()),
        ]
        .concat();

        assert_eq!(length_of(&webm), Some(EPISODE));
    }

    #[test]
    fn a_duration_in_four_bytes_is_read() {
        let segment = element(&INFO, &element(&DURATION, &1_440_000.0_f32.to_be_bytes()));

        assert_eq!(length_of(&matroska(&segment)), Some(EPISODE));
    }

    #[test]
    fn the_timecode_scale_sets_the_unit_of_the_duration() {
        // A microsecond a tick, so the same length takes a thousand times the
        // ticks.
        let segment = info(1_000, 1_440_000_000.0);

        assert_eq!(length_of(&matroska(&segment)), Some(EPISODE));
    }

    #[test]
    fn an_info_with_no_timecode_scale_counts_in_milliseconds() {
        let segment = element(&INFO, &element(&DURATION, &120_000.0_f64.to_be_bytes()));

        assert_eq!(length_of(&matroska(&segment)), Some(TRAILER));
    }

    #[test]
    fn a_fraction_of_a_tick_is_kept() {
        let segment = info(1_000_000, 1_440_000.5);

        assert_eq!(
            length_of(&matroska(&segment)),
            Some(EPISODE + Duration::from_micros(500))
        );
    }

    #[test]
    fn an_info_with_no_duration_records_no_length() {
        let segment = element(&INFO, &element(&TIMECODE_SCALE, &[0x0F, 0x42, 0x40]));

        assert_eq!(length_of(&matroska(&[segment, media()].concat())), None);
    }

    #[test]
    fn a_segment_of_unknown_size_is_read_to_its_info() {
        // A size whose bits are all set is unknown, as a file written live
        // leaves it, in a size of eight bytes or of one. Read as a number, the
        // one byte would end the `Segment` 127 bytes in, ahead of this `Info`.
        for unknown in [
            vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            vec![0xFF],
        ] {
            let file = [
                ebml_header("matroska"),
                SEGMENT.to_vec(),
                unknown.clone(),
                element(&VOID, &[0; 200]),
                info(1_000_000, 1_440_000.0),
                media(),
            ]
            .concat();

            assert_eq!(length_of(&file), Some(EPISODE), "{unknown:02x?}");
        }
    }

    #[test]
    fn info_outside_the_segment_is_not_read() {
        let file = [
            ebml_header("matroska"),
            element(&SEGMENT, &element(&VOID, &[0; 4])),
            info(1_000_000, 1_440_000.0),
        ]
        .concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn info_after_the_first_cluster_is_not_looked_for() {
        let segment = [element(&CLUSTER, &[0; 256]), info(1_000_000, 1_440_000.0)].concat();

        assert_eq!(length_of(&matroska(&segment)), None);
    }

    #[test]
    fn info_among_the_first_sixteen_elements_is_read() {
        let segment = [
            element(&VOID, &[0; 4]).repeat(15),
            info(1_000_000, 1_440_000.0),
        ]
        .concat();

        assert_eq!(length_of(&matroska(&segment)), Some(EPISODE));
    }

    #[test]
    fn info_behind_sixteen_other_elements_is_not_looked_for() {
        let segment = [
            element(&VOID, &[0; 4]).repeat(16),
            info(1_000_000, 1_440_000.0),
        ]
        .concat();

        assert_eq!(length_of(&matroska(&segment)), None);
    }

    #[test]
    fn an_info_of_many_kilobytes_is_read() {
        let title = vec![b'a'; 32 * 1024];
        let segment = element(
            &INFO,
            &[
                element(&MUXING_APP, &title),
                element(&DURATION, &1_440_000.0_f64.to_be_bytes()),
            ]
            .concat(),
        );

        assert_eq!(length_of(&matroska(&segment)), Some(EPISODE));
    }

    #[test]
    fn an_info_larger_than_sixty_four_kibibytes_is_not_read() {
        let title = vec![b'a'; 64 * 1024];
        let segment = element(
            &INFO,
            &[
                element(&MUXING_APP, &title),
                element(&DURATION, &1_440_000.0_f64.to_be_bytes()),
            ]
            .concat(),
        );

        assert_eq!(length_of(&matroska(&segment)), None);
    }

    #[test]
    fn a_duration_that_is_zero_negative_or_not_finite_is_no_length() {
        for ticks in [0.0, -1_440_000.0, f64::NAN, f64::INFINITY] {
            let segment = info(1_000_000, ticks);

            assert_eq!(length_of(&matroska(&segment)), None, "{ticks}");
        }
    }

    #[test]
    fn a_timecode_scale_of_zero_is_no_length() {
        let segment = element(
            &INFO,
            &[
                element(&TIMECODE_SCALE, &[0]),
                element(&DURATION, &1_440_000.0_f64.to_be_bytes()),
            ]
            .concat(),
        );

        assert_eq!(length_of(&matroska(&segment)), None);
    }

    #[test]
    fn a_size_that_takes_more_than_eight_bytes_is_no_element() {
        // A first byte of zero marks a size longer than eight bytes.
        let file = [ebml_header("matroska"), SEGMENT.to_vec(), vec![0x00, 0xFF]].concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn an_id_longer_than_four_bytes_is_no_element() {
        // Five bytes whose last four spell the ID of `Info`.
        let five = [0x08, 0x15, 0x49, 0xA9, 0x66];
        let body = element(&DURATION, &1_440_000.0_f64.to_be_bytes());
        let segment = [&five[..], &ebml_size(body.len()), &body].concat();

        assert_eq!(length_of(&matroska(&segment)), None);
    }

    #[test]
    fn a_matroska_file_cut_short_never_reads_past_its_end() {
        let file = an_episode();
        let info_ends = file.len() - media().len();

        for cut in 0..file.len() {
            let found = length_of(&file[..cut]);

            if cut < info_ends {
                assert_eq!(found, None, "cut at {cut}");
            } else {
                assert_eq!(found, Some(EPISODE), "cut at {cut}");
            }
        }
    }

    #[test]
    fn an_mp4_file_records_its_length_in_mvhd() {
        let file = [ftyp(), moov(&mvhd(1_000, 1_440_000)), mdat()].concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn a_moov_written_after_the_media_is_found() {
        let file = [
            ftyp(),
            mp4_box(*b"free", &[0; 8]),
            mdat(),
            moov(&mvhd(1_000, 1_440_000)),
        ]
        .concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn an_mvhd_of_version_one_is_read() {
        let file = [ftyp(), moov(&mvhd_v1(90_000, 129_600_000)), mdat()].concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn the_time_scale_sets_the_unit_of_the_duration() {
        // 600 a second, as QuickTime writes, with a fraction of a second left.
        let file = [ftyp(), moov(&mvhd(600, 864_003)), mdat()].concat();

        assert_eq!(length_of(&file), Some(EPISODE + Duration::from_millis(5)));
    }

    #[test]
    fn mvhd_behind_other_boxes_of_moov_is_found() {
        let inside = [mp4_box(*b"udta", &[0; 24]), mvhd(1_000, 120_000)].concat();
        let file = [ftyp(), mp4_box(*b"moov", &inside), mdat()].concat();

        assert_eq!(length_of(&file), Some(TRAILER));
    }

    #[test]
    fn a_box_with_a_64_bit_size_is_skipped_whole() {
        let mut large = vec![0, 0, 0, 1];
        large.extend_from_slice(b"mdat");
        large.extend_from_slice(&(16_u64 + 512).to_be_bytes());
        large.extend_from_slice(&[0; 512]);
        let file = [ftyp(), large, moov(&mvhd(1_000, 1_440_000))].concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn a_box_of_size_zero_runs_to_the_end_of_the_file() {
        let inside = [mvhd(1_000, 1_440_000), mp4_box(*b"trak", &[0; 64])].concat();
        let open_ended = [&[0, 0, 0, 0][..], b"moov", &inside].concat();
        let file = [ftyp(), mdat(), open_ended].concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn an_mp4_duration_that_is_unknown_or_zero_is_no_length() {
        let unknown = [ftyp(), moov(&mvhd(1_000, u32::MAX)), mdat()].concat();
        let unknown_v1 = [ftyp(), moov(&mvhd_v1(1_000, u64::MAX)), mdat()].concat();
        let zero = [ftyp(), moov(&mvhd(1_000, 0)), mdat()].concat();

        assert_eq!(length_of(&unknown), None);
        assert_eq!(length_of(&unknown_v1), None);
        assert_eq!(length_of(&zero), None);
    }

    #[test]
    fn a_time_scale_of_zero_is_no_length() {
        let file = [ftyp(), moov(&mvhd(0, 1_440_000)), mdat()].concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn an_mp4_length_below_a_nanosecond_is_no_length() {
        // Half a nanosecond, which rounds to zero.
        let file = [ftyp(), moov(&mvhd(2_000_000_000, 1)), mdat()].concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn an_mp4_file_with_no_moov_records_no_length() {
        assert_eq!(length_of(&[ftyp(), mdat()].concat()), None);
    }

    #[test]
    fn a_moov_among_the_first_sixteen_boxes_is_found() {
        let free = mp4_box(*b"free", &[0; 8]).repeat(14);
        let file = [ftyp(), free, moov(&mvhd(1_000, 1_440_000))].concat();

        assert_eq!(length_of(&file), Some(EPISODE));
    }

    #[test]
    fn a_moov_behind_sixteen_other_boxes_is_not_looked_for() {
        let free = mp4_box(*b"free", &[0; 8]).repeat(15);
        let file = [ftyp(), free, moov(&mvhd(1_000, 1_440_000))].concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn a_box_that_runs_past_its_parent_ends_the_walk() {
        let mut overrun = mp4_box(*b"mdat", &[0; 512]);
        overrun[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        let file = [ftyp(), overrun, moov(&mvhd(1_000, 1_440_000))].concat();

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn an_mp4_file_cut_short_never_reads_past_its_end() {
        let file = [ftyp(), moov(&mvhd(1_000, 1_440_000)), mdat()].concat();
        let mvhd_ends = ftyp().len() + 8 + mvhd(1_000, 1_440_000).len();

        for cut in 0..file.len() {
            let found = length_of(&file[..cut]);

            // A `moov` cut short runs past the file, and its parent is the
            // file.
            if cut < mvhd_ends + 72 {
                assert_eq!(found, None, "cut at {cut}");
            } else {
                assert_eq!(found, Some(EPISODE), "cut at {cut}");
            }
        }
    }

    #[test]
    fn a_file_of_another_kind_records_no_length() {
        let wave = riff_chunk(
            *b"RIFF",
            &[&b"WAVE"[..], &riff_chunk(*b"fmt ", &[0; 16])].concat(),
        );

        assert_eq!(length_of(&wave), None);
        assert_eq!(length_of(b"#EXTM3U\n#EXTINF:1440,Episode\n"), None);
        assert_eq!(length_of(&[]), None);
    }

    /// A RIFF chunk: its code, the size of its data in little-endian, and the
    /// data, padded to an even size.
    fn riff_chunk(id: [u8; 4], data: &[u8]) -> Vec<u8> {
        let size = u32::try_from(data.len()).expect("a chunk under 4 GiB");
        let mut chunk = [&id[..], &size.to_le_bytes(), data].concat();
        if data.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    /// A RIFF list: a `LIST` chunk whose data opens with the type of the list.
    fn riff_list(kind: [u8; 4], chunks: &[u8]) -> Vec<u8> {
        riff_chunk(*b"LIST", &[&kind[..], chunks].concat())
    }

    /// The `avih` of an AVI file, with the microseconds of one frame and the
    /// frames of the file.
    fn avih(micros_per_frame: u32, frames: u32) -> Vec<u8> {
        let mut data = vec![0; 56];
        data[0..4].copy_from_slice(&micros_per_frame.to_le_bytes());
        data[16..20].copy_from_slice(&frames.to_le_bytes());
        riff_chunk(*b"avih", &data)
    }

    /// A `strl` of a stream of the kind given, `vids` or `auds`: a `strh` with
    /// the scale, the rate and the length, and a `strf`.
    fn strl(kind: [u8; 4], scale: u32, rate: u32, length: u32) -> Vec<u8> {
        let mut strh = vec![0; 56];
        strh[0..4].copy_from_slice(&kind);
        strh[20..24].copy_from_slice(&scale.to_le_bytes());
        strh[24..28].copy_from_slice(&rate.to_le_bytes());
        strh[32..36].copy_from_slice(&length.to_le_bytes());
        riff_list(
            *b"strl",
            &[riff_chunk(*b"strh", &strh), riff_chunk(*b"strf", &[0; 40])].concat(),
        )
    }

    /// An AVI file whose `hdrl` holds the chunks given, followed by its media.
    fn avi(header: &[u8]) -> Vec<u8> {
        riff_chunk(
            *b"RIFF",
            &[
                &b"AVI "[..],
                &riff_list(*b"hdrl", header),
                &riff_list(*b"movi", &[0; 64]),
            ]
            .concat(),
        )
    }

    /// The `hdrl` of an episode at 25 frames a second, with a sound stream
    /// beside the video.
    fn an_avi_episode_header() -> Vec<u8> {
        [
            avih(40_000, 36_000),
            strl(*b"vids", 1, 25, 36_000),
            strl(*b"auds", 1, 48_000, 69_000_000),
        ]
        .concat()
    }

    #[test]
    fn an_avi_file_records_its_length_in_hdrl() {
        assert_eq!(length_of(&avi(&an_avi_episode_header())), Some(EPISODE));
    }

    #[test]
    fn the_video_stream_is_read_wherever_it_stands_among_the_streams() {
        // With no `avih`, the length can come from the video stream alone.
        let header = [
            strl(*b"auds", 1, 48_000, 69_000_000),
            strl(*b"vids", 1, 25, 36_000),
        ]
        .concat();

        assert_eq!(length_of(&avi(&header)), Some(EPISODE));
    }

    #[test]
    fn the_video_stream_outranks_the_frames_of_the_file() {
        // 29.97 frames a second: three frames last 0.1001 s, which the
        // microseconds of one frame, rounded, cannot say.
        let header = [avih(33_367, 3), strl(*b"vids", 1001, 30_000, 3)].concat();

        assert_eq!(
            length_of(&avi(&header)),
            Some(Duration::from_micros(100_100))
        );
    }

    #[test]
    fn an_avi_file_with_no_video_stream_is_read_by_its_frames() {
        let header = [avih(40_000, 36_000), strl(*b"auds", 1, 48_000, 69_000_000)].concat();

        assert_eq!(length_of(&avi(&header)), Some(EPISODE));
    }

    #[test]
    fn a_video_stream_with_no_rate_leaves_the_frames_to_decide() {
        let header = [avih(40_000, 36_000), strl(*b"vids", 1, 0, 36_000)].concat();

        assert_eq!(length_of(&avi(&header)), Some(EPISODE));
    }

    #[test]
    fn an_avi_file_of_no_frames_records_no_length() {
        let header = [avih(40_000, 0), strl(*b"vids", 1, 25, 0)].concat();

        assert_eq!(length_of(&avi(&header)), None);
    }

    #[test]
    fn a_chunk_of_odd_size_is_padded_to_an_even_one() {
        let header = [riff_chunk(*b"JUNK", &[0; 3]), an_avi_episode_header()].concat();

        assert_eq!(length_of(&avi(&header)), Some(EPISODE));
    }

    #[test]
    fn a_header_list_that_does_not_come_first_is_not_looked_for() {
        let file = riff_chunk(
            *b"RIFF",
            &[
                &b"AVI "[..],
                &riff_chunk(*b"JUNK", &[0; 16]),
                &riff_list(*b"hdrl", &an_avi_episode_header()),
            ]
            .concat(),
        );

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn a_riff_file_of_another_form_is_not_read_as_avi() {
        let file = riff_chunk(
            *b"RIFF",
            &[&b"WAVE"[..], &riff_list(*b"hdrl", &an_avi_episode_header())].concat(),
        );

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn an_avi_file_written_without_going_back_records_no_length() {
        // As FFmpeg writes one into a pipe: a `RIFF` of unknown size, no frames
        // in `avih`, and 2^30 frames in `strh` for the count it never filled in.
        let header = [avih(40_000, 0), strl(*b"vids", 1, 25, 1 << 30)].concat();
        let mut file = avi(&header);
        file[4..8].copy_from_slice(&u32::MAX.to_le_bytes());

        assert_eq!(length_of(&file), None);
    }

    #[test]
    fn a_header_list_of_many_kilobytes_is_read() {
        let header = [
            riff_chunk(*b"JUNK", &vec![0; 32 * 1024]),
            an_avi_episode_header(),
        ]
        .concat();

        assert_eq!(length_of(&avi(&header)), Some(EPISODE));
    }

    #[test]
    fn a_header_list_larger_than_sixty_four_kibibytes_is_not_read() {
        let header = [
            riff_chunk(*b"JUNK", &vec![0; 64 * 1024]),
            an_avi_episode_header(),
        ]
        .concat();

        assert_eq!(length_of(&avi(&header)), None);
    }

    #[test]
    fn an_avi_file_cut_short_never_reads_past_its_end() {
        let file = avi(&an_avi_episode_header());
        // The file's own header, the header of `hdrl` and its type, and then
        // `avih`, which gives the length by itself.
        let avih_ends = 12 + 12 + avih(40_000, 36_000).len();

        for cut in 0..file.len() {
            let found = length_of(&file[..cut]);

            if cut < avih_ends {
                assert_eq!(found, None, "cut at {cut}");
            } else {
                assert_eq!(found, Some(EPISODE), "cut at {cut}");
            }
        }
    }

    /// A file on disk with the bytes given, named with no extension of a
    /// video file: the tests of this crate share one process, and one of
    /// them counts the video files that process holds open.
    fn a_file_with(bytes: &[u8]) -> (TempDir, RawPath) {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let path = directory.path().join("played");
        fs::write(&path, bytes).expect("the file written");
        let raw = RawPath::from_bytes(path.as_os_str().as_bytes().to_vec());
        (directory, raw)
    }

    fn rewrite(file: &RawPath, bytes: &[u8]) {
        fs::write(
            Path::new(std::ffi::OsStr::from_bytes(file.as_bytes())),
            bytes,
        )
        .expect("the file written again");
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// The trailer's file, an MP4 one.
    fn a_trailer() -> Vec<u8> {
        [ftyp(), moov(&mvhd(1_000, 120_000)), mdat()].concat()
    }

    #[tokio::test]
    async fn the_length_of_a_file_is_read_from_its_header() {
        let (_directory, file) = a_file_with(&an_episode());

        assert_eq!(Lengths::default().of(&file, soon()).await, Some(EPISODE));
    }

    #[tokio::test]
    async fn a_file_is_read_once_while_a_source_plays_it() {
        let lengths = Lengths::default();
        let (_directory, file) = a_file_with(&an_episode());
        assert_eq!(lengths.of(&file, soon()).await, Some(EPISODE));

        rewrite(&file, &a_trailer());
        lengths.keep_only([&file]);

        assert_eq!(lengths.of(&file, soon()).await, Some(EPISODE));
    }

    #[tokio::test]
    async fn a_file_no_reading_names_any_more_is_read_again() {
        let lengths = Lengths::default();
        let (_directory, file) = a_file_with(&an_episode());
        assert_eq!(lengths.of(&file, soon()).await, Some(EPISODE));

        rewrite(&file, &a_trailer());
        lengths.keep_only([]);

        assert_eq!(lengths.of(&file, soon()).await, Some(TRAILER));
    }

    #[tokio::test]
    async fn a_file_that_cannot_be_opened_has_no_length() {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let gone = directory.path().join("gone");
        let file = RawPath::from_bytes(gone.as_os_str().as_bytes().to_vec());

        assert_eq!(Lengths::default().of(&file, soon()).await, None);
    }

    /// A read of a file whose read is already running, which must not start.
    fn no_second_read(_: &Path) -> Option<Duration> {
        unreachable!("a second read of one file")
    }

    /// Ask `lengths` for the length of `file` until it is known, for up to
    /// five seconds: the length a read already running keeps.
    async fn kept(lengths: &Lengths, file: &RawPath) -> Option<Duration> {
        let wait = async {
            loop {
                if let Some(length) = lengths.of_with(file, soon(), no_second_read).await {
                    return length;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .ok()
    }

    /// Start a read of `file` that waits for the test to let it go, as a read
    /// on a mount that has stopped answering, and give up on it after 20 ms.
    async fn a_stalled_read(
        lengths: &Lengths,
        file: &RawPath,
    ) -> (mpsc::Sender<()>, Option<Duration>) {
        let (release, held) = mpsc::channel::<()>();
        let asked = lengths.of_with(
            file,
            Instant::now() + Duration::from_millis(20),
            move |_| {
                held.recv().expect("the test lets the read go");
                Some(EPISODE)
            },
        );
        let in_time = tokio::time::timeout(Duration::from_secs(5), asked)
            .await
            .expect("the read is given up on at its deadline");
        (release, in_time)
    }

    #[tokio::test]
    #[should_panic(expected = "a bug in the read")]
    async fn a_read_that_panics_in_time_panics_the_reading() {
        let file = RawPath::from_bytes(b"/media/video/broken".to_vec());

        Lengths::default()
            .of_with(&file, soon(), |_| panic!("a bug in the read"))
            .await;
    }

    #[tokio::test]
    async fn a_read_that_panics_past_the_deadline_leaves_its_file_to_be_read_again() {
        let lengths = Lengths::default();
        let (_directory, file) = a_file_with(&an_episode());
        let (release, held) = mpsc::channel::<()>();
        let asked = lengths.of_with(
            &file,
            Instant::now() + Duration::from_millis(20),
            move |_| {
                held.recv().expect("the test lets the read go");
                panic!("a bug in the read")
            },
        );
        let in_time = tokio::time::timeout(Duration::from_secs(5), asked)
            .await
            .expect("the read is given up on at its deadline");
        assert_eq!(in_time, None);
        release.send(()).expect("the read still waits");

        // Once the read has panicked, its file is no read still running: a
        // round that names it nowhere forgets it, and it is read again.
        let again = async {
            loop {
                lengths.keep_only([]);
                if let Some(length) = lengths.of(&file, soon()).await {
                    return length;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), again)
                .await
                .ok(),
            Some(EPISODE)
        );
    }

    #[tokio::test]
    async fn a_read_past_the_deadline_gives_no_length_and_keeps_its_answer() {
        let lengths = Lengths::default();
        let file = RawPath::from_bytes(b"/media/video/stalled".to_vec());

        let (release, in_time) = a_stalled_read(&lengths, &file).await;
        assert_eq!(in_time, None);
        assert_eq!(lengths.of_with(&file, soon(), no_second_read).await, None);
        release.send(()).expect("the read still waits");

        assert_eq!(kept(&lengths, &file).await, Some(EPISODE));
    }

    #[tokio::test]
    async fn a_read_still_running_is_kept_when_no_reading_names_its_file() {
        let lengths = Lengths::default();
        let file = RawPath::from_bytes(b"/media/video/stalled".to_vec());
        let (release, in_time) = a_stalled_read(&lengths, &file).await;
        assert_eq!(in_time, None);

        lengths.keep_only([]);

        assert_eq!(lengths.of_with(&file, soon(), no_second_read).await, None);
        release.send(()).expect("the read still waits");
        assert_eq!(kept(&lengths, &file).await, Some(EPISODE));
    }
}
