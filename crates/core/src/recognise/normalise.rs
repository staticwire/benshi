//! The key a title is filed and looked up under.
//!
//! A list entry is text, and a filename is text the parser has already taken
//! apart, so neither is compared as it stands. Both are brought to a [`Key`],
//! so that a filename's key can be compared with the keys the list's entries
//! are filed under. A key forgives case, width, whitespace and punctuation
//! between words, the way a season or a part is spelled, a release group, and a
//! year or a type in brackets.
//!
//! **The marks at the very end of a title stay in the key.** `K-ON!` and
//! `K-ON!!` are different seasons, as are `Nisekoi` and `Nisekoi:`, and nothing
//! else tells them apart. The end is taken once the season, the part and the
//! year are out, so the `!!` of `Haikyuu!! 2nd Season` is a mark. Brackets and
//! dashes are not marks: `【Oshi no Ko】` reaches what the parser leaves of
//! `[Oshi no Ko]`, and the last dash of `Title -Subtitle-` closes the subtitle.
//!
//! **A season and a part are read out of the text**, because a list entry is
//! text that never meets the parser. `Season 2`, `2nd Season`, `Second Season`,
//! `Saison 2`, `S2`, `第2期` and a numeral from `II` to `IX` as the last word are
//! one season; `Part 2`, `Part II`, `Pt 2`, `Cour 2`, `2nd Cour`, `Parte 2`,
//! `Partie 2`, `P2` and `第2クール` are one part. `Part` and `Pt` are words with
//! other meanings, so beside a part in brackets, or one spelled beyond
//! mistaking, a bare `Part` stays part of the name. Standing alone it is still
//! read, because that is how a list spells a part; the parser is stricter and
//! counts a bare one only inside brackets. A part is never a season: AniList
//! lists the second half of some seasons as an entry of its own, spelled
//! `Part 2`, beside a whole second season spelled `Season 2`. A first season
//! and a first part carry no number, because `Show.S01E05` is the entry the
//! list calls `Show`.
//!
//! A numeral ending a title or following `Season` or `Saison` counts only in
//! uppercase, and `X` and `I` never count there: `X` ending a title is part of
//! its name rather than ten, `I` is a pronoun or a first film, and `Ii` is a
//! Japanese word. After `Part`, `Parte`, `Partie` or `Pt`, a numeral from `I`
//! to `X` counts in either case. A bare number at the end is not a season
//! either: the `0` of `Steins;Gate 0` is part of its name.
//!
//! Text is brought to NFKC first, so full-width letters and a numeral written
//! as one character, `Ⅱ`, reach their ordinary forms. `×` then becomes `x`, so
//! `HUNTER×HUNTER` meets `Hunter x Hunter`. A year in brackets or standing as a
//! word of its own is dropped wherever it stands, and so is a type in brackets,
//! because neither reaches the key from a filename: a list tells a series from
//! the film of its name by `(TV)` and `(Movie)`, and the parser takes the year
//! it reads, and a bracketed type, out of the title it reports. Case goes by
//! `str::to_lowercase` rather than full case folding, which differs on a few
//! letters such as German `ß` and Greek `ς`. A combining mark belongs to the
//! letter it sits on, so a Thai tone mark is neither erased nor taken for
//! punctuation.

use std::ops::Range;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::recognise::parse::Parsed;

/// The form a title is compared in.
///
/// Built from a list entry's text by [`Key::from_title`] and from a parsed
/// filename by [`Key::from_parsed`], so that the two can be compared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    letters: String,
    marks: String,
    season: Option<u32>,
    part: Option<u32>,
}

impl Key {
    /// The key a list entry spelled this way is filed under.
    ///
    /// Nothing where no letter or digit is left once the season, the part and
    /// the year are taken out: `Season 2` alone names no entry.
    #[must_use]
    pub fn from_title(title: &str) -> Option<Self> {
        let spelled = Spelled::read(title, Reported::default());
        Self::new(spelled.letters, spelled.marks, spelled.season, spelled.part)
    }

    /// The key a filename is looked up by.
    ///
    /// Where the parser reported a season, the title left behind is not read
    /// for another: a numeral ending it is part of the name, as it is in the
    /// list entry that spells the season out. The same holds for a part.
    #[must_use]
    pub fn from_parsed(parsed: &Parsed) -> Option<Self> {
        let reported = Reported {
            season: parsed.season.is_some(),
            part: parsed.part.is_some(),
        };
        let spelled = Spelled::read(parsed.title.as_deref()?, reported);
        Self::new(
            spelled.letters,
            spelled.marks,
            parsed.season.or(spelled.season),
            parsed.part.or(spelled.part),
        )
    }

    fn new(letters: String, marks: String, season: Option<u32>, part: Option<u32>) -> Option<Self> {
        let not_the_first = |number: &u32| *number != 1;
        (!letters.is_empty()).then(|| Self {
            letters,
            marks,
            season: season.filter(not_the_first),
            part: part.filter(not_the_first),
        })
    }
}

/// Which of the season and the part the parser has already read.
#[derive(Clone, Copy, Default)]
struct Reported {
    season: bool,
    part: bool,
}

/// What a title's text spells, before a first season is told from none.
struct Spelled {
    letters: String,
    marks: String,
    season: Option<u32>,
    part: Option<u32>,
}

impl Spelled {
    fn read(title: &str, reported: Reported) -> Self {
        let text: String = title
            .nfkc()
            .map(|c| if c == '×' { 'x' } else { c })
            .collect();
        let mut text = without_markers(&text);
        let part = if reported.part {
            None
        } else {
            take(&mut text, part_at)
        };
        let season = if reported.season {
            None
        } else {
            take(&mut text, season_at).or_else(|| take_ending_numeral(&mut text))
        };

        let name_end = text
            .char_indices()
            .rev()
            .find(|&(_, c)| is_letter(c))
            .map_or(0, |(at, c)| at + c.len_utf8());
        let (name, tail) = text.split_at(name_end);
        Self {
            letters: name
                .to_lowercase()
                .chars()
                .filter(|&c| is_letter(c))
                .collect(),
            marks: tail
                .chars()
                .filter(|&c| !is_word_break(c) && !is_bracket(c) && !is_dash(c))
                .collect(),
            season,
            part,
        }
    }
}

/// The part a stretch of text spells, in any form a title's part is read in.
///
/// For the parse, which reads back a part the parser lost by the same rule a
/// list entry's text is read with.
pub(super) fn part_in(text: &str) -> Option<u32> {
    let mut text: String = text.nfkc().collect();
    take(&mut text, part_at)
}

/// A letter or a digit of a name, with any combining mark on it.
fn is_letter(c: char) -> bool {
    c.is_alphanumeric() || is_combining_mark(c)
}

/// Opening and closing brackets, as NFKC leaves them.
///
/// The full-width forms of the ASCII brackets are not listed, because NFKC has
/// already turned them into the ASCII ones by the time this is read.
const BRACKETS: [(char, char); 13] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('<', '>'),
    ('【', '】'),
    ('「', '」'),
    ('『', '』'),
    ('〈', '〉'),
    ('《', '》'),
    ('〔', '〕'),
    ('〖', '〗'),
    ('〘', '〙'),
    ('〚', '〛'),
];

fn is_bracket(c: char) -> bool {
    BRACKETS
        .iter()
        .any(|&(open, close)| c == open || c == close)
}

/// What stands between words: whitespace, the zero-width space the parser
/// counts as a space, and the underscore it reads as one in a name joined by
/// underscores.
fn is_word_break(c: char) -> bool {
    c.is_whitespace() || c == '_' || c == '\u{200B}'
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '\u{00AD}' | '\u{2010}'..='\u{2015}' | '\u{2212}')
}

/// The text with every year and every bracketed type taken out.
///
/// A year is four digits from 1900 to 2099, and a type is one of the words the
/// parser reads as one. A type counts only inside brackets, here as in the
/// parser: `Show (Movie)` is a film of `Show`, while a bare `TV` is part of a
/// name. Brackets alone are not enough either, so `[Oshi no Ko]` keeps its name
/// and `(12)` keeps its number. A year counts in brackets and as a word of its
/// own, because the parser reports one of either kind apart from the title.
/// The parser's own range is narrower, and a year outside it is dropped here
/// from a list entry and a filename's title alike, so the two still meet.
fn without_markers(text: &str) -> String {
    without_bare_years(&without_bracketed_markers(text))
}

/// The text with every year standing as a word of its own taken out.
///
/// Digits joined to anything else are part of the name, as they are to the
/// parser, which reads a year only where the whole token is one: `Show/1999`
/// and `Maji LOVE 2000%` keep their digits.
fn without_bare_years(text: &str) -> String {
    let mut kept = text.to_owned();
    // From the end, so that a word's range still lines up with `kept` once the
    // years behind it have gone.
    for word in words(text).into_iter().rev() {
        let before = text[..word.start].chars().next_back();
        let after = text[word.end..].chars().next();
        let stands_alone = before.is_none_or(is_token_break) && after.is_none_or(is_token_break);
        if stands_alone && is_year(&text[word.start..word.end]) {
            kept.replace_range(word, " ");
        }
    }
    kept
}

/// What ends a token for the parser: a bracket, or one of its separators.
fn is_token_break(c: char) -> bool {
    is_word_break(c)
        || is_bracket(c)
        || is_dash(c)
        || matches!(c, '.' | ',' | '&' | '~' | '+' | '|' | ':')
}

/// The text with every bracket that holds only a year or a type taken out.
fn without_bracketed_markers(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let inside = &rest[c.len_utf8()..];
        match BRACKETS.iter().find(|&&(open, _)| open == c) {
            Some(&(_, close)) if let Some(after) = marker_then(inside, close) => {
                kept.push(' ');
                rest = after;
            }
            _ => {
                kept.push(c);
                rest = inside;
            }
        }
    }
    kept
}

/// What follows a year or a type and its closing bracket, where `text` is what
/// stands after the opening bracket.
///
/// The search for `close` runs to the end of the text, which is safe because a
/// year and a type hold no bracket of their own: where the marker matches, the
/// `close` found is the one that closes the bracket this was given.
fn marker_then(text: &str, close: char) -> Option<&str> {
    let (marker, after) = text.trim_start().split_once(close)?;
    let marker = marker.trim_end();
    (is_year(marker) || is_type(marker)).then_some(after)
}

/// Four digits from 1900 to 2099.
fn is_year(word: &str) -> bool {
    word.len() == 4
        && word.bytes().all(|b| b.is_ascii_digit())
        && (word.starts_with("19") || word.starts_with("20"))
}

/// A word the parser reads as the type of a release rather than as a name.
fn is_type(word: &str) -> bool {
    const TYPES: [&str; 10] = [
        "TV",
        "Movie",
        "Gekijouban",
        "OAD",
        "OAV",
        "OVA",
        "ONA",
        "SP",
        "Special",
        "Specials",
    ];

    TYPES.iter().any(|kind| kind.eq_ignore_ascii_case(word))
}

/// A number the text spells, and the bytes that spell it.
struct Found {
    number: u32,
    spelled: Range<usize>,
    /// Whether the word that marks it has other meanings, as `Part` has.
    ambiguous: bool,
}

/// Where a season or a part is spelled at one word of the text, if it is.
type Reader = fn(&str, &[Range<usize>], usize) -> Option<Found>;

/// Takes a number `read` finds out of the text and says which it was.
///
/// The first one whose marker means nothing else or stands inside brackets.
/// Where every marker is a bare `Part` or `Pt`, the first one found.
fn take(text: &mut String, read: Reader) -> Option<u32> {
    let words = words(text);
    let found: Vec<Found> = (0..words.len())
        .filter_map(|at| read(text, &words, at))
        .collect();
    let beyond_mistaking =
        |candidate: &&Found| !candidate.ambiguous || inside_brackets(text, candidate.spelled.start);
    let chosen = found.iter().find(beyond_mistaking).or(found.first())?;
    text.replace_range(chosen.spelled.clone(), " ");
    Some(chosen.number)
}

/// Whether more brackets open than close before this point in the text.
fn inside_brackets(text: &str, at: usize) -> bool {
    let depth = text[..at].chars().fold(0i32, |depth, c| {
        if BRACKETS.iter().any(|&(open, _)| c == open) {
            depth + 1
        } else if BRACKETS.iter().any(|&(_, close)| c == close) {
            depth - 1
        } else {
            depth
        }
    });
    depth > 0
}

/// Where each run of letters and digits in the text lies.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut start = None;
    for (at, c) in text.char_indices() {
        match (is_letter(c), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                words.push(from..at);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        words.push(from..text.len());
    }
    words
}

fn part_at(text: &str, words: &[Range<usize>], at: usize) -> Option<Found> {
    ["part", "parte", "partie", "pt"]
        .into_iter()
        .find_map(|marker| marker_and_number(text, words, at, marker, part_number))
        .or_else(|| marker_and_number(text, words, at, "cour", short_number))
        .or_else(|| {
            ["cour", "part", "parte", "partie"]
                .into_iter()
                .find_map(|marker| ordinal_and_marker(text, words, at, marker))
        })
        .or_else(|| {
            one_word(text, words, at, |word| {
                after_prefix(word, "p").and_then(short_number)
            })
        })
        .or_else(|| one_word(text, words, at, |word| counted(word, "クール")))
}

fn season_at(text: &str, words: &[Range<usize>], at: usize) -> Option<Found> {
    ["season", "saison"]
        .into_iter()
        .find_map(|marker| {
            marker_and_number(text, words, at, marker, season_number)
                .or_else(|| ordinal_and_marker(text, words, at, marker))
        })
        .or_else(|| {
            one_word(text, words, at, |word| {
                after_prefix(word, "s").and_then(short_number)
            })
        })
        .or_else(|| one_word(text, words, at, |word| counted(word, "期")))
}

/// A number spelled by one word alone: `S2`, `P2`, `第2期`.
fn one_word(
    text: &str,
    words: &[Range<usize>],
    at: usize,
    number: impl Fn(&str) -> Option<u32>,
) -> Option<Found> {
    Some(Found {
        number: number(&text[words[at].clone()])?,
        spelled: words[at].clone(),
        ambiguous: false,
    })
}

/// A Japanese count: `第2期`, `2期`, `第2クール`.
fn counted(word: &str, counter: &str) -> Option<u32> {
    let word = word.strip_prefix('第').unwrap_or(word);
    word.strip_suffix(counter).and_then(short_number)
}

/// `Season 2`, `Season.2` or `Season2`.
fn marker_and_number(
    text: &str,
    words: &[Range<usize>],
    at: usize,
    marker: &str,
    number: fn(&str) -> Option<u32>,
) -> Option<Found> {
    let joined = after_prefix(&text[words[at].clone()], marker)?;
    if !joined.is_empty() {
        return Some(Found {
            number: short_number(joined)?,
            spelled: words[at].clone(),
            ambiguous: means_something_else(marker),
        });
    }
    let next = next_word(text, words, at)?;
    Some(Found {
        number: number(&text[next.clone()])?,
        spelled: words[at].start..next.end,
        ambiguous: means_something_else(marker),
    })
}

/// `2nd Season` or `Second Season`.
fn ordinal_and_marker(
    text: &str,
    words: &[Range<usize>],
    at: usize,
    marker: &str,
) -> Option<Found> {
    let number = ordinal(&text[words[at].clone()])?;
    let next = next_word(text, words, at)?;
    text[next.clone()]
        .eq_ignore_ascii_case(marker)
        .then(|| Found {
            number,
            spelled: words[at].start..next.end,
            ambiguous: means_something_else(marker),
        })
}

/// Whether a marker word has meanings beyond the one read here.
///
/// `Part` stands for something else in `Extra Part` and `Part-Timer`, and `Pt`
/// opens the language tag `pt-BR`. The parser marks both ambiguous and counts
/// them only inside brackets, while it counts `Cour` and `Parte` anywhere.
fn means_something_else(marker: &str) -> bool {
    matches!(marker, "part" | "pt")
}

/// The word after this one, where nothing but whitespace and dots stands
/// between.
fn next_word<'w>(text: &str, words: &'w [Range<usize>], at: usize) -> Option<&'w Range<usize>> {
    let next = words.get(at + 1)?;
    text[words[at].end..next.start]
        .chars()
        .all(|c| c.is_whitespace() || c == '.')
        .then_some(next)
}

/// The title's last word, taken off as its season where it is a numeral.
fn take_ending_numeral(text: &mut String) -> Option<u32> {
    let last = words(text).pop()?;
    let number = numeral(&text[last.clone()])?;
    text.replace_range(last, " ");
    Some(number)
}

/// The rest of the word after `prefix`, which is matched ignoring case.
fn after_prefix<'w>(word: &'w str, prefix: &str) -> Option<&'w str> {
    let (head, rest) = word.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// One or two digits.
fn short_number(word: &str) -> Option<u32> {
    let digits = (1..=2).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| word.parse().ok()).flatten()
}

/// A season after the word `Season`: in digits, or a numeral from `II` to `IX`.
fn season_number(word: &str) -> Option<u32> {
    short_number(word).or_else(|| numeral(word))
}

/// The number after `Part`, in digits or in a numeral from `I` to `X`.
///
/// A numeral is safe to read here in a way it is not at the end of a title:
/// after the word `Part` an `I` or a `V` cannot be anything else.
fn part_number(word: &str) -> Option<u32> {
    const NUMERALS: [&str; 10] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"];

    short_number(word).or_else(|| {
        NUMERALS
            .iter()
            .zip(1..)
            .find(|(numeral, _)| numeral.eq_ignore_ascii_case(word))
            .map(|(_, number)| number)
    })
}

/// An uppercase numeral from `II` to `IX`.
fn numeral(word: &str) -> Option<u32> {
    const NUMERALS: [&str; 8] = ["II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];

    NUMERALS
        .iter()
        .zip(2..)
        .find(|(numeral, _)| **numeral == word)
        .map(|(_, number)| number)
}

/// `2nd` or `Second`, up to the ninth in words.
fn ordinal(word: &str) -> Option<u32> {
    const WORDS: [&str; 9] = [
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
    ];

    let spelled = WORDS
        .iter()
        .zip(1..)
        .find(|(ordinal, _)| ordinal.eq_ignore_ascii_case(word))
        .map(|(_, number)| number);
    spelled.or_else(|| {
        let (digits, suffix) = word.split_at_checked(word.len().checked_sub(2)?)?;
        ["st", "nd", "rd", "th"]
            .iter()
            .any(|ending| ending.eq_ignore_ascii_case(suffix))
            .then(|| short_number(digits))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::Key;
    use crate::encoding::Confidence;
    use crate::path::RawPath;
    use crate::recognise::parse::{Episode, Parsed, parse};

    /// The key a list entry spelled this way is filed under.
    fn listed(title: &str) -> Key {
        Key::from_title(title).expect("a title with letters in it has a key")
    }

    /// The key a file of this name is looked up by.
    fn named(name: &str) -> Key {
        Key::from_parsed(&parse(&RawPath::from_bytes(name.as_bytes().to_vec())))
            .expect("the name spells a title")
    }

    #[test]
    fn season_2_and_2nd_season_reach_the_same_key() {
        assert_eq!(
            listed("Show Title Season 2"),
            listed("Show Title 2nd Season")
        );
    }

    #[test]
    fn a_roman_numeral_ending_the_title_is_its_season() {
        assert_eq!(listed("Show Title II"), listed("Show Title Season 2"));
        assert_eq!(listed("Show Title IX"), listed("Show Title Season 9"));
    }

    #[test]
    fn a_season_spelled_as_a_word_is_a_season() {
        assert_eq!(
            listed("Show Title Second Season"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_season_abbreviated_is_a_season() {
        assert_eq!(listed("Show Title S2"), listed("Show Title Season 2"));
        assert_eq!(listed("Show Title Season2"), listed("Show Title Season 2"));
    }

    #[test]
    fn a_season_spelled_in_another_language_is_a_season() {
        assert_eq!(listed("Show Title Saison 2"), listed("Show Title Season 2"));
        assert_eq!(
            listed("Show Title 2nd Saison"),
            listed("Show Title Season 2")
        );
        assert_eq!(
            listed("ショータイトル 第2期"),
            listed("ショータイトル Season 2")
        );
        assert_eq!(
            named("ショータイトル 第2期 - 03.mkv"),
            listed("ショータイトル 第2期")
        );
    }

    #[test]
    fn a_season_in_roman_numerals_after_the_word_is_a_season() {
        assert_eq!(
            listed("Show Title Season II"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_roman_numeral_inside_the_title_is_part_of_its_name() {
        assert_ne!(listed("Show II Title"), listed("Show Title Season 2"));
        assert_ne!(listed("Show II Title"), listed("Show Title"));
    }

    #[test]
    fn x_and_i_ending_a_title_are_letters_and_not_numerals() {
        assert_ne!(listed("Show Title X"), listed("Show Title Season 10"));
        assert_ne!(listed("Show Title I"), listed("Show Title"));
    }

    #[test]
    fn a_numeral_in_mixed_case_is_a_word() {
        assert_ne!(listed("Show Title Ii"), listed("Show Title Season 2"));
    }

    #[test]
    fn a_bare_number_ending_a_title_is_not_a_season() {
        assert_ne!(listed("Show Title 2"), listed("Show Title Season 2"));
        assert_ne!(listed("Show Title 0"), listed("Show Title"));
    }

    #[test]
    fn a_first_season_is_the_title_itself() {
        assert_eq!(listed("Show Title Season 1"), listed("Show Title"));
        assert_eq!(listed("Show Title 1st Season"), listed("Show Title"));
    }

    #[test]
    fn a_part_is_never_a_season() {
        assert_ne!(listed("Show Title Part 2"), listed("Show Title Season 2"));
        assert_ne!(
            named("Show Title Part 2 - 03.mkv"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_part_and_a_cour_are_one_thing() {
        assert_eq!(listed("Show Title Cour 2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title 2nd Cour"), listed("Show Title Part 2"));
    }

    #[test]
    fn a_part_spelled_otherwise_is_the_same_part() {
        assert_eq!(listed("Show Title Part II"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Part.2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Part2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Part ii"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Part X"), listed("Show Title Part 10"));
        assert_eq!(listed("Show Title Part I"), listed("Show Title"));
    }

    #[test]
    fn a_part_spelled_in_another_language_is_a_part() {
        assert_eq!(listed("Show Title Parte 2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Pt 2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title Partie 2"), listed("Show Title Part 2"));
        assert_eq!(listed("Show Title 第2クール"), listed("Show Title Part 2"));
    }

    #[test]
    fn a_part_abbreviated_is_a_part() {
        assert_eq!(
            listed("Show Title S3 P2"),
            listed("Show Title Season 3 Part 2")
        );
    }

    #[test]
    fn a_part_in_brackets_wins_over_a_bare_one() {
        // A bare `Part` beside a bracketed one is part of the name, and the
        // parser reads only the bracketed one, so the list does the same. The
        // brackets count however far inside them the part stands.
        for spelled in [
            "Show Title Part 6 (Part 2)",
            "Show Title Part 6 (Season 3 Part 2)",
        ] {
            assert_eq!(
                listed(spelled),
                named(&format!("{spelled} - 03.mkv")),
                "{spelled}"
            );
        }
        assert_ne!(
            listed("Show Title Part 6 (Part 2)"),
            listed("Show Title Part 6")
        );
    }

    #[test]
    fn a_part_spelled_beyond_mistaking_wins_over_one_in_brackets() {
        // `Cour` can mean nothing else, so it counts outside brackets, and it
        // stands before the bracketed `Part` here. The parser reads the same
        // number from this name. Only the number is compared: the parser takes
        // `(Part 2)` out of the title altogether, where a list entry keeps it.
        assert_eq!(listed("Show Title Cour 3 (Part 2)").part, Some(3));
        assert_eq!(named("Show Title Cour 3 (Part 2) - 03.mkv").part, Some(3));
    }

    #[test]
    fn a_season_and_its_part_are_both_in_the_key() {
        let both = listed("Show Title Season 3 Part 2");

        assert_eq!(both, listed("Show Title 3rd Season Cour 2"));
        assert_ne!(both, listed("Show Title Season 3"));
        assert_ne!(both, listed("Show Title Part 2"));
    }

    #[test]
    fn a_first_part_is_the_title_itself() {
        assert_eq!(listed("Show Title Part 1"), listed("Show Title"));
    }

    #[test]
    fn titles_differing_only_by_case_share_a_key() {
        assert_eq!(listed("SHOW TITLE"), listed("Show Title"));
        assert_eq!(listed("show title"), listed("Show Title"));
        assert_eq!(listed("ШОУ"), listed("шоу"));
    }

    #[test]
    fn punctuation_between_words_is_erased() {
        for spelling in [
            "Show: Title",
            "Show-Title",
            "Show/Title",
            "Show.Title",
            "ShowTitle",
        ] {
            assert_eq!(listed(spelling), listed("Show Title"), "{spelling}");
        }
        assert_eq!(listed("Show Title: 2"), listed("Show Title 2"));
    }

    #[test]
    fn marks_ending_a_title_are_kept() {
        assert_ne!(listed("Show Title!"), listed("Show Title"));
        assert_ne!(listed("Show Title!!"), listed("Show Title!"));
        assert_ne!(listed("Show Title'"), listed("Show Title"));
    }

    #[test]
    fn a_mark_standing_apart_at_the_end_is_still_a_mark() {
        assert_ne!(listed("Show Title ♭"), listed("Show Title"));
    }

    #[test]
    fn marks_left_at_the_end_once_the_season_is_taken_off_are_kept() {
        assert_eq!(
            listed("Show Title!! 2nd Season"),
            listed("Show Title!! Season 2")
        );
        assert_ne!(
            listed("Show Title!! 2nd Season"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_bracket_is_not_a_mark() {
        assert_eq!(listed("【Show Title】"), listed("Show Title"));
        assert_eq!(listed("[Show Title]"), listed("Show Title"));
        assert_eq!(listed("Show Title (Sub)"), listed("Show Title Sub"));
    }

    #[test]
    fn a_dash_ending_a_title_is_not_a_mark() {
        assert_eq!(
            listed("Show Title -Subtitle-"),
            listed("Show Title Subtitle")
        );
        assert_eq!(listed("Show Title - Part 2"), listed("Show Title Part 2"));
    }

    #[test]
    fn a_space_the_parser_reads_between_words_is_not_a_mark() {
        assert_eq!(listed("Show Title_"), listed("Show Title"));
        assert_eq!(listed("Show Title\u{200B}"), listed("Show Title"));
    }

    #[test]
    fn a_combining_mark_belongs_to_its_letter() {
        assert_ne!(listed("ไก่ย่าง"), listed("ไกยาง"));
    }

    #[test]
    fn a_year_in_brackets_of_any_kind_is_not_in_the_key() {
        for spelling in [
            "Show Title (2019)",
            "Show Title (1998)",
            "Show Title [2019]",
            "Show Title {2019}",
            "Show Title <2019>",
            "Show Title 【2019】",
            "Show Title （2019）",
        ] {
            assert_eq!(listed(spelling), listed("Show Title"), "{spelling}");
        }
    }

    #[test]
    fn a_year_before_a_season_is_not_in_the_key_either() {
        assert_eq!(
            listed("Show Title (2024) Season 2"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_year_standing_alone_is_not_in_the_key_either() {
        // The parser reads a bare year out of a filename and reports it apart
        // from the title, so a list entry spelling one has to lose it too.
        assert_eq!(listed("Show Title 2013"), listed("Show Title"));
        assert_eq!(listed("1999 Show Title"), listed("Show Title"));
        assert_eq!(
            named("[Group] Show Title 2013 - 03.mkv"),
            listed("Show Title 2013")
        );
    }

    #[test]
    fn digits_joined_to_the_name_are_part_of_it() {
        // The parser reads a year only where the digits are a word of their
        // own, so `2000%` and `Show/1999` keep theirs. Each is compared with
        // the same spelling less the digits, so that the punctuation left
        // behind cannot be what tells the two keys apart.
        assert_ne!(listed("Show Title 2000%"), listed("Show Title %"));
        assert_ne!(listed("Show/1999"), listed("Show/"));
    }

    #[test]
    fn a_type_in_brackets_is_not_in_the_key() {
        // AniList tells a series from the film of the same name by a type in
        // brackets. A filename's title never carries one, because the parser
        // reports a bracketed type as an element of its own.
        for spelling in [
            "Show Title (TV)",
            "Show Title (Movie)",
            "Show Title (OVA)",
            "Show Title (ONA)",
            "Show Title (OAD)",
            "Show Title (OAV)",
            "Show Title (SP)",
            "Show Title (Special)",
            "Show Title (Specials)",
            "Show Title (Gekijouban)",
            "Show Title（TV）",
        ] {
            assert_eq!(listed(spelling), listed("Show Title"), "{spelling}");
        }
        assert_eq!(
            listed("Show Title [TV] Season 2 Part 2"),
            listed("Show Title Season 2 Part 2")
        );
    }

    #[test]
    fn a_word_in_brackets_that_is_not_a_type_stays_in_the_key() {
        assert_ne!(listed("Show Title (Zero)"), listed("Show Title"));
        assert_ne!(listed("Show Title (Movies)"), listed("Show Title"));
    }

    #[test]
    fn a_type_in_a_filename_is_not_in_the_key() {
        assert_eq!(
            named("[Group] Show Title (TV) - 03.mkv"),
            listed("Show Title (TV)")
        );
    }

    #[test]
    fn a_number_that_is_not_a_year_stays() {
        assert_ne!(listed("Show Title (12)"), listed("Show Title"));
        assert_ne!(listed("Show Title (2150)"), listed("Show Title"));
        assert_ne!(listed("Show Title 2150"), listed("Show Title"));
        // Four digits, and no fewer: a year is a year by its length as well.
        assert_ne!(listed("Show Title (20)"), listed("Show Title"));
        assert_ne!(listed("Show Title 20"), listed("Show Title"));
    }

    #[test]
    fn full_width_letters_reach_their_ordinary_forms() {
        assert_eq!(listed("ＳＨＯＷ　ＴＩＴＬＥ"), listed("Show Title"));
        assert_eq!(
            listed("Show Title ２nd Season"),
            listed("Show Title Season 2")
        );
    }

    #[test]
    fn a_numeral_written_as_one_character_is_a_numeral() {
        assert_eq!(listed("Show Title Ⅱ"), listed("Show Title Season 2"));
    }

    #[test]
    fn a_multiplication_sign_between_words_is_the_letter_x() {
        assert_eq!(listed("Show×Title"), listed("Show x Title"));
    }

    #[test]
    fn titles_sharing_a_prefix_do_not_share_a_key() {
        assert_ne!(listed("Show Title: Zero"), listed("Show Title: Apocrypha"));
        assert_ne!(listed("Show Title: Zero"), listed("Show Title"));
    }

    #[test]
    fn titles_one_letter_apart_do_not_share_a_key() {
        assert_ne!(listed("Show Title"), listed("Show Titles"));
        assert_ne!(listed("Show Title"), listed("Snow Title"));
    }

    #[test]
    fn a_title_with_no_letters_has_no_key() {
        for spelling in ["", "!!!", "Season 2", "(2019)"] {
            assert_eq!(Key::from_title(spelling), None, "{spelling:?}");
        }
    }

    #[test]
    fn a_name_that_spells_no_title_has_no_key() {
        let nothing = Parsed {
            title: None,
            episode: Episode::Only(3),
            season: Some(2),
            part: None,
            year: None,
            release_group: Some("Group".to_owned()),
            confidence: Confidence::Certain,
        };

        assert_eq!(Key::from_parsed(&nothing), None);
    }

    #[test]
    fn a_release_group_is_not_in_the_key() {
        assert_eq!(
            named("[GroupA] Show Title - 03.mkv"),
            named("[GroupB] Show Title - 03.mkv")
        );
        assert_eq!(named("[GroupA] Show Title - 03.mkv"), listed("Show Title"));
    }

    #[test]
    fn a_season_and_part_the_parser_reports_are_in_the_key() {
        assert_eq!(
            named("[Group] Show Title S2 - 03 [1080p].mkv"),
            listed("Show Title 2nd Season")
        );
        assert_eq!(
            named("Show Title Season 3 Part 2 - 03.mkv"),
            listed("Show Title Season 3 Part 2")
        );
    }

    #[test]
    fn a_season_and_part_the_parser_leaves_in_the_title_are_in_the_key() {
        assert_eq!(
            named("Show Title II - 03.mkv"),
            listed("Show Title Season 2")
        );
        assert_eq!(
            named("Show Title Part 2 - 03.mkv"),
            listed("Show Title Part 2")
        );
    }

    #[test]
    fn an_episode_title_after_a_season_and_episode_is_not_in_the_key() {
        // Season and episode as `1x05`, then the episode's own name ending in a
        // numeral, a year and a group. The parser reads `1x05` as season one,
        // episode five, and ends the title there, so the `II` of the episode's
        // name never reaches the key as a second season.
        let name = "Show Title - 1x05 - The Episode Name II [1994] [Group].mkv";

        assert_eq!(named(name), listed("Show Title"));
    }

    #[test]
    fn a_numeral_stays_in_the_name_where_the_parser_reported_the_season() {
        // The list reads `Season 2` first and keeps the numeral as part of the
        // name. The parser takes `Season 2` out of a filename, so the numeral
        // left ending its title is part of the name there too.
        assert_eq!(
            named("Show Title II Season 2 - 03.mkv"),
            listed("Show Title II Season 2")
        );
        assert_ne!(
            named("Show Title II Season 2 - 03.mkv"),
            listed("Show Title II")
        );

        let reported = Parsed {
            title: Some("Show Title II".to_owned()),
            episode: Episode::Only(3),
            season: Some(3),
            part: None,
            year: None,
            release_group: None,
            confidence: Confidence::Certain,
        };
        assert_eq!(
            Key::from_parsed(&reported),
            Some(listed("Show Title II Season 3"))
        );
    }

    #[test]
    fn a_mark_the_parser_cut_off_a_filename_is_in_the_key() {
        assert_eq!(
            named("[Group] Show Title: - 03 [1080p].mkv"),
            listed("Show Title:")
        );
        assert_ne!(
            named("[Group] Show Title: - 03 [1080p].mkv"),
            listed("Show Title")
        );
    }
}
