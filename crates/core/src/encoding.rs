//! Turning the bytes of a filename into text that can be read and parsed.
//!
//! A filename on Linux is bytes, and those bytes are frequently not UTF-8: an
//! archive unpacked with the wrong setting, a share mounted from Windows, a
//! file copied off a disc pressed in 2004. Refusing such a name means the
//! episode is never recognised; replacing the bytes it cannot read means it is
//! recognised as the wrong thing, which is worse.
//!
//! So every name is decoded, and the decision is **reported rather than
//! hidden**: the caller learns which encoding was used and whether that was a
//! fact or a guess.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use serde::{Deserialize, Serialize};

/// How far the text can be trusted.
///
/// Two-valued for the same reason [`crate::Known`] is three-valued: a fact and
/// an inference lead to different behaviour, and a type that cannot tell them
/// apart forces every consumer to assume the worse of the two or the better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    /// The bytes were valid UTF-8. Nothing was inferred.
    Certain,
    /// The bytes were not valid UTF-8, so an encoding was inferred from them.
    ///
    /// The text may still be wrong. A consumer that acts on it - writing
    /// progress to a list, say - should weigh that, and an interface showing it
    /// may say where it came from.
    Detected,
}

/// Text recovered from a byte string, with the decision that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedText {
    /// The text itself.
    pub text: String,
    /// The encoding used, named as the WHATWG standard names it.
    pub encoding: &'static str,
    /// Whether the encoding was known or inferred.
    pub confidence: Confidence,
}

/// Read a filename, whatever it was encoded in.
///
/// UTF-8 is tried first and never guessed at: valid UTF-8 is valid UTF-8, and
/// running a detector over it would invite a confident wrong answer on a short
/// name. Only when that fails is an encoding inferred, and the result then says
/// so.
///
/// This never fails. Every byte string yields text, because a name benshi
/// cannot read is still a name the user can see, and refusing to render it
/// turns a recognition problem into a blank screen.
#[must_use]
pub fn decode(bytes: &[u8]) -> DecodedText {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return DecodedText {
            text: text.to_owned(),
            encoding: "UTF-8",
            confidence: Confidence::Certain,
        };
    }

    // ISO-2022-JP is allowed. The detector's own warning against it applies to
    // browsers rendering pages that can run scripts; this is a filename, and
    // the encoding is a real one that real Japanese files are named in.
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Allow);
    detector.feed(bytes, true);

    // UTF-8 is denied as a guess because it has already been ruled out above:
    // these bytes are provably not valid UTF-8.
    //
    // Named with its full type because the decoder that follows comes from
    // `encoding_rs`, which is therefore a dependency this crate really uses
    // rather than one it inherits and leaves unmentioned.
    let encoding: &'static encoding_rs::Encoding = detector.guess(None, Utf8Detection::Deny);

    // The dropped flag says whether any byte was unmappable and became a
    // replacement character. It does not change the answer: the encoding was
    // inferred either way, and `Detected` already says the text may be wrong.
    let (text, _had_errors) = encoding.decode_without_bom_handling(bytes);

    DecodedText {
        text: text.into_owned(),
        encoding: encoding.name(),
        confidence: Confidence::Detected,
    }
}

#[cfg(test)]
mod tests {
    use super::{Confidence, decode};

    #[test]
    fn plain_ascii_is_utf8_and_certain() {
        let decoded = decode(b"[Group] Show - 03.mkv");
        assert_eq!(decoded.text, "[Group] Show - 03.mkv");
        assert_eq!(decoded.encoding, "UTF-8");
        assert_eq!(decoded.confidence, Confidence::Certain);
    }

    #[test]
    fn valid_utf8_japanese_is_certain_and_never_guessed_at() {
        let decoded = decode("日本語.mkv".as_bytes());
        assert_eq!(decoded.text, "日本語.mkv");
        assert_eq!(decoded.encoding, "UTF-8");
        assert_eq!(decoded.confidence, Confidence::Certain);
    }

    #[test]
    fn shift_jis_is_detected_and_reported_as_a_guess() {
        // "ソレユケ" in Shift-JIS. Not valid UTF-8.
        let decoded = decode(b"\x83\x5c\x83\x8c\x83\x86\x83\x50");
        assert_eq!(decoded.text, "ソレユケ");
        assert_eq!(decoded.confidence, Confidence::Detected);
    }

    #[test]
    fn windows_1251_is_detected_and_reported_as_a_guess() {
        // "Привет всем" in windows-1251. Not valid UTF-8.
        let decoded = decode(b"\xcf\xf0\xe8\xe2\xe5\xf2 \xe2\xf1\xe5\xec");
        assert_eq!(decoded.text, "Привет всем");
        assert_eq!(decoded.confidence, Confidence::Detected);
    }

    #[test]
    fn a_guess_never_claims_to_be_certain() {
        // The distinction is the whole point: a caller showing a title to a
        // user, or deciding whether to trust a recognition result, must be able
        // to tell a fact from an inference. Collapsing the two is the same
        // mistake as collapsing Known into Option.
        let guessed = decode(b"\x83\x5c\x83\x8c");
        assert_ne!(guessed.confidence, Confidence::Certain);
    }

    #[test]
    fn decoding_never_fails() {
        // Every byte string produces text. A filename benshi cannot read is
        // still a filename the user can see, and refusing to render it turns a
        // recognition problem into a blank screen.
        for bytes in [
            b"\xff\xfe\xfd".as_slice(),
            b"".as_slice(),
            b"\x00\x01\x02".as_slice(),
        ] {
            let decoded = decode(bytes);
            assert!(!decoded.encoding.is_empty());
        }
    }

    #[test]
    fn an_empty_name_is_empty_text() {
        assert_eq!(decode(b"").text, "");
    }
}
