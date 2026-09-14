//! Filesystem paths as the platform actually reports them.
//!
//! A path on Linux is an arbitrary byte string. A filename extracted from a
//! Japanese archive with the wrong encoding, or copied from a Windows share, is
//! routinely not valid UTF-8, and benshi must carry it through untouched rather
//! than refuse it or mangle it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A filesystem path exactly as the platform reported it.
///
/// A path on Linux is an arbitrary byte string, so this type holds bytes. It is
/// never opened: callers parse the filename and display it.
///
/// The serialised form keeps runs of valid UTF-8 verbatim, so an ordinary name
/// stays readable in a trace, and escapes every other byte, plus the escape
/// character itself, as `%XX`. The mapping is reversible for any input.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RawPath(Vec<u8>);

impl RawPath {
    /// Take a path as the platform reported it.
    #[must_use]
    pub const fn from_bytes(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The path as the platform reported it.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Everything after the last `/`, or the whole path if there is none.
    ///
    /// Only `/` separates. A backslash does not, because 0x5C is the trailing
    /// byte of several Shift-JIS characters and splitting on it would cut a
    /// Japanese filename in half. A platform whose separator is the backslash
    /// normalises before constructing this type: which byte separates is
    /// platform knowledge, and an adapter is where platform knowledge lives.
    #[must_use]
    pub fn file_name(&self) -> &[u8] {
        match self.0.iter().rposition(|byte| *byte == b'/') {
            Some(last) => &self.0[last + 1..],
            None => &self.0,
        }
    }
}

/// Shows the escaped form, which is readable for the cases that are readable
/// and unambiguous for the cases that are not. A derived implementation would
/// print a wall of byte values.
impl fmt::Debug for RawPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RawPath({:?})", escape(&self.0))
    }
}

impl Serialize for RawPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&escape(&self.0))
    }
}

impl<'de> Deserialize<'de> for RawPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let escaped = String::deserialize(deserializer)?;
        unescape(&escaped).map(Self).map_err(de::Error::custom)
    }
}

/// Render bytes so that the result is valid UTF-8 and the mapping is reversible.
///
/// Runs of valid UTF-8 are emitted as they are; every other byte, and every
/// literal `%`, becomes `%XX`. The escape must escape itself or a filename
/// containing a per-cent sign decodes as something else.
fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());

    // `utf8_chunks` splits the input the way this escape needs it: each chunk
    // is a run that decodes as text followed by the bytes that do not, with a
    // sequence cut short by the end of the input counting as the latter.
    for chunk in bytes.utf8_chunks() {
        push_text(&mut out, chunk.valid());
        for byte in chunk.invalid() {
            push_escaped(&mut out, *byte);
        }
    }

    out
}

fn push_text(out: &mut String, text: &str) {
    for character in text.chars() {
        if character == '%' {
            push_escaped(out, b'%');
        } else {
            out.push(character);
        }
    }
}

fn push_escaped(out: &mut String, byte: u8) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(DIGITS[usize::from(byte >> 4)]));
    out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
}

/// Recover the bytes an [`escape`] call rendered.
fn unescape(escaped: &str) -> Result<Vec<u8>, String> {
    let source = escaped.as_bytes();
    let mut out = Vec::with_capacity(source.len());
    let mut index = 0;

    while index < source.len() {
        if source[index] != b'%' {
            out.push(source[index]);
            index += 1;
            continue;
        }

        let digits = source
            .get(index + 1..index + 3)
            .ok_or_else(|| format!("truncated escape at byte {index}"))?;
        let high = hex_value(digits[0])?;
        let low = hex_value(digits[1])?;
        out.push(high << 4 | low);
        index += 3;
    }

    Ok(out)
}

fn hex_value(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        other => Err(format!(
            "{:?} is not a hexadecimal digit",
            char::from(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::RawPath;

    /// The Shift-JIS encoding of an ordinary Japanese filename.
    ///
    /// Not valid UTF-8. The second byte of the first character is 0x5C, which
    /// is also the ASCII backslash - the classic trap in this encoding.
    const SHIFT_JIS: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e.mkv";

    /// The windows-1251 encoding of a Cyrillic filename. Not valid UTF-8.
    const CP1251: &[u8] = b"/anime/\xcf\xf0\xe8\xe2\xe5\xf2.mkv";

    #[test]
    fn a_utf8_path_round_trips() {
        let path = RawPath::from_bytes(b"/anime/[Group] Show - 03.mkv".to_vec());
        let text = serde_json::to_string(&path).expect("serialise");
        let back: RawPath = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(path, back);
    }

    #[test]
    fn a_japanese_utf8_path_round_trips_and_stays_readable() {
        let path = RawPath::from_bytes("/anime/日本語.mkv".as_bytes().to_vec());
        let text = serde_json::to_string(&path).expect("serialise");

        // Valid UTF-8 passes through unescaped, so a trace stays greppable by
        // the name a person would type.
        assert!(
            text.contains("日本語"),
            "expected readable text, got {text}"
        );

        let back: RawPath = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(path, back);
    }

    #[test]
    fn a_shift_jis_path_round_trips_byte_for_byte() {
        let path = RawPath::from_bytes(SHIFT_JIS.to_vec());
        let text = serde_json::to_string(&path).expect("serialise");
        let back: RawPath = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(path, back);
        assert_eq!(back.as_bytes(), SHIFT_JIS);
    }

    #[test]
    fn a_windows_1251_path_round_trips_byte_for_byte() {
        let path = RawPath::from_bytes(CP1251.to_vec());
        let text = serde_json::to_string(&path).expect("serialise");
        let back: RawPath = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(path, back);
        assert_eq!(back.as_bytes(), CP1251);
    }

    #[test]
    fn an_invalid_path_still_serialises_as_a_string() {
        // Not an array of numbers: a trace is read by people, and a wall of
        // byte values is not readable at any length.
        let path = RawPath::from_bytes(SHIFT_JIS.to_vec());
        let text = serde_json::to_string(&path).expect("serialise");
        assert!(text.starts_with('"'), "expected a JSON string, got {text}");
    }

    #[test]
    fn a_literal_percent_survives_the_escape() {
        // The escape character has to escape itself, or a filename containing
        // a per-cent sign decodes as something else entirely.
        let path = RawPath::from_bytes(b"/anime/100%25 Pascal-sensei.mkv".to_vec());
        let text = serde_json::to_string(&path).expect("serialise");
        let back: RawPath = serde_json::from_str(&text).expect("deserialise");
        assert_eq!(path, back);
    }

    #[test]
    fn every_byte_value_round_trips() {
        // Exhaustive rather than representative: the encoding is a bijection on
        // byte strings or it is not, and there are only 256 cases to check.
        let all: Vec<u8> = (0..=u8::MAX).collect();
        let path = RawPath::from_bytes(all.clone());

        let text = serde_json::to_string(&path).expect("serialise");
        let back: RawPath = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(back.as_bytes(), all.as_slice());
    }

    #[test]
    fn the_file_name_is_what_follows_the_last_separator() {
        let path = RawPath::from_bytes(SHIFT_JIS.to_vec());
        assert_eq!(path.file_name(), &SHIFT_JIS[b"/anime/".len()..]);
    }

    #[test]
    fn a_path_with_no_separator_is_all_file_name() {
        let path = RawPath::from_bytes(b"Show - 03.mkv".to_vec());
        assert_eq!(path.file_name(), b"Show - 03.mkv");
    }

    #[test]
    fn a_backslash_does_not_split_a_shift_jis_character() {
        // 0x5C is both the ASCII backslash and the trailing byte of several
        // Shift-JIS characters. Splitting on it would cut a filename in half,
        // which is the exact bug that makes this encoding notorious.
        let path = RawPath::from_bytes(SHIFT_JIS.to_vec());
        assert!(path.file_name().starts_with(b"\x83\x5c"));
    }
}
