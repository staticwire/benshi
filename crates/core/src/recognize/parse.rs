//! What a filename spells, before anything is matched against it.
//!
//! One function, and everything the parser reports that this crate does not
//! read is dropped here rather than further down. **Which kinds are dropped is
//! written out rather than caught by a wildcard**, so a kind added upstream
//! stops the build and has to be given an answer instead of changing behavior
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
//!
//! **Japanese title quotes are read as spaces.** `「」` and `『』` set off a
//! title or a subtitle in Japanese, and the parser counts them among its
//! brackets: `作品名『副題』` would arrive as the title `作品名` with
//! the subtitle reported as a group, and a file of a special would reach the
//! series it belongs to. Read as spaces, the subtitle stays in the title, and
//! the title meets the entry that spells it.

use std::ops::Range;

use anitomy_ng::{Element, ElementKind, Options};
use serde::{Deserialize, Serialize};

use crate::encoding::{Confidence, decode};
use crate::path::RawPath;
use crate::recognize::normalize::part_in;

/// What a filename says about which episode it holds.
///
/// Four cases and not an [`Option`], because a name spelling several episodes
/// and a name spelling none are different facts that lead different places. A
/// batch is a thing this program cannot record progress for; a film is a thing
/// it can. Collapsing them would leave an explanation with nothing to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// it is neither rounded into a neighbor it is not nor called absent.
    ///
    /// **Apart from [`Absent`](Episode::Absent) for the reason [`Several`](Episode::Several) is
    /// apart from it.** A film spells no episode and is a work this program can
    /// record as watched; a half episode is not. A consumer reading the two
    /// alike would mark a whole series watched because somebody played a recap.
    NotWhole,
    /// None spelled at all: a film, or a name that does not say. A name
    /// spelling zero for the season and for the episode at once is read as
    /// spelling none.
    Absent,
}

/// What a name says the release is, where it spells a type word.
///
/// The word stays out of the title where the title ends before it, so
/// `Show Title Season 3 OVA - 01` reports the title and the season of the
/// series it extends. Without the word kept here, an episode of an OVA reads
/// as an episode of the series. `TV` says nothing a name without it does not,
/// and is not kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    /// A film: `Movie`, `Gekijouban`.
    Film,
    /// A release apart from the broadcast: `OVA`, `OAD`, `OAV`, `ONA`, `SP`,
    /// `Special`, `Specials`. One kind, because a service lists such a
    /// release as an OVA where another lists it as a special.
    Extra,
}

/// The kind a type word names, and none for `TV` and for a word of no type.
pub(crate) fn kind_of(word: &str) -> Option<Kind> {
    const FILMS: [&str; 2] = ["Movie", "Gekijouban"];
    const EXTRAS: [&str; 7] = ["OVA", "OAD", "OAV", "ONA", "SP", "Special", "Specials"];

    let named = |words: &[&str]| words.iter().any(|w| w.eq_ignore_ascii_case(word));
    if named(&FILMS) {
        Some(Kind::Film)
    } else if named(&EXTRAS) {
        Some(Kind::Extra)
    } else {
        None
    }
}

/// What a filename spells.
///
/// What [`parse`] reads from a name for the stages below, and nothing is
/// matched yet. Every field [`parse`] fills is read from the **name**, never
/// from what a corpus holds. [`read`](super::read) answers with another
/// reading where an assignment or the list holds it, and
/// [`read_file`](super::read_file) with one read from a directory above the
/// file, where [`from_directory`](Parsed::from_directory) says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parsed {
    /// The title as the name spelled it, where the parser found one, or as a
    /// directory above the file spelled it where
    /// [`from_directory`](Parsed::from_directory) says so.
    pub title: Option<String>,
    /// The episode, or the reason there is not exactly one.
    pub episode: Episode,
    /// The season, where the name spelled one in a form the parser knows.
    ///
    /// `Season 2`, `2nd Season` and `Season II` all arrive here as two. A
    /// numeral ending the title does not: `Show Title II` comes back with the
    /// numeral still in the title and no season at all, which is why the
    /// normalizer carries a rule of its own for it. Season zero beside episode
    /// zero is not kept.
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
    /// What the name says the release is, where it spells a type word other
    /// than `TV`.
    pub kind: Option<Kind>,
    /// The title read on through the episode number and the words after it.
    ///
    /// The parser reads `Show Title 13.mkv` as the title `Show Title` and
    /// episode 13, and the words after such a number as the episode's title.
    /// Where the number belongs to the title, this is the title the name
    /// spells: `Show Title 13`, or `Show Title 13 - The Subtitle` of `Show
    /// Title 13 - The Subtitle [1080p].mkv`. The list decides which reading
    /// holds, so this one is kept beside the first.
    ///
    /// Nothing where anything but spaces and underscores parts the title from
    /// the number, or where the name spells anything but one episode in digits
    /// alone. Nothing either where the parser left a letter or a digit after
    /// the number unread, as the `v` of `Show Title 13v2` or the subtitle of
    /// `Show Title 13 (1983) - The Subtitle`. A version mark and a subtitle
    /// are refused alike, and a title read on without the subtitle would drop
    /// what tells one work from another.
    pub title_with_number: Option<String>,
    /// The episode a name begins with and the words after it, as
    /// `(1, "Quiet Harbor")` of `01 Quiet Harbor.mkv` and of
    /// `01 - Quiet Harbor.mkv`.
    ///
    /// The words are the show's title in `01 Show Title.mkv` and an episode's
    /// in `01 Quiet Harbor.mkv`, and a number beginning a title is the title's
    /// own in `12 Quiet Lanterns.mkv`. The parser cannot tell these apart, so
    /// this reading is kept beside its own, and the list decides. A year the
    /// parser reads as one, as in `2001 Quiet Harbor.mkv`, is no episode.
    ///
    /// Where the parser took the number into the title, a number of one digit
    /// is not read as an episode. A title begins with one, as in
    /// `7 Lanterns per Night.mkv`, and a name read episode first can be read
    /// against its directory, where such a film would be taken for an episode
    /// of the show it is kept beside. Nothing either where the parser left a
    /// letter or a digit after the title or the episode unread, as
    /// [`title_with_number`](Parsed::title_with_number) says.
    pub episode_first: Option<(u32, String)>,
    /// The title was read from a directory above the file.
    ///
    /// The episode and the release group are what the file's name spells,
    /// and so are the season, the part, the year and the type wherever it
    /// spells one. The confidence and the text are the directory's. Set only
    /// by [`read_file`](super::read_file); [`parse`] reads the name alone.
    pub from_directory: bool,
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
    /// recognizes, so a name spelling `Show Title - The Subtitle - 03` reports
    /// a title of `Show Title` and the subtitle belongs to no element at all.
    /// A stage reading the title alone cannot tell such a file from the series
    /// it extends. What is struck out is the release group, the episode, the
    /// season, the part and the volume with the words that mark them, the
    /// year, the resolution, the source, the extension and the rest of the
    /// release's own vocabulary.
    ///
    /// A space is left wherever something was struck out, so that the words on
    /// either side of it stay two words. The directory's text where
    /// [`from_directory`](Parsed::from_directory) says so.
    pub text: String,
}

/// The quotes Japanese sets a title or a subtitle off with, which are read as
/// spaces.
const TITLE_QUOTES: [char; 4] = ['「', '」', '『', '』'];

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
        kind: None,
        title_with_number: None,
        episode_first: None,
        from_directory: false,
        release_group: None,
        confidence: decoded.confidence,
        text: String::new(),
    };
    let mut episodes = Vec::new();
    let text = decoded.text.replace(TITLE_QUOTES, " ");
    let elements = anitomy_ng::parse(&text, Options::default());
    let lost_part = part_lost_after_season(&text, &elements);
    let cut_marks = marks_cut_off_the_title(&text, &elements);
    parsed.text = without_the_release(&text, &elements);
    parsed.title_with_number = title_read_on(&text, &elements);
    parsed.episode_first = episode_first(&text, &elements);
    parsed.kind = release_kind(&text, &elements);

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

            // Read by `release_kind` above, where it stands.
            | ElementKind::Type

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
    // Zero for the season and for the episode at once counts nothing, as
    // in a film named with `s00e00`, and the name is read as spelling
    // neither. Either zero alone is kept: episode zero can number a prologue,
    // and an episode in season zero a special.
    if parsed.season == Some(0) && parsed.episode == Episode::Only(0) {
        parsed.season = None;
        parsed.episode = Episode::Absent;
    }
    parsed.part = parsed.part.or(lost_part);
    if let Some(title) = &mut parsed.title {
        title.push_str(&cut_marks);
    }
    parsed
}

/// The title read on through the episode number and the words after it, as
/// [`Parsed::title_with_number`] says.
fn title_read_on(text: &str, elements: &[Element]) -> Option<String> {
    let title = elements
        .iter()
        .find(|element| element.kind == ElementKind::Title)?;
    let mut episodes = elements
        .iter()
        .filter(|element| element.kind == ElementKind::Episode);
    let episode = episodes.next()?;
    if episodes.next().is_some() || !episode.value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let chars: Vec<char> = text.chars().collect();
    if !chars
        .get(span(title).end..episode.position)?
        .iter()
        .all(|&c| c == ' ' || c == '_')
    {
        return None;
    }
    if unread_after(&chars, elements, span(episode).end)? {
        return None;
    }

    // The parser reports in order of position, so the first element after the
    // number is the one that follows it.
    let upto = elements
        .iter()
        .find(|element| element.position > episode.position)
        .filter(|element| element.kind == ElementKind::EpisodeTitle)
        .map_or(span(episode).end, |subtitle| span(subtitle).end);
    Some(
        chars
            .get(title.position..upto)?
            .iter()
            .map(|&c| if c == '_' { ' ' } else { c })
            .collect(),
    )
}

/// The episode a name begins with and the words after it, as
/// [`Parsed::episode_first`] says.
fn episode_first(text: &str, elements: &[Element]) -> Option<(u32, String)> {
    let mut titles = elements
        .iter()
        .filter(|element| element.kind == ElementKind::Title);
    let mut episodes = elements
        .iter()
        .filter(|element| element.kind == ElementKind::Episode);

    let chars: Vec<char> = text.chars().collect();

    match (titles.next(), episodes.next(), episodes.next()) {
        // `01 Quiet Harbor`: the parser took the number into the title.
        (Some(title), None, None) => {
            let (number, words) = title.value.split_once(' ')?;
            if number.len() < 2 || unread_after(&chars, elements, span(title).end)? {
                return None;
            }
            Some((number.parse().ok()?, words.trim().to_owned()))
        }
        // `01 - Quiet Harbor`: the parser read the number, and the words
        // after it as the episode's title.
        (None, Some(episode), None) => {
            let number = episode.value.parse().ok()?;
            let words = elements
                .iter()
                .find(|element| element.kind == ElementKind::EpisodeTitle)?;
            if unread_after(&chars, elements, span(episode).end)? {
                return None;
            }
            Some((number, words.value.clone()))
        }
        _ => None,
    }
}

/// Where in the text an element stands, in characters.
fn span(element: &Element) -> Range<usize> {
    element.position..element.position + element.value.chars().count()
}

/// Whether a letter or a digit after this place in the text is one no
/// element reports, or nothing where an element does not fit the text.
///
/// The parser drops what it cannot place: in `Show Title 13 (1983) - The
/// Subtitle` it reports the year and nothing after it.
fn unread_after(chars: &[char], elements: &[Element], from: usize) -> Option<bool> {
    let mut reported = vec![false; chars.len()];
    for element in elements {
        reported.get_mut(span(element))?.fill(true);
    }

    Some(
        chars
            .iter()
            .zip(&reported)
            .skip(from)
            .any(|(c, &reported)| c.is_alphanumeric() && !reported),
    )
}

/// The kind of the release, from the first type word the parser reports
/// where it names the release rather than standing as a word of a title.
///
/// A type word names the release in brackets, as in `Show Title (OVA) - 01`
/// and `Show Title - 01 [OVA]`, and before the episode where it ends the
/// title or stands after it, as in `Show Title OVA - 01` and `Show Title
/// Season 3 OVA - 01`. `Gekijouban` names a film from in front of its title,
/// as in `Gekijouban Show Title`. Anywhere else the word is a title's: the
/// show's own, as in `Special Title - 03`, or an episode's, as in `Show
/// Title - 07 - Special Training`. The parser reports a type there all the
/// same.
fn release_kind(text: &str, elements: &[Element]) -> Option<Kind> {
    let chars: Vec<char> = text.chars().collect();
    let title = elements
        .iter()
        .find(|element| element.kind == ElementKind::Title)
        .map(span);
    let episode = elements
        .iter()
        .find(|element| element.kind == ElementKind::Episode)
        .map(|element| element.position);

    elements
        .iter()
        .filter(|element| element.kind == ElementKind::Type)
        .filter(|element| {
            let word = span(element);
            let bracketed = word
                .start
                .checked_sub(1)
                .and_then(|before| chars.get(before))
                .is_some_and(|c| "([{【".contains(*c));
            let before_episode = episode.is_none_or(|episode| word.start < episode);
            let placed = title.as_ref().is_none_or(|title| {
                word.start >= title.end
                    || word.end == title.end
                    || (word.start == title.start
                        && element.value.eq_ignore_ascii_case("Gekijouban"))
            });
            bracketed || (before_episode && placed)
        })
        .find_map(|element| kind_of(&element.value))
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

/// The name with what the release itself spells struck out.
///
/// Kept: the title, an episode title, and anything the parser could not place,
/// which is where a subtitle beyond the end of the title ends up. Struck out:
/// what an element of any other kind spells and every word it touches, because
/// a list entry never spells a resolution, a group or an episode number, and
/// counting them would tell against every candidate alike. That takes in
/// `ElementKind::Other`: a word from the parser's keyword list, such as
/// `Uncensored`, or a broadcast marker, such as `7月新番`. A list entry can
/// spell such a keyword, as `Kite Remastered` does, and its key and its words
/// are read without it too.
///
/// **A number goes with what marks it.** The parser reports a season, an
/// episode, a volume and a part as the number alone: `S01E03` comes back as
/// `01` and `03`, and `Season II` as `2`, one character long. Each of its
/// patterns matches a word whole, so striking whole words takes the `S` and
/// the `E`, the `x` of `1x03`, the `nd` of `2nd` and both letters of `II`. A
/// word the parser lists as marking a number goes too, where it stands before
/// a number of its own kind with nothing but delimiters between, as in
/// `Episode 03` and `Vol.2`. A season's word and a part's also go after one,
/// as in `2nd Season` and `2nd Cour`. Such a word standing apart in the title
/// stays: in `New Season S01E03` the season is reported, but not through the
/// `Season` before it.
///
/// What goes is replaced by a space rather than removed, so that two words
/// which were apart stay apart. Positions come from the parser and are counted
/// in `char`s.
fn without_the_release(text: &str, elements: &[Element]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let words = words_of(&chars);
    let mut struck = vec![false; chars.len()];
    let mut carries: Vec<Vec<ElementKind>> = vec![Vec::new(); words.len()];
    let mut named = vec![false; words.len()];
    for element in elements {
        let from = element.position.min(chars.len());
        let upto = (from + element.value.chars().count()).min(chars.len());
        let touched = words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.start < upto && from < word.end);
        match element.kind {
            ElementKind::Title | ElementKind::EpisodeTitle => {
                for (at, _) in touched {
                    named[at] = true;
                }
            }
            kind => {
                struck[from..upto].fill(true);
                for (at, word) in touched {
                    struck[word.clone()].fill(true);
                    carries[at].push(kind);
                }
            }
        }
    }

    let only_delimiters = |one: &Range<usize>, other: &Range<usize>| {
        chars[one.end..other.start].iter().all(|&c| is_delimiter(c))
    };
    for (at, word) in words.iter().enumerate() {
        let Some(kind) = number_marked_by(&chars[word.clone()]) else {
            continue;
        };
        let before_one = words
            .get(at + 1)
            .is_some_and(|next| carries[at + 1].contains(&kind) && only_delimiters(word, next));
        let after_one = matches!(kind, ElementKind::Season | ElementKind::Part)
            && at.checked_sub(1).is_some_and(|before| {
                carries[before].contains(&kind) && only_delimiters(&words[before], word)
            });
        if !named[at] && (before_one || after_one) {
            struck[word.clone()].fill(true);
        }
    }

    chars
        .iter()
        .zip(&struck)
        .map(|(&c, &gone)| if gone { ' ' } else { c })
        .collect()
}

/// Where each run of letters and digits in the name lies, counted in `char`s.
fn words_of(chars: &[char]) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    let mut start = None;
    for (at, c) in chars.iter().enumerate() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                words.push(from..at);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        words.push(from..chars.len());
    }
    words
}

/// The kind of number the parser reads this word as marking, where it reads
/// it as marking one.
///
/// The words `anitomy-ng` lists for an episode, a season, a volume and a part,
/// matched ignoring ASCII case as it matches them.
fn number_marked_by(word: &[char]) -> Option<ElementKind> {
    const MARKERS: [(&str, ElementKind); 16] = [
        ("Ep", ElementKind::Episode),
        ("Eps", ElementKind::Episode),
        ("Episode", ElementKind::Episode),
        ("Episodes", ElementKind::Episode),
        ("Episodio", ElementKind::Episode),
        ("Episódio", ElementKind::Episode),
        ("Capitulo", ElementKind::Episode),
        ("Folge", ElementKind::Episode),
        ("Season", ElementKind::Season),
        ("Saison", ElementKind::Season),
        ("Vol", ElementKind::Volume),
        ("Volume", ElementKind::Volume),
        ("Cour", ElementKind::Part),
        ("Part", ElementKind::Part),
        ("Parte", ElementKind::Part),
        ("Pt", ElementKind::Part),
    ];

    let word: String = word.iter().collect();
    MARKERS
        .iter()
        .find(|(marker, _)| marker.eq_ignore_ascii_case(&word))
        .map(|&(_, kind)| kind)
}

/// What may stand between a word that marks a number and the number: any
/// whitespace, an underscore, and the separators the parser trims off the end
/// of a title.
///
/// The parser's own delimiters are these with `~` added, and with five spaces
/// in place of any whitespace.
fn is_delimiter(c: char) -> bool {
    is_trimmed_separator(c) || c == '_' || c.is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::{Episode, Kind, Parsed, parse};
    use crate::encoding::Confidence;
    use crate::path::RawPath;

    /// Parse a name given as text, which is what every fixture here is.
    fn spelled(name: &str) -> Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    /// The words a stage comparing text finds in what the parse left of a
    /// name.
    fn words_left(name: &str) -> Vec<String> {
        spelled(name)
            .text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_release_is_struck_out_of_the_text_a_stage_compares() {
        // A title ends at the first thing the parser recognizes, so a subtitle
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
    fn the_letters_beside_a_season_and_an_episode_go_with_their_numbers() {
        // A letter left behind is a word the entry does not spell, and it
        // counts against every candidate alike.
        for name in [
            "Alpha Beta S01E03.mkv",
            "Alpha.Beta.S01E03.1080p.WEB.mkv",
            "Alpha Beta 1x03.mkv",
            "Alpha Beta S01E03",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn the_letters_beside_an_episode_go_with_its_number() {
        for name in [
            "Alpha Beta E03.mkv",
            "Alpha Beta EP03.mkv",
            "Alpha Beta - 03v2.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn the_word_before_an_episode_goes_with_its_number() {
        for name in [
            "Alpha Beta Episode 03.mkv",
            "Alpha Beta Ep 03.mkv",
            "Alpha_Beta_Episode_03.mkv",
            "Alpha Beta Episode - 03.mkv",
            "Alpha Beta EPISODE 03.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn the_word_beside_a_season_goes_with_its_number() {
        for name in [
            "Alpha Beta Season 2 - 03.mkv",
            "Alpha Beta 2nd Season - 03.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn a_season_read_from_a_word_goes_whole() {
        // The parser reports the number it read, so `II` and `Second` come
        // back as `2`, one character long.
        for name in [
            "Alpha Beta Season II - 03.mkv",
            "Alpha Beta Second Season - 03.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn a_season_counted_in_japanese_goes_whole() {
        assert_eq!(words_left("Alpha Beta 第2期 - 03.mkv"), ["Alpha", "Beta"]);
    }

    #[test]
    fn the_word_before_a_volume_goes_with_its_number() {
        for name in [
            "Alpha Beta Vol.2 - 03.mkv",
            "Alpha Beta Volume 2 - 03.mkv",
            "Alpha Beta Vol - 03.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn the_word_beside_a_part_goes_with_its_number() {
        for name in ["Alpha Beta Cour 2 - 03.mkv", "Alpha Beta 2nd Cour - 03.mkv"] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn a_marker_after_an_episode_stays() {
        // The parser reads a marker of an episode before its number, and a
        // season's or a part's after one too, as in `2nd Season` and
        // `2nd Cour`.
        assert_eq!(
            words_left("Alpha Beta - 03 - Episode of the Sea.mkv"),
            ["Alpha", "Beta", "Episode", "of", "the", "Sea"]
        );
    }

    #[test]
    fn a_marker_before_a_number_of_another_kind_stays() {
        assert_eq!(
            words_left("Alpha Beta Episode 2nd Season - 03.mkv"),
            ["Alpha", "Beta", "Episode"]
        );
    }

    #[test]
    fn a_marker_set_apart_from_its_number_by_a_bracket_stays() {
        assert_eq!(
            words_left("Alpha Beta (Episode) 03.mkv"),
            ["Alpha", "Beta", "Episode"]
        );
        assert_eq!(
            words_left("Alpha Beta S02E03 (Season).mkv"),
            ["Alpha", "Beta", "Season"]
        );
    }

    #[test]
    fn a_marker_the_parser_reads_as_part_of_the_title_stays() {
        // `S01E03` reports a season, but not through the `Season` before it,
        // so the parser keeps that word in the title and the key holds it.
        let parsed = spelled("Alpha Beta New Season S01E03.mkv");

        assert_eq!(parsed.title.as_deref(), Some("Alpha Beta New Season"));
        assert_eq!(
            words_left("Alpha Beta New Season S01E03.mkv"),
            ["Alpha", "Beta", "New", "Season"]
        );
    }

    #[test]
    fn what_the_parser_reports_as_other_goes() {
        // The parser's own words for an edit of the release, and the mark of
        // a broadcast season that some groups put where a title stands.
        for name in [
            "Alpha Beta - 03 [Uncensored].mkv",
            "Alpha Beta Uncut - 03.mkv",
            "Alpha Beta - 03 (Remastered).mkv",
            "[7月新番] Alpha Beta - 03.mkv",
        ] {
            assert_eq!(words_left(name), ["Alpha", "Beta"], "{name}");
        }
    }

    #[test]
    fn what_the_release_spells_goes_even_where_it_is_no_letter() {
        // A mark that is no letter here can become one once the text is
        // brought to NFKC, as `™` becomes `TM`.
        let parsed = spelled("[Group™] Alpha Beta - 03.mkv");

        assert_eq!(parsed.release_group.as_deref(), Some("Group™"));
        assert!(!parsed.text.contains('™'), "{:?}", parsed.text);
    }

    #[test]
    fn a_marker_apart_from_a_number_stays() {
        assert_eq!(
            words_left("Alpha Beta - 03 - The Episode Name.mkv"),
            ["Alpha", "Beta", "The", "Episode", "Name"]
        );
    }

    #[test]
    fn every_word_the_parser_lists_as_a_marker_goes_with_its_number() {
        let episode = [
            "Ep",
            "Eps",
            "Episode",
            "Episodes",
            "Episodio",
            "Episódio",
            "Capitulo",
            "Folge",
        ];
        let season_or_volume = ["Season", "Saison", "Vol", "Volume"];
        // `Part` and `Pt` count for the parser only inside brackets.
        let part = ["Cour", "Part", "Parte", "Pt"];
        let names = episode
            .iter()
            .map(|marker| format!("Alpha Beta {marker} 03.mkv"))
            .chain(
                season_or_volume
                    .iter()
                    .map(|marker| format!("Alpha Beta {marker} 2 - 03.mkv")),
            )
            .chain(
                part.iter()
                    .map(|marker| format!("Alpha Beta ({marker} 2) - 03.mkv")),
            );

        for name in names {
            assert_eq!(words_left(&name), ["Alpha", "Beta"], "{name}");
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
    fn a_subtitle_in_japanese_title_quotes_stays_in_the_title() {
        // 「」 and 『』 mark a title in Japanese. Read as the brackets of a
        // release group, they would leave the title of the series alone, and a
        // file of a special would reach the series.
        for (name, episode) in [
            ("作品名『副題』.mkv", Episode::Absent),
            ("作品名「副題」 - 03.mkv", Episode::Only(3)),
        ] {
            let parsed = spelled(name);
            let title = parsed.title.clone().unwrap_or_default();

            assert!(
                title.starts_with("作品名") && title.contains("副題"),
                "{name}: {title:?}"
            );
            assert_eq!(parsed.release_group, None, "{name}");
            assert_eq!(parsed.episode, episode, "{name}");
            assert_eq!(words_left(name), ["作品名", "副題"], "{name}");
        }
    }

    #[test]
    fn a_type_word_the_name_spells_is_kept_as_its_kind() {
        for (name, kind) in [
            (
                "[Group] Show Title Season 3 OVA - 01 [1080p].mkv",
                Some(Kind::Extra),
            ),
            ("[Group] Show Title OAD - 02 [1080p].mkv", Some(Kind::Extra)),
            ("Show Title Specials - 02.mkv", Some(Kind::Extra)),
            ("[Group] Show Title Movie [1080p].mkv", Some(Kind::Film)),
            ("[Group] Show Title - 03 [1080p].mkv", None),
            ("[Group] Show Title (TV) - 03 [1080p].mkv", None),
            (
                "[Group] Show Title (OVA) - 01 [1080p].mkv",
                Some(Kind::Extra),
            ),
            ("[Group] Show Title - 01 [OVA].mkv", Some(Kind::Extra)),
            ("Gekijouban Show Title (2019).mkv", Some(Kind::Film)),
        ] {
            assert_eq!(spelled(name).kind, kind, "{name}");
        }
    }

    #[test]
    fn a_type_word_of_a_title_is_no_kind_of_the_release() {
        for name in [
            "[Group] Show Title - 05 - Movie Night [1080p].mkv",
            "Show.Title.S01E05.The.Movie.Night.1080p.WEB.mkv",
            "[Group] Show Title - 07 - Special Training [1080p].mkv",
            "[Group] Show Title - 08 - The OVA Club [1080p].mkv",
            "[Group] Special Title - 03 [1080p].mkv",
            "[Group] Show Title SP Professional - 03 [1080p].mkv",
        ] {
            assert_eq!(spelled(name).kind, None, "{name}");
        }
    }

    #[test]
    fn a_type_word_after_the_season_is_out_of_the_title_and_the_season_stays() {
        let parsed = spelled("[Group] Show Title Season 3 OVA - 01 [1080p].mkv");

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.season, Some(3));
        assert_eq!(parsed.episode, Episode::Only(1));
    }

    #[test]
    fn a_name_that_begins_with_a_number_is_also_read_as_that_episode() {
        for (name, first) in [
            ("01 Quiet Harbor.mkv", Some((1, "Quiet Harbor"))),
            (
                "[Group] 12 Show Title [1080p].mkv",
                Some((12, "Show Title")),
            ),
            ("01 - Show Title.mkv", Some((1, "Show Title"))),
            ("01. Quiet Harbor.mkv", Some((1, "Quiet Harbor"))),
            ("S01E01 Quiet Harbor.mkv", Some((1, "Quiet Harbor"))),
            ("12 Quiet Lanterns.mkv", Some((12, "Quiet Lanterns"))),
            ("7 Lanterns per Night.mkv", None),
            ("01 Quiet Harbor (1983) - The Subtitle.mkv", None),
            ("Show Title - 01.mkv", None),
            ("01.mkv", None),
            ("1100 Quiet Harbor.mkv", Some((1100, "Quiet Harbor"))),
            ("2001 Quiet Harbor.mkv", None),
            ("01 - The Episode Name.mkv", None),
            ("01-02 - Quiet Harbor.mkv", None),
            ("12 Show Title - 05.mkv", None),
        ] {
            assert_eq!(
                spelled(name)
                    .episode_first
                    .as_ref()
                    .map(|(episode, title)| (*episode, title.as_str())),
                first,
                "{name}"
            );
        }
    }

    #[test]
    fn a_number_after_a_space_is_read_on_into_the_title() {
        for (name, read_on) in [
            ("Show Title 13.mkv", Some("Show Title 13")),
            ("[Group] Show Title 13 [1080p].mkv", Some("Show Title 13")),
            ("Show Title 13 (1983).mkv", Some("Show Title 13")),
            (
                "[Group] Show Title 13 - The Subtitle [1080p].mkv",
                Some("Show Title 13 - The Subtitle"),
            ),
            (
                "Show_Title_13_-_The_Subtitle.mkv",
                Some("Show Title 13 - The Subtitle"),
            ),
            ("Show Title - 13.mkv", None),
            ("Show Title-13.mkv", None),
            ("Show Title S01E13.mkv", None),
            ("Show Title.mkv", None),
            ("Show Title 01-12.mkv", None),
            ("Show.Title.13.mkv", None),
            ("Show Title 13 [1080p] 2.mkv", None),
            ("Show Title 05.5.mkv", None),
            ("Show Title 13v2.mkv", None),
            ("Show Title 13 - The Subtitle v2.mkv", None),
            ("Show Title 13 (1983) - The Subtitle.mkv", None),
            ("Show Title 13 - The Episode Name.mkv", None),
        ] {
            assert_eq!(
                spelled(name).title_with_number.as_deref(),
                read_on,
                "{name}"
            );
        }
    }

    #[test]
    fn a_release_group_in_lenticular_brackets_is_still_a_group() {
        let parsed = spelled("【Group】作品名 - 03.mkv");

        assert_eq!(parsed.release_group.as_deref(), Some("Group"));
        assert_eq!(parsed.title.as_deref(), Some("作品名"));
        assert_eq!(parsed.episode, Episode::Only(3));
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
    fn season_zero_and_episode_zero_together_spell_neither() {
        // A film named with `s00e00`. Left as spelled, season zero goes into
        // the key, and the film's entry is filed with no season.
        let parsed = spelled("Show.Title.The.Movie.s00e00.HD1080p.WEBRip.mp4");

        assert_eq!(parsed.title.as_deref(), Some("Show Title The Movie"));
        assert_eq!(parsed.season, None);
        assert_eq!(parsed.episode, Episode::Absent);
    }

    #[test]
    fn an_episode_in_season_zero_keeps_its_season() {
        // `S00E03` numbers a special. With the season taken off, the name
        // would reach the series and name its third episode.
        for (name, episode) in [
            ("Show.Title.S00E03.1080p.WEB.mkv", 3),
            ("[Group] Show Title - S00E01 - A Special [1080p].mkv", 1),
            ("Show Title Season 0 - 01.mkv", 1),
        ] {
            let parsed = spelled(name);

            assert_eq!(parsed.season, Some(0), "{name}");
            assert_eq!(parsed.episode, Episode::Only(episode), "{name}");
        }
    }

    #[test]
    fn season_zero_beside_no_whole_episode_keeps_its_season() {
        // A whole episode numbered zero is all that takes the season with it.
        // Read as spelling no season, a name with no episode beside it would
        // reach the series and name the whole of it.
        for (name, episode) in [
            ("Show Title Season 0 [1080p].mkv", Episode::Absent),
            ("Show Title Season 0 - 01-12.mkv", Episode::Several),
            ("Show Title Season 0 - 05.5.mkv", Episode::NotWhole),
        ] {
            let parsed = spelled(name);

            assert_eq!(parsed.season, Some(0), "{name}");
            assert_eq!(parsed.episode, episode, "{name}");
        }
    }

    #[test]
    fn episode_zero_outside_season_zero_is_an_episode() {
        // A series can number a prologue zero.
        for name in ["Show Title - 00.mkv", "Show.Title.S01E00.1080p.WEB.mkv"] {
            let parsed = spelled(name);

            assert_eq!(parsed.episode, Episode::Only(0), "{name}");
        }
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
        // back whole, with no season at all, so the normalizer reads the
        // numeral out of the title.
        //
        // The test is here rather than beside that rule because this is a fact
        // about the parser. A parser that started reading such a numeral would
        // change what reaches the normalizer from a filename, and this test is
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
        // Rounded into a neighbor it would record progress against an episode
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
        // `Cour 2` is a part the parser recognizes on its own. AniList lists
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
        // meets the parser, so the normalizer has to take a part out of a
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
