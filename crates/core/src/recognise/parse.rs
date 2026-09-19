//! What a filename spells, before anything is matched against it.
//!
//! One function, and everything the parser reports that this crate does not
//! read is dropped here rather than further down. **Which kinds are dropped is
//! written out rather than caught by a wildcard**, so a kind added upstream
//! stops the build and has to be given an answer instead of changing behaviour
//! quietly.
//!
//! Five kinds are kept and the choice was measured on 2026-09-20 rather than
//! taken from the plan, which asked for seven. `Part` did not fire on the form
//! the plan named: `Show Title Part 2` comes back with the part still inside the
//! title, because a bare unenclosed `Part` counts as ambiguous upstream. It does
//! fire on `Cour 2` and on an enclosed `(Season 1 Part 2)`. `EpisodeTitle` fired
//! and was wrong: `Show Title - 03 - The Episode Name` reported an episode title
//! of `The`. Neither earns a field.

use anitomy_ng::{ElementKind, Options};

use crate::encoding::{Confidence, decode};
use crate::path::RawPath;

/// What a filename says about which episode it holds.
///
/// Three cases and not an [`Option`], because a name spelling several episodes
/// and a name spelling none are different facts that lead different places. A
/// batch is a thing this program cannot record progress for; a film is a thing
/// it can. Collapsing them would leave an explanation with nothing to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episode {
    /// Exactly one, which is the only case anything downstream can record.
    Only(u32),
    /// Several, and therefore none of them.
    ///
    /// `01-12` comes back from the parser as two episodes rather than a range,
    /// measured on 2026-09-20. Neither number is the answer: a file holding
    /// twelve episodes is not the first and is not the twelfth, and writing
    /// either down is the failure this project is built to prevent.
    Several,
    /// One episode, spelled as something other than a whole number.
    ///
    /// `5.5` is a real episode of some series and there is nowhere to put it, so
    /// it is neither rounded into a neighbour it is not nor called absent.
    ///
    /// **Apart from [`Absent`](Episode::Absent) for the reason [`Several`](Episode::Several) is
    /// apart from it.** A film spells no episode and is a work this program can
    /// record as watched; a half episode is not. A consumer reading the two
    /// alike would mark a whole series watched because somebody played a recap.
    NotWhole,
    /// None spelled at all: a film, or a name that does not say.
    Absent,
}

/// What a filename spells.
///
/// The parser's answer, narrowed to what the stages below read, and nothing is
/// matched or judged yet. Every field is what the **name** said, never what a
/// corpus holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The title as the name spelled it, where the parser found one.
    pub title: Option<String>,
    /// The episode, or the reason there is not exactly one.
    pub episode: Episode,
    /// The season, where the name spelled one in a form the parser knows.
    ///
    /// `Season 2` and `2nd Season` both arrive here as two. A roman numeral
    /// does not: `Show Title II` comes back with the numeral still in the title
    /// and no season at all, measured on 2026-09-20, which is why the normaliser
    /// has to carry a rule of its own rather than trust this field.
    pub season: Option<u32>,
    /// The year, which is what tells a remake from its original.
    pub year: Option<u32>,
    /// The release group, kept because a corpus entry never carries one and the
    /// normaliser has to take it off both sides.
    pub release_group: Option<String>,
    /// Whether the text this was parsed from was read or inferred.
    ///
    /// A filename is bytes and every encoding is read, so a name that is not
    /// valid UTF-8 still arrives here - but by inference, and an inferred title
    /// is a weaker input than one that could not have been anything else. The
    /// fact travels with the parse so that an explanation can give it.
    pub confidence: Confidence,
}

/// Read a filename for what it says about the media.
///
/// Begins with a decode, because the parser takes text and this program stores
/// filenames as bytes. Nothing here fails: a name that says nothing produces a
/// `Parsed` saying nothing, which is an answer the stages below can act on.
#[must_use]
pub fn parse(name: &RawPath) -> Parsed {
    let decoded = decode(name.as_bytes());
    let mut parsed = Parsed {
        title: None,
        episode: Episode::Absent,
        season: None,
        year: None,
        release_group: None,
        confidence: decoded.confidence,
    };
    let mut episodes = Vec::new();

    for element in anitomy_ng::parse(&decoded.text, Options::default()) {
        // The first of a kind wins. The parser reports them in the order it
        // found them, so the first is the leftmost, and a name carrying two of
        // anything but an episode is a name this crate has no better rule for.
        match element.kind {
            ElementKind::Title if parsed.title.is_none() => parsed.title = Some(element.value),
            ElementKind::Season if parsed.season.is_none() => {
                parsed.season = element.value.parse().ok();
            }
            ElementKind::Year if parsed.year.is_none() => parsed.year = element.value.parse().ok(),
            ElementKind::ReleaseGroup if parsed.release_group.is_none() => {
                parsed.release_group = Some(element.value);
            }
            ElementKind::Episode => episodes.push(element.value),

            // Kept above, and reached here only where one was already found.
            ElementKind::Title
            | ElementKind::Season
            | ElementKind::Year
            | ElementKind::ReleaseGroup

            // Dropped, and listed rather than caught by a wildcard: a kind
            // added upstream then stops the build and has to be answered for.
            | ElementKind::AudioTerm
            | ElementKind::Device
            | ElementKind::EpisodeTitle
            | ElementKind::FileChecksum
            | ElementKind::FileExtension
            | ElementKind::Language
            | ElementKind::Other
            | ElementKind::Part
            | ElementKind::ReleaseInformation
            | ElementKind::ReleaseVersion
            | ElementKind::Source
            | ElementKind::Subtitles
            | ElementKind::Type
            | ElementKind::VideoResolution
            | ElementKind::VideoTerm
            | ElementKind::Volume => {}
        }
    }

    parsed.episode = match episodes.as_slice() {
        [one] => one.parse().map_or(Episode::NotWhole, Episode::Only),
        [] => Episode::Absent,
        _ => Episode::Several,
    };
    parsed
}

#[cfg(test)]
mod tests {
    use super::{Episode, Parsed, parse};
    use crate::encoding::Confidence;
    use crate::path::RawPath;

    /// Parse a name given as text, which is what every fixture here is.
    fn spelled(name: &str) -> Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    #[test]
    fn a_release_group_and_a_checksum_are_taken_off_the_title() {
        // The ordinary case, and the one every other fixture is a variation of.
        // The group is kept because the normaliser has to strip it from a
        // corpus entry too; the checksum and the resolution are dropped, and
        // nothing below here can see that they were ever present.
        let parsed = spelled("[SubsPlease] Show Title - 03 (1080p) [A1B2C3D4].mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode, Episode::Only(3));
        assert_eq!(parsed.release_group.as_deref(), Some("SubsPlease"));
    }

    #[test]
    fn a_name_spelling_two_episodes_spells_no_single_one() {
        // Measured on 2026-09-20 rather than assumed: `01-12` comes back as two
        // `Episode` elements, "01" and "12", and not as a range.
        //
        // Neither number is the answer. A file holding twelve episodes is not
        // episode one and is not episode twelve, and recording either is the
        // failure this project exists to avoid. Several and absent are kept
        // apart so that an explanation can say which happened.
        let parsed = spelled("[Group] Show Title - 01-12 (1080p).mkv");

        assert_eq!(parsed.episode, Episode::Several);
        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_film_spells_no_episode_at_all() {
        // Absent, and distinct from several. A film carries no episode and a
        // name that simply does not say carries none either; both are this
        // case, and neither is a number.
        let parsed = spelled("Show Title Movie (2019) [1080p].mkv");

        assert_eq!(parsed.episode, Episode::Absent);
        assert_eq!(parsed.year, Some(2019));
        // And absent is not the answer a fraction gets, which is the whole
        // reason there are four cases and not three.
        assert_ne!(parsed.episode, Episode::NotWhole);
    }

    #[test]
    fn two_spellings_of_a_season_reach_the_same_number() {
        // Half of the season rule, and the half the parser already does.
        // `Season 2` and `2nd Season` both come back as `Season = "2"`,
        // measured on 2026-09-20.
        let spelled_out = spelled("Show Title Season 2 - 03.mkv");
        let ordinal = spelled("Show Title 2nd Season - 03.mkv");

        assert_eq!(spelled_out.season, Some(2));
        assert_eq!(ordinal.season, Some(2));
        assert_eq!(spelled_out.title, ordinal.title);
    }

    #[test]
    fn a_roman_numeral_season_stays_in_the_title() {
        // The other half, and the parser does not do it: `Show Title II` comes
        // back whole, with no season at all. Measured on 2026-09-20, and it is
        // why the normaliser has to carry a rule for numerals rather than
        // trust this.
        //
        // The test is here rather than beside that rule because this is a fact
        // about the parser, and a parser that started handling numerals would
        // make the rule unreachable while every test of it still passed.
        let parsed = spelled("Show Title II - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title II"));
        assert_eq!(parsed.season, None);
    }

    #[test]
    fn a_year_in_parentheses_is_a_year_and_not_a_title() {
        // A year disambiguates a remake from its original, so it is kept rather
        // than dropped with the rest of the noise.
        let parsed = spelled("Show Title (2019) - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.year, Some(2019));
        assert_eq!(parsed.episode, Episode::Only(3));
    }

    #[test]
    fn a_name_that_is_not_valid_utf_8_is_parsed_and_says_it_was_inferred() {
        // A filename is bytes and every encoding is read. The name below is the
        // same Shift-JIS one the rest of the workspace tests against. It
        // parses, and the answer records that the text was inferred rather than
        // read, because a title recovered by guessing at an encoding is a
        // weaker input than one that could not be anything else.
        let shift_jis = [
            0x83, 0x5C, 0x83, 0x8C, 0x83, 0x62, 0x83, 0x5E, // "ソレッタ"
            0x20, 0x2D, 0x20, 0x30, 0x33, 0x2E, 0x6D, 0x6B, 0x76, // " - 03.mkv"
        ];
        let parsed = parse(&RawPath::from_bytes(shift_jis.to_vec()));

        assert_eq!(parsed.confidence, Confidence::Detected);
        assert_eq!(parsed.episode, Episode::Only(3));
    }

    #[test]
    fn a_name_that_is_valid_utf_8_says_the_text_was_read_and_not_guessed() {
        // The other side of the same fact, and the one that stops the field
        // from being a constant.
        let parsed = spelled("Show Title - 03.mkv");

        assert_eq!(parsed.confidence, Confidence::Certain);
    }

    #[test]
    fn an_episode_that_is_not_a_whole_number_is_neither_rounded_nor_absent() {
        // Measured on 2026-09-20: `05.5` arrives as one episode element whose
        // value is the text "05.5". Rounded into a neighbour it would record
        // progress against an episode nobody watched; called absent it would be
        // indistinguishable from a film, and a film is a work this program marks
        // watched as a whole. It is neither.
        let parsed = spelled("[Group] Show Title - 05.5 (1080p).mkv");

        assert_eq!(parsed.episode, Episode::NotWhole);
        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_reissued_release_is_still_the_episode_it_reissues() {
        // `03v2` is version two of episode three, not episode thirty-two and
        // not a version of anything this crate records. Measured on 2026-09-20:
        // the parser separates them, and the version is among the kinds dropped.
        let parsed = spelled("[Group] Show Title - 03v2.mkv");

        assert_eq!(parsed.episode, Episode::Only(3));
    }
}
