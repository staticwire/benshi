//! What a filename spells, before anything is matched against it.
//!
//! One function, and everything the parser reports that this crate does not
//! read is dropped here rather than further down. **Which kinds are dropped is
//! written out rather than caught by a wildcard**, so a kind added upstream
//! stops the build and has to be given an answer instead of changing behaviour
//! quietly.
//!
//! Six kinds are kept. `EpisodeTitle` is not one of them: for
//! `Show Title - 03 - The Episode Name` the parser reports an episode title of
//! `The`. `Part` is, because AniList lists the second part of some seasons as
//! an entry of its own.
//!
//! **The parser also loses a part in more than one place, and one of them is
//! read back here.** A bare `Part` counts upstream only inside brackets, and a
//! title ends at the season marker, so in `Show Title Season 3 Part 2 - 03`
//! the part belongs to no element at all. [`parse`] reads it back from that
//! one place and no other.

use anitomy_ng::{Element, ElementKind, Options};

use crate::encoding::{Confidence, decode};
use crate::path::RawPath;
use crate::recognise::normalise::part_in;

/// What a filename says about which episode it holds.
///
/// Four cases and not an [`Option`], because a name spelling several episodes
/// and a name spelling none are different facts that lead different places. A
/// batch is a thing this program cannot record progress for; a film is a thing
/// it can. Collapsing them would leave an explanation with nothing to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Episode {
    /// Exactly one, the only case that names an episode to record progress
    /// against.
    Only(u32),
    /// Several, and therefore none of them.
    ///
    /// `01-12` comes back from the parser as two episodes rather than a range.
    /// Neither number is the answer: a file holding twelve episodes is not the
    /// first and is not the twelfth, and writing either down is the failure
    /// this project is built to prevent.
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
    /// `Season 2`, `2nd Season` and `Season II` all arrive here as two. A
    /// numeral ending the title does not: `Show Title II` comes back with the
    /// numeral still in the title and no season at all, which is why the
    /// normaliser carries a rule of its own for it.
    pub season: Option<u32>,
    /// The part of a season, where the parser reported one or [`parse`] read
    /// one back from after the season marker.
    ///
    /// What the name said and nothing decided about it: `Part 1` is one here,
    /// and whether a first part is the same list entry as no part at all is the
    /// key's rule. A part the name spells inside the title, with no season in
    /// front of it, stays in [`title`](Parsed::title) where the parser left it.
    pub part: Option<u32>,
    /// The year, which is what tells a remake from its original.
    pub year: Option<u32>,
    /// The release group, which the parser reports apart from the title.
    ///
    /// Taken off here and nowhere later. Brackets in a list entry are part of
    /// its name - `[Oshi no Ko]` is the whole romaji title of one series on
    /// AniList - so a rule stripping a bracketed tag from text would reduce
    /// that title to nothing.
    pub release_group: Option<String>,
    /// Whether the text this was parsed from was read or inferred.
    ///
    /// A filename is bytes and every encoding is read, so a name that is not
    /// valid UTF-8 still arrives here - but by inference, and an inferred title
    /// is a weaker input than one that could not have been anything else. The
    /// fact travels with the parse so that an explanation can give it.
    pub confidence: Confidence,
    /// The name with everything the parser could name as noise struck out.
    ///
    /// **What a stage comparing text is given, and it is not
    /// [`title`](Parsed::title).** A title ends at the first thing the parser
    /// recognises, so a name spelling `Show Title - The Subtitle - 03` reports
    /// a title of `Show Title` and the subtitle belongs to no element at all.
    /// A stage reading the title alone cannot tell such a file from the series
    /// it extends. What is struck out is the release group, the episode, the
    /// season, the part, the year, the resolution, the source, the extension
    /// and the rest of the release's own vocabulary - everything a list never
    /// spells, and nothing a name might be.
    ///
    /// A space is left wherever something was struck out, so that the words on
    /// either side of it stay two words.
    pub text: String,
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
        part: None,
        year: None,
        release_group: None,
        confidence: decoded.confidence,
        text: String::new(),
    };
    let mut episodes = Vec::new();
    let elements = anitomy_ng::parse(&decoded.text, Options::default());
    let lost_part = part_lost_after_season(&decoded.text, &elements);
    let cut_marks = marks_cut_off_the_title(&decoded.text, &elements);
    parsed.text = without_the_release(&decoded.text, &elements);

    for element in elements {
        // The first of a kind wins. The parser sorts what it reports by
        // position, so the first is the leftmost, and a name carrying two of
        // anything but an episode is a name this crate has no better rule for.
        match element.kind {
            ElementKind::Title if parsed.title.is_none() => parsed.title = Some(element.value),
            ElementKind::Season if parsed.season.is_none() => {
                parsed.season = element.value.parse().ok();
            }
            ElementKind::Part if parsed.part.is_none() => parsed.part = element.value.parse().ok(),
            ElementKind::Year if parsed.year.is_none() => parsed.year = element.value.parse().ok(),
            ElementKind::ReleaseGroup if parsed.release_group.is_none() => {
                parsed.release_group = Some(element.value);
            }
            ElementKind::Episode => episodes.push(element.value),

            // Kept above, and reached here only where one was already found.
            ElementKind::Title
            | ElementKind::Season
            | ElementKind::Part
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
    parsed.part = parsed.part.or(lost_part);
    if let Some(title) = &mut parsed.title {
        title.push_str(&cut_marks);
    }
    parsed
}

/// The marks the parser cut off the end of the title, read back from the name.
///
/// The parser counts `:`, `,`, `+`, `|`, `&`, `.` and the dashes as
/// separators and trims them off the end of a title, so `Show Title: - 03`
/// arrives as `Show Title`. It keeps a `.` that a space follows. Some sequels
/// are told apart from their first season by such a mark alone. So a run of
/// them right after the title is read back where whitespace, an underscore or
/// the end of the name follows it. Where a letter follows, as in
/// `Show.Title.S02E03`, the run is the name's own separator.
fn marks_cut_off_the_title(text: &str, elements: &[Element]) -> String {
    let Some(title) = elements
        .iter()
        .find(|element| element.kind == ElementKind::Title)
    else {
        return String::new();
    };
    let mut after = text
        .chars()
        .skip(title.position + title.value.chars().count())
        .peekable();
    let mut cut = String::new();
    while let Some(mark) = after.next_if(|&c| is_trimmed_separator(c)) {
        cut.push(mark);
    }

    if after.next().is_none_or(|c| c.is_whitespace() || c == '_') {
        cut
    } else {
        String::new()
    }
}

/// The separators the parser trims off the end of a title, apart from the
/// spaces and underscores that stand between words.
fn is_trimmed_separator(c: char) -> bool {
    matches!(
        c,
        ':' | ',' | '+' | '|' | '&' | '.' | '-' | '\u{00AD}' | '\u{2010}'..='\u{2015}'
    )
}

/// A part the parser read past without reporting it anywhere.
///
/// Looked for only between the season element and the element after it. The
/// parser loses one there because the title ends at the season marker and a
/// bare `Part` counts upstream only inside brackets. It does not read
/// `Part II`, `第2クール` or `Partie 2` at all. After the episode, a `Part` is
/// part of the episode's own title, and the parser is right about that. A
/// `Part` after a year or a resolution is lost the same way, as in
/// `Show Title (2019) Part 2 - 03`, and nothing here reads that one back.
///
/// The part is read with the rule a list entry's text is read with, so that a
/// filename and the entry spelling it agree. The parser counts positions in
/// `char`s, so the text is cut by characters rather than by bytes.
fn part_lost_after_season(text: &str, elements: &[Element]) -> Option<u32> {
    let mut from_season = elements
        .iter()
        .skip_while(|element| element.kind != ElementKind::Season);
    let start = from_season.next()?.position;
    let end = from_season.next().map_or(usize::MAX, |next| next.position);
    let between: String = text
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();

    part_in(&between)
}

/// The name with everything the release itself spells struck out.
///
/// Kept: the title, an episode title, and anything the parser could not place,
/// which is where a subtitle beyond the end of the title ends up. Struck out:
/// every other kind, because a list entry never spells a resolution, a group or
/// an episode number, and counting them would tell against every candidate
/// alike.
///
/// What goes is replaced by a space rather than removed, so that two words
/// which were apart stay apart. Positions come from the parser and are counted
/// in `char`s.
fn without_the_release(text: &str, elements: &[Element]) -> String {
    let mut kept: Vec<char> = text.chars().collect();
    for element in elements {
        if matches!(
            element.kind,
            ElementKind::Title | ElementKind::EpisodeTitle | ElementKind::Other
        ) {
            continue;
        }
        let from = element.position.min(kept.len());
        let upto = (from + element.value.chars().count()).min(kept.len());
        for slot in &mut kept[from..upto] {
            *slot = ' ';
        }
    }
    kept.into_iter().collect()
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
    fn the_release_is_struck_out_of_the_text_a_stage_compares() {
        // A title ends at the first thing the parser recognises, so a subtitle
        // beyond it belongs to no element at all and a stage reading the title
        // alone cannot tell this file from the series it extends. What the
        // release itself spells goes, because no list entry spells any of it.
        let parsed =
            spelled("[SubsPlease] Show Title Cour 2 - The Subtitle - 03 (1080p) [A1B2C3D4].mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert!(
            parsed.text.contains("Subtitle"),
            "the subtitle is gone: {:?}",
            parsed.text
        );
        for noise in ["SubsPlease", "1080p", "A1B2C3D4", "mkv"] {
            assert!(
                !parsed.text.contains(noise),
                "{noise} is still in {:?}",
                parsed.text
            );
        }
    }

    #[test]
    fn a_release_group_and_a_checksum_are_taken_off_the_title() {
        // The ordinary case, and the one every other fixture is a variation of.
        // The group is kept; the checksum and the resolution are dropped, and
        // nothing below here can see that they were ever present.
        let parsed = spelled("[SubsPlease] Show Title - 03 (1080p) [A1B2C3D4].mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode, Episode::Only(3));
        assert_eq!(parsed.release_group.as_deref(), Some("SubsPlease"));
    }

    #[test]
    fn a_name_spelling_two_episodes_spells_no_single_one() {
        // `01-12` comes back as two `Episode` elements, "01" and "12", and not
        // as a range.
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
        // `Season 2` and `2nd Season` both come back as `Season = "2"`.
        let spelled_out = spelled("Show Title Season 2 - 03.mkv");
        let ordinal = spelled("Show Title 2nd Season - 03.mkv");

        assert_eq!(spelled_out.season, Some(2));
        assert_eq!(ordinal.season, Some(2));
        assert_eq!(spelled_out.title, ordinal.title);
    }

    #[test]
    fn a_roman_numeral_season_stays_in_the_title() {
        // The other half, and the parser does not do it: `Show Title II` comes
        // back whole, with no season at all, so the normaliser reads the
        // numeral out of the title.
        //
        // The test is here rather than beside that rule because this is a fact
        // about the parser. A parser that started reading such a numeral would
        // change what reaches the normaliser from a filename, and this test is
        // what would say so.
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
        // `05.5` arrives as one episode element whose value is the text "05.5".
        // Rounded into a neighbour it would record progress against an episode
        // nobody watched; called absent it would be indistinguishable from a
        // film, and a film is a work this program marks watched as a whole. It
        // is neither.
        let parsed = spelled("[Group] Show Title - 05.5 (1080p).mkv");

        assert_eq!(parsed.episode, Episode::NotWhole);
        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_reissued_release_is_still_the_episode_it_reissues() {
        // `03v2` is version two of episode three, not episode thirty-two and
        // not a version of anything this crate records. The parser separates
        // them, and the version is among the kinds dropped.
        let parsed = spelled("[Group] Show Title - 03v2.mkv");

        assert_eq!(parsed.episode, Episode::Only(3));
    }

    #[test]
    fn a_part_the_parser_reports_is_kept() {
        // `Cour 2` is a part the parser recognises on its own. AniList lists
        // the second part of some seasons as an entry of its own, so a name
        // that spells one has to say so.
        let parsed = spelled("Show Title Season 2 Cour 2 - 03.mkv");

        assert_eq!(parsed.season, Some(2));
        assert_eq!(parsed.part, Some(2));
    }

    #[test]
    fn a_part_after_a_season_is_found_where_the_parser_loses_it() {
        // For every one of these the parser reports a title of `Show Title` and
        // a season of three, and the part appears nowhere: in no element and
        // not in the title. A bare `Part` counts upstream only inside brackets,
        // and the title ends at the season, so what lies between the season and
        // the episode belongs to nobody.
        //
        // Left there, a file of a season's second part would reach the list
        // entry for its first.
        for name in [
            "Show Title Season 3 Part 2 - 03.mkv",
            "Show Title S3 Part 2 - 03.mkv",
            "Show Title 3rd Season Part 2 - 03.mkv",
            "Show Title Season 3 - Part 2 - 03.mkv",
            "Show Title Season 3 Part.2 - 03.mkv",
            "Show Title Season 3 Part II - 03.mkv",
        ] {
            let parsed = spelled(name);

            assert_eq!(parsed.title.as_deref(), Some("Show Title"), "{name}");
            assert_eq!(parsed.season, Some(3), "{name}");
            assert_eq!(parsed.part, Some(2), "{name}");
        }
    }

    #[test]
    fn a_part_in_another_form_after_a_season_is_found_too() {
        // The parser reads the season in these and reports nothing between it
        // and the episode. It reads no part in these forms anywhere.
        for name in [
            "ショータイトル 第2期 第2クール - 03.mkv",
            "Show Title Saison 2 Partie 2 - 03.mkv",
        ] {
            let parsed = spelled(name);

            assert_eq!(parsed.season, Some(2), "{name}");
            assert_eq!(parsed.part, Some(2), "{name}");
        }
    }

    #[test]
    fn a_part_after_the_episode_names_the_episode_and_not_the_season() {
        // What the parser is right to leave alone: after the episode number,
        // `Part 1` is part of an episode's own title. Read as the season's
        // part, it would report something the name did not say. A `Part 2` in
        // its place would send the file to a list entry it does not belong to.
        let parsed = spelled("Show Title Season 3 - 03 - The Comeback, Part 1.mkv");

        assert_eq!(parsed.season, Some(3));
        assert_eq!(parsed.part, None);
    }

    #[test]
    fn a_part_inside_the_title_stays_in_the_title() {
        // With no season in front of it, the parser keeps `Part 2` inside the
        // title. Nothing here reads it out. A list entry is text that never
        // meets the parser, so the normaliser has to take a part out of a
        // title's text anyway, and one place doing that is one rule where two
        // would have to agree.
        let parsed = spelled("Show Title Part 2 - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title Part 2"));
        assert_eq!(parsed.part, None);
    }

    #[test]
    fn a_first_part_is_reported_as_the_first() {
        // What the name spelled, and nothing decided about it. Whether a first
        // part is the same list entry as no part at all is the key's rule; the
        // parse only reports that the name said `Part 1`.
        let parsed = spelled("Show Title Season 3 Part 1 - 03.mkv");

        assert_eq!(parsed.part, Some(1));
    }

    #[test]
    fn a_mark_the_parser_cuts_off_the_title_is_read_back() {
        // The parser counts `:`, `,`, `+`, `|` and `&` as separators and trims
        // them off the end of a title. Left there, a file of `Show Title:`
        // would reach the list entry `Show Title`, and some sequels are told
        // apart from their first season by nothing else.
        for (name, title) in [
            ("[Group] Show Title: - 03 [1080p].mkv", "Show Title:"),
            ("Show Title, - 03.mkv", "Show Title,"),
            ("Show Title+ - 03.mkv", "Show Title+"),
        ] {
            assert_eq!(spelled(name).title.as_deref(), Some(title), "{name}");
        }
    }

    #[test]
    fn a_mark_is_read_back_where_underscores_separate_the_words() {
        let parsed = spelled("Show_Title:_-_03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title:"));
    }

    #[test]
    fn a_mark_ending_the_name_is_read_back() {
        let parsed = spelled("Show Title:");

        assert_eq!(parsed.title.as_deref(), Some("Show Title:"));
    }

    #[test]
    fn a_closing_bracket_after_the_title_is_not_read_back() {
        // The parser takes a bracketed title from inside its brackets, and the
        // bracket that closes it is not a mark it cut.
        let parsed = spelled("[Group] [Show Title] - 03 [1080p].mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_dot_between_the_words_of_a_name_is_not_read_back() {
        // Where dots separate the words, the dot after the title is the next
        // separator and says nothing about the title.
        let parsed = spelled("Show.Title.S02E03.1080p.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_separator_standing_apart_is_not_read_back() {
        let parsed = spelled("Show Title - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
    }

    #[test]
    fn a_mark_the_parser_keeps_is_not_read_twice() {
        // The parser keeps `!` inside the title, so what follows the title is
        // the space before the episode, and nothing is added.
        let parsed = spelled("Show Title! - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title!"));
    }

    #[test]
    fn a_mark_is_read_back_behind_a_title_that_is_not_ascii() {
        let parsed = spelled("ソレッタ: - 03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("ソレッタ:"));
    }

    #[test]
    fn a_part_is_found_behind_a_title_that_is_not_ascii() {
        // The parser counts positions in `char`s. A title in katakana is four
        // characters and twelve bytes, so the text between the season and the
        // episode is found only when it is cut by characters.
        let parsed = spelled("ソレッタ Season 3 Part 2 - 03.mkv");

        assert_eq!(parsed.season, Some(3));
        assert_eq!(parsed.part, Some(2));
    }
}
