//! What a filename turned out to be, and which stage decided it.
//!
//! The pipeline has four stages: a mapping the user made by hand, an exact match
//! on a normalized key, a token index that narrows the corpus to a handful, and
//! a score over what is left. **Only three of them can answer.** The index
//! decides nothing: it hands candidates to the scorer. So three variants of
//! [`Stage`] are stages of the pipeline, and the fourth, [`Stage::Unlisted`],
//! is what the daemon answers for a name the pipeline refused. Why they run in
//! that order is written on [`Stage`].
//!
//! The answer is defined for every stage, so that each one is written against a
//! shape that already has to explain itself. [`by_key`] is the only stage here
//! that computes one.
//!
//! **A stage is named rather than numbered.** The criteria speak of "stage one",
//! and that number is not carried here: inserting a stage renumbers every stage
//! after it, while a name says what decided and does not move.

pub mod altname;
pub mod corpus;
pub mod index;
pub mod normalize;
pub mod parse;
pub mod score;

use crate::ShowId;
use crate::path::RawPath;
use crate::recognize::altname::{Altnames, Assignment};
use crate::recognize::corpus::Corpus;
use crate::recognize::index::Index;
use crate::recognize::normalize::{Key, without_type_words};
use crate::recognize::parse::{Episode, Kind, Parsed, kind_of, parse};
use crate::recognize::score::by_score;

/// What the stages answer, in the order they are asked.
///
/// A name the user assigned by hand wins over a name the program worked out,
/// and it wins where the program would have been right. Why the order is this
/// one is written on [`Stage`].
///
/// Ends in a refusal rather than in a best guess, and the refusal is the
/// scorer's: it says how close the best candidate came, and nothing where there
/// was no candidate to score. Where the name spelled no title at all, the
/// refusal names nothing, because there is nothing it could name, and the
/// index is not asked for candidates it could only find by a title.
#[must_use]
pub fn decide(parsed: &Parsed, altnames: &Altnames, corpus: &Corpus, index: &Index) -> Recognition {
    by_altname(parsed, altnames)
        .map(Recognition::Recognized)
        .or_else(|| by_key(parsed, corpus))
        .unwrap_or_else(|| {
            let candidates = parsed
                .title
                .as_deref()
                .map_or_else(Vec::new, |title| index.candidates(title));
            by_score(parsed, &candidates, corpus)
        })
}

/// Decide a name, and read it another way where no assignment answers the
/// title as parsed.
///
/// `Show Title 13.mkv` reads as the title `Show Title` and episode 13. Where
/// neither an assignment nor the key answers that title, and an assignment
/// or the list holds `Show Title 13`, the name is that work with no episode.
/// The words after the number then belong to the title too, so
/// `Show Title 13 - The Subtitle` reaches only the entry that spells all of
/// it. Only spaces and underscores may part the title from the number, so
/// `Show Title - 13` stays the thirteenth episode whatever the list holds.
/// In the same way `01 Show Title.mkv` is episode one of `Show Title` where
/// the list answers that and not `01 Show Title`.
///
/// Where no other reading is answered either, the name is decided as the
/// parser read it. The other readings are asked of the assignments and the
/// key, and before the scorer is asked of the title as parsed: against a
/// list holding `Show Title 13` alone, the scorer answers `Show Title` as
/// its thirteenth episode.
///
/// Where no assignment answers the title as parsed, the name read whole, as
/// [`Parsed::whole`] keeps it, is asked before the key of that title. The
/// parser reads `Show Title Episode 0: The First Contact [1080p].mkv` as
/// episode 0 of `Show Title`, and the name is the work an assignment or the
/// key of its whole spelling answers, where one does. A spelling with its
/// type words taken out never answers the name read whole. Where the name
/// read whole is the title read on through its number, it is asked as
/// above, after the key. Where its key is that of the title as parsed, it
/// is not asked.
///
/// Answers with the parse the answer was decided by. A parse of the name
/// read whole is asked with no type and answered with the type the name
/// spells, which [`settle`](crate::settle::settle) holds against the format
/// of the entry.
#[must_use]
pub fn read(
    parsed: Parsed,
    altnames: &Altnames,
    corpus: &Corpus,
    index: &Index,
) -> (Parsed, Recognition) {
    let assigned = by_altname(&parsed, altnames).is_some();
    if !assigned
        && let Some(whole) = whole_reading(&parsed)
        && let Some(answer) = by_altname(&whole, altnames)
            .map(Recognition::Recognized)
            .or_else(|| by_key(&whole, corpus))
    {
        return (
            Parsed {
                kind: parsed.kind,
                ..whole
            },
            answer,
        );
    }
    if !assigned && by_key(&parsed, corpus).is_none() {
        for other in other_readings(&parsed) {
            let answer = by_altname(&other, altnames)
                .map(Recognition::Recognized)
                .or_else(|| by_key(&other, corpus));
            if let Some(answer) = answer {
                return (other, answer);
            }
        }
    }
    let answer = decide(&parsed, altnames, corpus, index);
    (parsed, answer)
}

/// The readings a parse keeps beside its own: its title read on through its
/// number, and the episode it begins with as an episode of the show the
/// words after it name.
fn other_readings(parsed: &Parsed) -> impl Iterator<Item = Parsed> {
    let reading = |title: String, episode: Episode| Parsed {
        title: Some(title),
        episode,
        title_with_number: None,
        episode_first: None,
        whole: None,
        ..parsed.clone()
    };
    let read_on = parsed
        .title_with_number
        .clone()
        .map(|title| reading(title, Episode::Absent));
    let first = parsed
        .episode_first
        .clone()
        .map(|(episode, title)| reading(title, Episode::Only(episode)));

    read_on.into_iter().chain(first)
}

/// The name read whole as a parse of its own, where its key is not that of
/// the title as parsed and it is not the title read on through its number.
///
/// It spells no season, no part and no type: they are words of its title.
fn whole_reading(parsed: &Parsed) -> Option<Parsed> {
    let (title, episode) = parsed.whole.clone()?;
    if parsed.title_with_number.as_ref() == Some(&title) {
        return None;
    }
    let whole = Parsed {
        title: Some(title),
        episode,
        season: None,
        part: None,
        kind: None,
        title_with_number: None,
        episode_first: None,
        whole: None,
        ..parsed.clone()
    };

    (Key::from_parsed(&whole) != Key::from_parsed(parsed)).then_some(whole)
}

/// Decide a file by its name, and by the directories above it where the name
/// reaches nothing and may not spell its show: it spells an episode but no
/// title, or begins with its episode as [`Parsed::episode_first`] reads one.
///
/// [`read`] decides the name alone. A directory is not part of what a
/// release spells, and one named after another show would carry that show's
/// words into the name, so a name that spells a title and an episode, as
/// `Show Title - 03.mkv`, is never read against its directory. A name that
/// begins with its episode may not spell its show at all:
/// `01 Quiet Harbor.mkv` and `01 - Quiet Harbor.mkv` are episode one, and
/// the show is in the directory, as `Show Title/01 Quiet Harbor.mkv`.
///
/// The directories are read from the file up. A directory is a part of the
/// show above it where it spells no title, as `Season 2`, a type word alone,
/// as `Specials`, or a number the parser reads as an episode, as `Disc 1`.
/// Such a directory gives the reading the season, the part and the type it
/// spells, where the name and the directories below it spell none. The first
/// directory that names a show is decided with the file's episode, and the
/// walk ends there. A directory the parser reads as a title and a number, as
/// `Show Title 13`, is a show where the list answers it and a part of one
/// where it does not. A path through `..` ends the walk there, because the
/// name of the directory it leads to is not in the path.
///
/// Where that show reaches nothing either, the directory's reading is
/// answered, with its refusal, only where the directory names a release: it
/// spells a season, a year, a release group or a type. A directory of a
/// title alone, as `video`, can hold many shows, so the name is then
/// answered as it spells itself.
#[must_use]
pub fn read_file(
    path: &RawPath,
    altnames: &Altnames,
    corpus: &Corpus,
    index: &Index,
) -> (Parsed, Recognition) {
    let name = parse(&RawPath::from_bytes(path.file_name().to_vec()));
    let episode = match &name.episode_first {
        Some((episode, _)) => Episode::Only(*episode),
        None if name.title.is_none() && name.episode != Episode::Absent => name.episode,
        None => return read(name, altnames, corpus, index),
    };

    let (parsed, answer) = read(name, altnames, corpus, index);
    if !matches!(answer, Recognition::Unrecognized(_)) {
        return (parsed, answer);
    }
    by_directory(path, &parsed, episode, altnames, corpus, index).unwrap_or((parsed, answer))
}

/// The show a directory above the file names, decided with the file's
/// episode, as [`read_file`] says.
fn by_directory(
    path: &RawPath,
    name: &Parsed,
    episode: Episode,
    altnames: &Altnames,
    corpus: &Corpus,
    index: &Index,
) -> Option<(Parsed, Recognition)> {
    let mut season = name.season;
    let mut part = name.part;
    let mut kind = name.kind;
    let mut directory = path.parent();

    while let Some(here) = directory {
        directory = here.parent();
        match here.file_name() {
            b".." => return None,
            b"." => continue,
            _ => {}
        }
        let spelled = parse(&RawPath::from_bytes(here.file_name().to_vec()));
        let type_alone = spelled
            .title
            .as_deref()
            .is_some_and(|title| title.split_whitespace().all(|word| kind_of(word).is_some()));

        if !type_alone && let Some(title) = spelled.title.clone() {
            let as_show = |title: String| Parsed {
                title: Some(title),
                episode,
                season: season.or(spelled.season),
                part: part.or(spelled.part),
                year: name.year.or(spelled.year),
                kind: kind.or(spelled.kind),
                title_with_number: None,
                episode_first: None,
                whole: None,
                from_directory: true,
                release_group: name.release_group.clone(),
                confidence: spelled.confidence,
                text: spelled.text.clone(),
            };
            if spelled.episode == Episode::Absent {
                let show = as_show(title);
                let answer = decide(&show, altnames, corpus, index);
                let names_a_release = spelled.season.is_some()
                    || spelled.year.is_some()
                    || spelled.release_group.is_some()
                    || spelled.kind.is_some();
                return (names_a_release || !matches!(answer, Recognition::Unrecognized(_)))
                    .then_some((show, answer));
            }
            if let Some(numbered) = spelled.title_with_number.clone() {
                let show = as_show(numbered);
                let answer = decide(&show, altnames, corpus, index);
                if !matches!(answer, Recognition::Unrecognized(_)) {
                    return Some((show, answer));
                }
            }
        }

        season = season.or(spelled.season);
        part = part.or(spelled.part);
        kind = kind.or(spelled.kind);
    }

    None
}

/// The entry the user named this file's spelling, where they named one.
///
/// A [`Match`] rather than a [`Recognition`]: this stage cannot be ambiguous,
/// because one key names one entry, and it has no standing to refuse - a
/// refusal is a fact about the corpus, and this stage never looked at one.
///
/// The match carries the assignment it was decided by, so that an explanation
/// shows the decision that was made rather than looking the key up again
/// against assignments that may have changed since.
#[must_use]
pub fn by_altname(parsed: &Parsed, altnames: &Altnames) -> Option<Match> {
    let assignment = altnames.assigned(&Key::from_parsed(parsed)?)?;
    Some(Match {
        show: assignment.show,
        title: assignment.title.clone(),
        episode: parsed.episode,
        stage: Stage::Altname(assignment.clone()),
    })
}

/// What an exact match on the normalized key answers, where it answers at all.
///
/// Nothing where the name reaches no entry: an exact key hits or misses, and a
/// name that misses is what the stages below are for. A refusal is the whole
/// sequence's answer rather than this stage's, so nothing here says a name was
/// not recognized - only that this stage did not recognize it.
///
/// Several entries under one key is an answer, and the answer is that the name
/// does not say. Two list entries share a key honestly - a remake spelled like
/// its original, a season told from the one before it by a mark a filename
/// cannot carry, a series and a special that carry one romaji title - and
/// picking whichever was filed first writes progress against an entry nobody
/// named.
#[must_use]
pub fn by_key(parsed: &Parsed, corpus: &Corpus) -> Option<Recognition> {
    let spelled = parsed.title.as_deref()?;
    let key = Key::from_parsed(parsed)?;
    let mut shows = reached_by(&key, parsed.kind, corpus);
    if shows.is_empty()
        && parsed.kind.is_some()
        && let Some((title, _)) = without_type_words(spelled)
        && let Some(untyped) = Key::from_parsed(&Parsed {
            title: Some(title),
            ..parsed.clone()
        })
    {
        // The parser leaves a type word in a title that no season ends, as
        // in `Show Title OVA - 01`, where a list spells `Show Title (OVA)`.
        shows = reached_by(&untyped, parsed.kind, corpus);
    }
    let candidates: Vec<Candidate> = shows
        .into_iter()
        .map(|show| Candidate {
            show,
            title: corpus.title_of(show).unwrap_or_default().to_owned(),
        })
        .collect();

    match candidates.as_slice() {
        [] => None,
        [one] => Some(Recognition::Recognized(Match {
            show: one.show,
            title: one.title.clone(),
            episode: parsed.episode,
            stage: Stage::Key,
        })),
        _ => Some(Recognition::Ambiguous(Ambiguity {
            parsed: spelled.to_owned(),
            candidates,
        })),
    }
}

/// The entries a key reaches, in the order they were filed, and after them
/// those it reaches through a spelling that held a type word of the kind the
/// name spells, in the order those were filed.
fn reached_by(key: &Key, kind: Option<Kind>, corpus: &Corpus) -> Vec<ShowId> {
    let mut shows = corpus.candidates(key);
    if let Some(kind) = kind {
        for show in corpus.typed_candidates(kind, key) {
            if !shows.contains(&show) {
                shows.push(show);
            }
        }
    }
    shows
}

/// How well a candidate title matched, from nothing to exactly.
///
/// A proportion and nothing else can be one, so it is checked on the way in and
/// private afterwards. **Refused rather than clamped**, for the reason
/// [`WatchedPolicyError`](crate::timeline::WatchedPolicyError) gives: a number
/// quietly moved into range makes an explanation disagree with what actually
/// happened, and the whole purpose of carrying a score is to be believed.
///
/// Not serializable, deliberately. A derived `Deserialize` on a newtype is
/// transparent and would let a message arriving over the socket carry a score
/// of five, which is the kind of hole this crate refuses to open. The wire form
/// belongs with the request that needs one, and it can convert through
/// [`Score::new`] where the check still runs.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Score(f64);

impl Score {
    /// A score, or nothing where the value is not a proportion.
    ///
    /// A value that is not a number is refused along with the rest: it fails the
    /// comparison against both bounds, which is the answer wanted here anyway.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Self(value))
    }

    /// The proportion, for a caller that has to render or compare it.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }
}

/// The stage that produced an answer.
///
/// In the order they are tried, and the order is the point: a manual assignment
/// is the user overriding the program, so it is consulted before any heuristic
/// rather than after everything else has failed.
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    /// A mapping the user made by hand, and which one.
    ///
    /// The assignment travels inside the stage for the reason the score does:
    /// it is what this stage decided by, and a stage that did not consult one
    /// has none to carry. What an explanation shows for it is the text the
    /// user gave and the season and part it was bound to, which is where a
    /// file of another season fails to match it.
    Altname(Assignment),
    /// An exact match on the normalized key.
    Key,
    /// The best candidate the scorer found, and how well it scored.
    ///
    /// The score lives inside the stage rather than beside it, so that a stage
    /// which did not score has no score to lose and none to invent. Zero
    /// would read as the worst possible match and one as a perfect comparison
    /// that never happened; both are false of an exact match.
    Scored(Score),
    /// A name the list does not hold, kept by its exact key as a show of its
    /// own. The show is filed the first time the key is seen and found by
    /// the key every time after.
    ///
    /// Asked after every stage of the list has refused the name, by the key
    /// alone: the scorer never compares a name with one of these, so a name
    /// that is close to one is a show of its own.
    Unlisted,
}

/// A filename that was recognized.
///
/// The show is the corpus entry that matched, rather than the text the filename
/// spelled. A [`Refusal`] carries that second thing, and confusing the two is how
/// a near miss gets written down as a title. Under [`Stage::Unlisted`] the show
/// is one kept for a name the list does not hold, and the title is the name's
/// own spelling.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The show the corpus filed the entry as, or the show kept for a name the
    /// list does not hold.
    pub show: ShowId,
    /// The title as the corpus spells it, or as the name spelled it under
    /// [`Stage::Unlisted`].
    pub title: String,
    /// The episode in the release's numbering, as it was read from the name.
    ///
    /// The four cases of [`Episode`] travel here whole, because this is what a
    /// consumer reads and the four lead different places. A film spells none
    /// and can be marked watched; a file holding twelve episodes spells several
    /// and is neither the first nor the twelfth. Reduced to a number and an
    /// absence, the two would arrive alike, and a file of twelve episodes would
    /// mark the series watched.
    ///
    /// Not [`Known`](crate::Known), and this is the one place in the workspace
    /// the difference has to be argued rather than assumed. `Known` exists
    /// because a source that *cannot* report a value and one that did not
    /// report it are different facts about that source. A filename is not a
    /// source and declares no capabilities, so the third case would have
    /// nothing to mean.
    pub episode: Episode,
    /// Which stage decided, and how well where it scored.
    pub stage: Stage,
}

/// Why a filename was not recognized.
///
/// Reported and never guessed at. Writing progress against the wrong title is
/// the worst failure this program can have, and a best guess below the threshold
/// is how it is reached.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    /// The title as the filename spelled it, before any match was tried.
    pub parsed: String,
    /// How close the best candidate came, where there was one to compare.
    ///
    /// Absent and low are different answers and lead different places. Absent
    /// says nothing was scored: the name spelled no title or two seasons, or
    /// the list left nothing to compare it with, because the index found no
    /// candidate or because none it found spells the season and the part the
    /// name does. A low score says the corpus was searched and nothing in it
    /// that spells the name's season and part was close, which points at the
    /// name or at a missing entry. It says how close and never why that was
    /// not enough: a candidate is refused below the scorer's floor, and above
    /// it where the next candidate ran level. Reserved rather than enforced,
    /// as the field is public and nothing here stops a caller writing `None`
    /// after scoring.
    pub best: Option<Score>,
}

/// Several entries one name reaches, and none of them chosen.
///
/// Apart from a [`Refusal`] because the two send a reader to different places.
/// A refusal says the corpus was searched and nothing in it was the title; an
/// ambiguity says the name reaches more than one entry and does not say
/// which. What settles it is the entries' own data - a format, an
/// episode count, a date - which is the list's to answer and not the text's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguity {
    /// The title as the filename spelled it.
    pub parsed: String,
    /// The entries it reaches: those its key reaches in the order the corpus
    /// filed them, then those it reaches through a type word.
    ///
    /// More than one wherever [`by_key`] built it: one candidate is a [`Match`]
    /// and none is no ambiguity at all. Reserved rather than enforced, as
    /// [`Refusal::best`] is - the field is public and nothing here refuses a
    /// shorter list.
    pub candidates: Vec<Candidate>,
}

/// One entry of several that a name reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The show the corpus filed the entry as.
    pub show: ShowId,
    /// The title as the corpus spells it.
    pub title: String,
}

/// What recognition answers with.
#[derive(Debug, Clone, PartialEq)]
pub enum Recognition {
    /// A show, and the stage that decided it.
    Recognized(Match),
    /// Several entries, and nothing in the name to choose between them.
    ///
    /// A variant of its own rather than a refusal carrying a list, because a
    /// consumer that treats it as a refusal loses the one thing that makes it
    /// answerable later, and a consumer that treats it as a match has to invent
    /// which candidate it meant.
    Ambiguous(Ambiguity),
    /// Nothing matched, and why.
    ///
    /// A variant rather than an `Err`, because an unrecognized filename is a
    /// fact about the filename and not a failure of the program. Nothing
    /// retries it, nothing logs it as a fault, and something downstream has to
    /// tell the user about it.
    Unrecognized(Refusal),
}

#[cfg(test)]
mod tests {
    use super::{
        Ambiguity, Candidate, Match, Recognition, Refusal, Score, Stage, by_key, decide, read,
        read_file,
    };
    use crate::ShowId;
    use crate::encoding::Confidence;
    use crate::path::RawPath;
    use crate::recognize::altname::{Altnames, Assignment};
    use crate::recognize::corpus::Corpus;
    use crate::recognize::index::Index;
    use crate::recognize::parse::{Episode, Kind, Parsed, parse};

    /// A score the tests use where the exact value is not the point.
    fn a_score(value: f64) -> Score {
        Score::new(value).expect("the value is between zero and one")
    }

    /// What a file of this name spells, so that a test names a file rather
    /// than building the parse by hand.
    fn named(name: &str) -> Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    /// The answer stage two gives a file of this name against this corpus.
    fn found(name: &str, corpus: &Corpus) -> Option<Recognition> {
        by_key(&named(name), corpus)
    }

    /// A recognition of this show by this stage, for comparing against.
    fn recognized(show: i64, title: &str, episode: Episode) -> Recognition {
        Recognition::Recognized(Match {
            show: ShowId(show),
            title: title.to_owned(),
            episode,
            stage: Stage::Key,
        })
    }

    /// One of several entries a name reaches.
    fn candidate(show: i64, title: &str) -> Candidate {
        Candidate {
            show: ShowId(show),
            title: title.to_owned(),
        }
    }

    /// What a file of this name is read as against this corpus, with no
    /// assignments.
    fn read_as(name: &str, corpus: &Corpus) -> (Parsed, Recognition) {
        read(named(name), &Altnames::new(), corpus, &Index::of(corpus))
    }

    #[test]
    fn a_title_that_ends_in_a_number_is_read_on_where_its_key_reaches_nothing() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title 13.mkv", &corpus);

        assert_eq!(parsed.title.as_deref(), Some("Show Title 13"));
        assert_eq!(parsed.episode, Episode::Absent);
        assert_eq!(parsed.title_with_number, None);
        assert_eq!(answer, recognized(1, "Show Title 13", Episode::Absent));
    }

    #[test]
    fn a_title_read_on_keeps_the_words_after_its_number() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title 13 - The Subtitle",
            Vec::<&str>::new(),
        );

        let (parsed, answer) = read_as("Show Title 13 - The Subtitle.mkv", &corpus);

        assert_eq!(
            parsed.title.as_deref(),
            Some("Show Title 13 - The Subtitle")
        );
        assert_eq!(
            answer,
            recognized(2, "Show Title 13 - The Subtitle", Episode::Absent)
        );
    }

    /// What a file at this path is read as against this corpus, with no
    /// assignments.
    fn read_at(path: &str, corpus: &Corpus) -> (Parsed, Recognition) {
        read_file(
            &RawPath::from_bytes(path.as_bytes().to_vec()),
            &Altnames::new(),
            corpus,
            &Index::of(corpus),
        )
    }

    /// A corpus of these titles, each spelled one way, filed as shows
    /// numbered from one in the order given.
    fn a_list_of(titles: &[&str]) -> Corpus {
        let mut corpus = Corpus::new();
        for (show, title) in (1..).zip(titles) {
            corpus.file(ShowId(show), title, Vec::<&str>::new());
        }
        corpus
    }

    #[test]
    fn a_name_that_begins_with_its_episode_reaches_the_show_named_after_it() {
        let (parsed, answer) = read_as("01 Show Title.mkv", &a_list_of(&["Show Title"]));

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode_first, None);
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));
    }

    #[test]
    fn a_title_that_begins_with_a_number_reaches_its_entry_whole() {
        let (parsed, answer) = read_as(
            "12 Quiet Lanterns.mkv",
            &a_list_of(&["12 Quiet Lanterns", "Quiet Lanterns"]),
        );

        assert_eq!(parsed.episode, Episode::Absent);
        assert_eq!(answer, recognized(1, "12 Quiet Lanterns", Episode::Absent));
    }

    #[test]
    fn a_name_that_begins_with_its_episode_is_of_the_show_its_directory_names() {
        let (parsed, answer) = read_at(
            "/anime/Show Title/01 Quiet Harbor.mkv",
            &a_list_of(&["Show Title"]),
        );

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert!(parsed.from_directory);
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));
    }

    #[test]
    fn a_name_with_no_title_is_of_the_show_its_directory_names() {
        let (parsed, answer) = read_at(
            "/anime/Show Title/01 - Quiet Harbor.mkv",
            &a_list_of(&["Show Title"]),
        );

        assert!(parsed.from_directory);
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));

        let (_, answer) = read_at("/anime/Show Title/01.mkv", &a_list_of(&["Show Title"]));

        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));
    }

    #[test]
    fn a_name_with_a_title_of_its_own_never_asks_its_directory() {
        let (parsed, answer) = read_at(
            "/anime/Other Show/[Group] Show Title - 03.mkv",
            &a_list_of(&["Other Show"]),
        );

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert!(!parsed.from_directory);
        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_season_or_a_disc_directory_is_part_of_the_show_above_it() {
        let list = a_list_of(&["Show Title", "Show Title Season 2"]);

        let (parsed, answer) = read_at("/anime/Show Title/Season 2/01 - Quiet Harbor.mkv", &list);

        assert_eq!(parsed.season, Some(2));
        assert_eq!(
            answer,
            recognized(2, "Show Title Season 2", Episode::Only(1))
        );

        let (_, answer) = read_at("/anime/Show Title/Disc 1/01 Quiet Harbor.mkv", &list);

        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));
    }

    #[test]
    fn a_directory_gives_what_the_name_does_not_spell_and_the_name_keeps_its_own() {
        let list = a_list_of(&["Show Title Season 3 Part 2"]);

        let (_, answer) = read_at(
            "/anime/Show Title Season 3 Part 2/01 - Quiet Harbor.mkv",
            &list,
        );
        assert_eq!(
            answer,
            recognized(1, "Show Title Season 3 Part 2", Episode::Only(1))
        );

        let (parsed, _) = read_at(
            "/anime/Show Title Season 2/S03E01 - Quiet Harbor.mkv",
            &list,
        );
        assert_eq!(parsed.season, Some(3));

        let (parsed, _) = read_at("/anime/Show Title (1999)/01 - Quiet Harbor.mkv", &list);
        assert_eq!(parsed.year, Some(1999));
        let (parsed, _) = read_at(
            "/anime/Show Title (1999)/01 - Quiet Harbor (2001).mkv",
            &list,
        );
        assert_eq!(parsed.year, Some(2001));
    }

    #[test]
    fn a_directory_the_key_misses_is_scored_on_its_own_words() {
        let (_, answer) = read_at(
            "/anime/Show Title Cour 2 - The Subtitle/01 - Quiet Harbor.mkv",
            &a_list_of(&["Show Title", "Show Title Cour 2 The Subtitle"]),
        );

        assert!(
            matches!(
                &answer,
                Recognition::Recognized(Match {
                    show: ShowId(2),
                    episode: Episode::Only(1),
                    stage: Stage::Scored(_),
                    ..
                })
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_name_with_neither_a_title_nor_an_episode_never_asks_its_directory() {
        let (parsed, answer) =
            read_at("/anime/Show Title/[1080p].mkv", &a_list_of(&["Show Title"]));

        assert!(!parsed.from_directory);
        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_title_read_from_a_directory_carries_how_the_directory_was_decoded() {
        let path = RawPath::from_bytes(
            b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e [Group]/01 - Quiet Harbor.mkv".to_vec(),
        );

        let (parsed, _) = read_file(
            &path,
            &Altnames::new(),
            &Corpus::new(),
            &Index::of(&Corpus::new()),
        );

        assert_eq!(parsed.title.as_deref(), Some("ソレッタ"));
        assert_eq!(parsed.confidence, Confidence::Detected);
    }

    #[test]
    fn a_directory_of_specials_reads_its_files_as_extras_of_the_show_above_it() {
        let (parsed, _) = read_at(
            "/anime/Show Title/Specials/01 - Quiet Harbor.mkv",
            &a_list_of(&["Show Title"]),
        );

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.kind, Some(Kind::Extra));
        assert_eq!(parsed.episode, Episode::Only(1));
    }

    #[test]
    fn a_directory_whose_number_is_its_own_reaches_its_entry() {
        let (_, answer) = read_at(
            "/anime/Show Title 13/01 - Quiet Harbor.mkv",
            &a_list_of(&["Show Title 13"]),
        );

        assert_eq!(answer, recognized(1, "Show Title 13", Episode::Only(1)));
    }

    #[test]
    fn a_directory_that_names_a_release_is_kept_as_the_show() {
        let (parsed, answer) = read_at(
            "/video/Show.Title.Season.1.1999.DVDRip-Group/01 Quiet Harbor.mkv",
            &Corpus::new(),
        );

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode, Episode::Only(1));
        assert!(parsed.from_directory);
        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );

        for directory in [
            "Show Title Season 2",
            "Show Title (1999)",
            "[Group] Show Title",
            "Show Title OVA",
        ] {
            let (parsed, _) = read_at(
                &format!("/video/{directory}/01 Quiet Harbor.mkv"),
                &Corpus::new(),
            );

            assert!(parsed.from_directory, "{directory}");
            assert_eq!(
                parsed.kind.is_some(),
                directory.ends_with("OVA"),
                "{directory}"
            );
        }
    }

    #[test]
    fn a_plain_directory_that_reaches_nothing_leaves_the_name_as_it_spells_itself() {
        for (path, title) in [
            (
                "/video/7 Lanterns per Night.mkv",
                Some("7 Lanterns per Night"),
            ),
            ("/video/01 - Quiet Harbor.mkv", None),
        ] {
            let (parsed, _) = read_at(path, &Corpus::new());

            assert_eq!(parsed.title.as_deref(), title, "{path}");
            assert!(!parsed.from_directory, "{path}");
        }
    }

    #[test]
    fn a_walk_ends_at_the_first_directory_that_names_a_show() {
        // A show kept inside another's directory is not that other show.
        let (_, answer) = read_at(
            "/anime/Show Title/Other Title/01 Quiet Harbor.mkv",
            &a_list_of(&["Show Title"]),
        );

        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_name_the_list_answers_never_asks_its_directory() {
        let (parsed, answer) = read_at(
            "/anime/Other Show/01 Show Title.mkv",
            &a_list_of(&["Show Title", "Other Show"]),
        );

        assert!(!parsed.from_directory);
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));

        let (parsed, answer) = read_at(
            "/anime/Show Title/01 Quiet Harbor.mkv",
            &a_list_of(&["Show Title", "Quiet Harbor"]),
        );

        assert_eq!(parsed.title.as_deref(), Some("Quiet Harbor"));
        assert_eq!(answer, recognized(2, "Quiet Harbor", Episode::Only(1)));
    }

    #[test]
    fn a_title_beginning_with_one_digit_is_no_episode_of_its_directory() {
        let (parsed, answer) = read_at(
            "/anime/Other Show/7 Lanterns per Night.mkv",
            &a_list_of(&["Other Show"]),
        );

        assert!(!parsed.from_directory);
        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_part_of_the_show_gives_its_season_its_part_and_its_type() {
        let list = Corpus::new();
        for (path, season, part, kind) in [
            (
                "/anime/[Group] Show Title/Season 2 OVA/01 - Quiet Harbor.mkv",
                Some(2),
                None,
                Some(Kind::Extra),
            ),
            (
                "/anime/[Group] Show Title/[OVA]/01.mkv",
                None,
                None,
                Some(Kind::Extra),
            ),
            (
                "/anime/[Group] Show Title/Disc 1 - Season 2/01.mkv",
                Some(2),
                None,
                None,
            ),
            (
                "/anime/[Group] Show Title/Cour 2/01.mkv",
                None,
                Some(2),
                None,
            ),
        ] {
            let (parsed, _) = read_at(path, &list);

            assert!(parsed.from_directory, "{path}");
            assert_eq!(
                (parsed.season, parsed.part, parsed.kind),
                (season, part, kind),
                "{path}"
            );
        }
    }

    #[test]
    fn the_season_of_the_name_and_then_of_the_nearest_directory_holds() {
        let list = Corpus::new();
        for (path, season) in [
            (
                "/anime/[Group] Show Title/Season 2/S03E01 - Quiet Harbor.mkv",
                Some(3),
            ),
            ("/anime/Show Title Season 3/Season 2/01.mkv", Some(2)),
        ] {
            let (parsed, _) = read_at(path, &list);

            assert!(parsed.from_directory, "{path}");
            assert_eq!(parsed.season, season, "{path}");
        }
    }

    #[test]
    fn a_directory_never_gives_the_file_its_release_group() {
        let (parsed, _) = read_at(
            "/video/[Group] Show Title/01 Quiet Harbor.mkv",
            &Corpus::new(),
        );

        assert!(parsed.from_directory);
        assert_eq!(parsed.release_group, None);
    }

    #[test]
    fn a_name_of_a_season_and_an_episode_alone_reads_its_directory() {
        let (_, answer) = read_at(
            "/anime/Show Title/Season 2 - 03.mkv",
            &a_list_of(&["Show Title", "Show Title Season 2"]),
        );

        assert_eq!(
            answer,
            recognized(2, "Show Title Season 2", Episode::Only(3))
        );
    }

    #[test]
    fn a_walk_through_a_parent_reference_ends_there() {
        let list = a_list_of(&["Show Title"]);

        let (_, answer) = read_at("/anime/Show Title/../01 Quiet Harbor.mkv", &list);
        assert!(
            matches!(answer, Recognition::Unrecognized(_)),
            "got {answer:?}"
        );

        let (_, answer) = read_at("/anime/Show Title/./01 Quiet Harbor.mkv", &list);
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(1)));
    }

    #[test]
    fn an_assignment_answers_a_name_read_episode_first_and_a_directory() {
        let list = a_list_of(&["Show Title", "Show Title (2019)"]);
        let mut altnames = Altnames::new();
        altnames
            .name("Show Title", ShowId(2), &list)
            .expect("the name is assigned");
        let index = Index::of(&list);

        for path in ["01 Show Title.mkv", "/anime/Show Title/01 Quiet Harbor.mkv"] {
            let (_, answer) = read_file(
                &RawPath::from_bytes(path.as_bytes().to_vec()),
                &altnames,
                &list,
                &index,
            );

            assert!(
                matches!(
                    answer,
                    Recognition::Recognized(Match {
                        show: ShowId(2),
                        stage: Stage::Altname(_),
                        ..
                    })
                ),
                "{path}: {answer:?}"
            );
        }
    }

    #[test]
    fn a_title_read_on_that_reaches_nothing_is_decided_as_the_name_spells_it() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title 13 - The Subtitle.mkv", &corpus);

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode, Episode::Only(13));
        assert_eq!(
            answer,
            decide(
                &named("Show Title 13 - The Subtitle.mkv"),
                &Altnames::new(),
                &corpus,
                &Index::of(&corpus)
            )
        );
    }

    #[test]
    fn a_title_read_on_is_asked_before_the_scorer_would_answer_the_title_as_parsed() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());

        let scored = decide(
            &named("Show Title 13.mkv"),
            &Altnames::new(),
            &corpus,
            &Index::of(&corpus),
        );

        assert!(
            matches!(
                scored,
                Recognition::Recognized(Match {
                    show: ShowId(1),
                    episode: Episode::Only(13),
                    stage: Stage::Scored(_),
                    ..
                })
            ),
            "got {scored:?}"
        );
        assert_eq!(
            read_as("Show Title 13.mkv", &corpus).1,
            recognized(1, "Show Title 13", Episode::Absent)
        );
    }

    #[test]
    fn an_assignment_of_the_title_read_on_answers_before_the_key() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title 13 (2019)", Vec::<&str>::new());
        let mut altnames = Altnames::new();
        altnames
            .name("Show Title 13", ShowId(2), &corpus)
            .expect("the name is assigned");

        let (_, answer) = read(
            named("Show Title 13.mkv"),
            &altnames,
            &corpus,
            &Index::of(&corpus),
        );

        assert!(
            matches!(
                answer,
                Recognition::Recognized(Match {
                    show: ShowId(2),
                    stage: Stage::Altname(_),
                    ..
                })
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_title_that_reaches_an_entry_keeps_its_number_as_the_episode() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title 13", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title 13.mkv", &corpus);

        assert_eq!(parsed.episode, Episode::Only(13));
        assert_eq!(answer, recognized(1, "Show Title", Episode::Only(13)));
    }

    #[test]
    fn a_number_after_a_dash_is_never_read_into_the_title() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 13", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title - 13.mkv", &corpus);

        assert_eq!(parsed.episode, Episode::Only(13));
        assert_ne!(answer, recognized(1, "Show Title 13", Episode::Absent));
    }

    #[test]
    fn a_name_the_user_assigned_is_not_read_on() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Assigned Show", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title 13", Vec::<&str>::new());
        let mut altnames = Altnames::new();
        altnames
            .name("Show Title", ShowId(1), &corpus)
            .expect("the name is assigned");

        let (parsed, _) = read(
            named("Show Title 13.mkv"),
            &altnames,
            &corpus,
            &Index::of(&corpus),
        );

        assert_eq!(parsed.episode, Episode::Only(13));
    }

    #[test]
    fn a_name_the_list_holds_whole_answers_before_its_title() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title Episode 0: The First Contact",
            Vec::<&str>::new(),
        );

        let (parsed, answer) = read_as(
            "[Group] Show Title Episode 0: The First Contact [1080p].mkv",
            &corpus,
        );

        assert_eq!(
            parsed.title.as_deref(),
            Some("Show Title Episode 0: The First Contact")
        );
        assert_eq!(
            answer,
            recognized(
                2,
                "Show Title Episode 0: The First Contact",
                Episode::Absent
            )
        );
    }

    #[test]
    fn a_season_and_an_episode_the_list_spells_whole_belong_to_its_title() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 7", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title Season 7 Episode 13",
            Vec::<&str>::new(),
        );

        let (parsed, answer) = read_as("Show Title Season 7 Episode 13.mkv", &corpus);

        assert_eq!(parsed.season, None);
        assert_eq!(
            answer,
            recognized(2, "Show Title Season 7 Episode 13", Episode::Absent)
        );
    }

    #[test]
    fn a_name_read_whole_keeps_the_episode_after_its_dash() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title Episode 0", Vec::<&str>::new());

        assert_eq!(
            read_as("Show Title Episode 0 - 02.mkv", &corpus).1,
            recognized(2, "Show Title Episode 0", Episode::Only(2))
        );
    }

    #[test]
    fn a_name_the_list_does_not_hold_whole_is_read_as_parsed() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());

        assert_eq!(
            read_as(
                "[Group] Show Title Episode 0: The First Contact [1080p].mkv",
                &corpus
            )
            .1,
            recognized(1, "Show Title", Episode::Only(0))
        );
    }

    #[test]
    fn an_assignment_of_the_title_answers_before_the_name_read_whole() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title Episode 0: The First Contact",
            Vec::<&str>::new(),
        );
        let mut altnames = Altnames::new();
        altnames
            .name("Show Title", ShowId(1), &corpus)
            .expect("the name is assigned");

        let (_, answer) = read(
            named("[Group] Show Title Episode 0: The First Contact [1080p].mkv"),
            &altnames,
            &corpus,
            &Index::of(&corpus),
        );

        assert!(
            matches!(
                answer,
                Recognition::Recognized(Match {
                    show: ShowId(1),
                    stage: Stage::Altname(_),
                    ..
                })
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_number_after_a_dash_inside_a_name_is_never_read_whole() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title 13 - The Subtitle",
            Vec::<&str>::new(),
        );

        assert_eq!(
            read_as("Show Title - 13 - The Subtitle.mkv", &corpus).1,
            recognized(1, "Show Title", Episode::Only(13))
        );
    }

    #[test]
    fn an_assignment_of_the_name_read_whole_answers_it() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(2), "Another Show", Vec::<&str>::new());
        let mut altnames = Altnames::new();
        altnames
            .name(
                "Show Title Episode 0: The First Contact",
                ShowId(2),
                &corpus,
            )
            .expect("the name is assigned");

        let (_, answer) = read(
            named("[Group] Show Title Episode 0: The First Contact [1080p].mkv"),
            &altnames,
            &corpus,
            &Index::of(&corpus),
        );

        assert!(
            matches!(
                answer,
                Recognition::Recognized(Match {
                    show: ShowId(2),
                    stage: Stage::Altname(_),
                    ..
                })
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_name_read_whole_keeps_the_type_it_spells() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title Season 3 OVA", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title Season 3 OVA.mkv", &corpus);

        assert_eq!(parsed.kind, Some(Kind::Extra));
        assert_eq!(
            answer,
            recognized(2, "Show Title Season 3 OVA", Episode::Absent)
        );
    }

    #[test]
    fn a_name_read_whole_is_answered_by_its_whole_spelling_alone() {
        // Without its type word the whole name is `Show Title Season 3`, which
        // the list holds; the title as parsed answers it instead.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3", Vec::<&str>::new());

        let (parsed, _) = read_as("Show Title Season 3 OVA - 01.mkv", &corpus);

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.season, Some(3));
    }

    #[test]
    fn a_name_whose_whole_key_is_its_titles_keeps_its_parse() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 2", Vec::<&str>::new());

        let (parsed, answer) = read_as("Show Title Season 2 - 03.mkv", &corpus);

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.season, Some(2));
        assert_eq!(
            answer,
            recognized(1, "Show Title Season 2", Episode::Only(3))
        );
    }

    #[test]
    fn a_name_that_spells_a_type_also_reaches_the_entry_whose_title_spells_one() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title Season 3 OVA", Vec::<&str>::new());

        assert_eq!(
            found("[Group] Show Title Season 3 OVA - 01 [1080p].mkv", &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Show Title".to_owned(),
                candidates: vec![
                    candidate(1, "Show Title Season 3"),
                    candidate(2, "Show Title Season 3 OVA"),
                ],
            }))
        );
        assert_eq!(
            found("[Group] Show Title Season 3 - 01 [1080p].mkv", &corpus),
            Some(recognized(1, "Show Title Season 3", Episode::Only(1))),
            "a name of no type word never reaches the entry through its type"
        );
    }

    #[test]
    fn a_name_of_an_ova_reaches_the_entry_its_title_spells_and_no_film() {
        // The title as read reaches an entry, so it is not asked again
        // without its type word, and a film's spelling is no OVA's.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title OVA", Vec::<&str>::new());
        corpus.file(ShowId(3), "Show Title Movie", Vec::<&str>::new());

        assert_eq!(
            found("[Group] Show Title OVA - 01 [1080p].mkv", &corpus),
            Some(recognized(2, "Show Title OVA", Episode::Only(1)))
        );
        assert_eq!(
            found("[Group] Show Title Movie [1080p].mkv", &corpus),
            Some(recognized(3, "Show Title Movie", Episode::Absent))
        );
    }

    #[test]
    fn a_name_reaches_the_spellings_filed_under_its_own_kind_alone() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3 Movie", Vec::<&str>::new());

        assert_eq!(
            found("[Group] Show Title Season 3 Movie [1080p].mkv", &corpus),
            Some(recognized(1, "Show Title Season 3 Movie", Episode::Absent))
        );
        assert_eq!(
            found("[Group] Show Title Season 3 OVA - 01 [1080p].mkv", &corpus),
            None
        );
    }

    #[test]
    fn a_name_reaches_the_entries_of_its_key_before_those_of_a_type_word() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3 OVA", Vec::<&str>::new());
        corpus.file(ShowId(2), "Show Title Season 3", Vec::<&str>::new());

        let Some(Recognition::Ambiguous(several)) =
            found("[Group] Show Title Season 3 OVA - 01 [1080p].mkv", &corpus)
        else {
            panic!("the name reaches both entries");
        };
        let shows: Vec<ShowId> = several.candidates.iter().map(|c| c.show).collect();
        assert_eq!(shows, [ShowId(2), ShowId(1)]);
    }

    #[test]
    fn a_file_of_a_special_named_in_japanese_title_quotes_reaches_the_special() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Series Title", ["作品名"]);
        corpus.file(ShowId(2), "Series Title: The Subtitle", ["作品名『副題』"]);

        assert_eq!(
            found("作品名『副題』.mkv", &corpus),
            Some(recognized(2, "Series Title: The Subtitle", Episode::Absent))
        );
    }

    #[test]
    fn an_answer_names_the_stage_that_decided_and_how_well_it_scored() {
        // The whole reason a recognition is inspectable. A score on its own
        // cannot be compared against another stage's, because an exact match on
        // a key and a best guess among candidates are not the same kind of
        // certainty, and a stage on its own does not say how close the call
        // was. The two travel together or neither is worth printing.
        let scored = Recognition::Recognized(Match {
            show: ShowId(1),
            title: "Show".to_owned(),
            episode: Episode::Only(3),
            stage: Stage::Scored(a_score(0.91)),
        });

        let Recognition::Recognized(found) = &scored else {
            panic!("the answer is a recognition")
        };
        assert_eq!(found.stage, Stage::Scored(a_score(0.91)));
    }

    #[test]
    fn a_stage_that_does_not_score_carries_no_score_to_lose() {
        // An exact match has no score, and a type that carried one anyway would
        // need a value invented for it - zero, which reads as the worst
        // possible match, or one, which reads as a perfect comparison that
        // never happened. Neither is true, so the stage holds the score where
        // there is one and nothing where there is not.
        let manual = Stage::Altname(Assignment {
            spelled: "SnK".to_owned(),
            season: None,
            part: None,
            show: ShowId(1),
            title: "Show".to_owned(),
        });
        let exact = Stage::Key;

        assert_ne!(manual, exact);
        assert!(!matches!(manual, Stage::Scored(_)));
        assert!(!matches!(exact, Stage::Scored(_)));
    }

    #[test]
    fn a_film_has_no_episode_rather_than_episode_zero() {
        // A film carries no episode number and neither does a name that simply
        // does not spell one. Zero would be a number, and every consumer below
        // would have to know it meant absence - the sentinel this workspace
        // refuses everywhere else.
        //
        // A film and a batch are apart here as well, which is why the parser's
        // own answer is carried rather than a number that may be missing: a
        // film can be marked watched and a file of twelve episodes cannot.
        let film = Match {
            show: ShowId(1),
            title: "A Film".to_owned(),
            episode: Episode::Absent,
            stage: Stage::Key,
        };
        let batch = Match {
            episode: Episode::Several,
            ..film.clone()
        };

        assert_eq!(film.episode, Episode::Absent);
        assert_ne!(film.episode, batch.episode);
    }

    #[test]
    fn a_filename_nothing_matched_is_an_answer_and_not_an_error() {
        // Unrecognized is a fact about the filename, not a failure of the
        // program, so it is a variant rather than an `Err`. It carries what the
        // parser made of the name, because that is the first thing anybody
        // asking why will want, and how close the best candidate came.
        let nothing_close = Recognition::Unrecognized(Refusal {
            parsed: "Some Show".to_owned(),
            best: Some(a_score(0.41)),
        });
        let nothing_to_compare = Recognition::Unrecognized(Refusal {
            parsed: "Some Show".to_owned(),
            best: None,
        });

        assert_ne!(nothing_close, nothing_to_compare);
    }

    #[test]
    fn a_score_outside_zero_to_one_is_refused_and_each_bound_is_not() {
        // A score is a proportion and nothing else can be one. Refused rather
        // than clamped, for the reason the watched policy gives: a number
        // quietly moved into range makes the explanation disagree with what
        // actually happened.
        assert!(Score::new(-0.01).is_none());
        assert!(Score::new(1.01).is_none());
        assert!(Score::new(f64::NAN).is_none());
        assert!(Score::new(0.0).is_some());
        assert!(Score::new(1.0).is_some());
    }

    #[test]
    fn a_better_score_compares_greater_than_a_worse_one() {
        // The one thing a score is for. Ordering it is what lets a stage pick a
        // best candidate and what lets a refusal say how close it came.
        assert!(a_score(0.9) > a_score(0.4));
    }

    #[test]
    fn a_score_reads_back_as_the_number_it_was_given() {
        // Its own test rather than a second assertion above, because the break
        // list prints the name of whatever went red: a score read back as
        // nothing has nothing to do with ordering, and a report saying so sends
        // the next reader to the wrong line.
        //
        // Compared as bits rather than as numbers. A lint denies strict float
        // equality, and the claim here is exactly that the bits went in and came
        // back unchanged, which is what bit equality says and what an epsilon
        // would only approximate.
        assert_eq!(a_score(0.5).value().to_bits(), 0.5_f64.to_bits());
    }

    #[test]
    fn a_name_one_entry_is_filed_under_names_it_and_the_stage_that_found_it() {
        // The release group, the resolution and the episode are all off the
        // name before the key is built, so what is compared is the title
        // alone, and the answer names the entry rather than the text.
        let corpus: Corpus = ["Show Title", "Another Show"].into_iter().collect();

        assert_eq!(
            found("[Group] Show Title - 03 [1080p].mkv", &corpus),
            Some(recognized(1, "Show Title", Episode::Only(3)))
        );
    }

    #[test]
    fn two_entries_under_one_key_are_reported_rather_than_chosen() {
        // A remake spelled like its original, told apart on the list by a year
        // the key drops because the parser reports a year apart from the
        // title. Answering the first entry filed would write a season of one
        // show against the other, and nothing in the name says which is meant.
        let corpus: Corpus = ["Fruits Basket", "Fruits Basket (2019)"]
            .into_iter()
            .collect();

        assert_eq!(
            found("Fruits Basket - 01.mkv", &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Fruits Basket".to_owned(),
                candidates: vec![
                    candidate(1, "Fruits Basket"),
                    candidate(2, "Fruits Basket (2019)")
                ],
            }))
        );
    }

    #[test]
    fn two_shows_of_one_title_are_reported_rather_than_merged() {
        // Two entries that share a title are two shows, and a name that
        // reaches both names neither. Merged by their title, they would be
        // one answer, and every file of one would be written to the other.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(7), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(9), "Show Title", Vec::<&str>::new());

        assert_eq!(
            found("Show Title - 03.mkv", &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Show Title".to_owned(),
                candidates: vec![candidate(7, "Show Title"), candidate(9, "Show Title")],
            }))
        );
    }

    #[test]
    fn a_match_names_the_show_it_found() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(7), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(9), "Another Show", Vec::<&str>::new());

        assert_eq!(
            found("Another Show - 03.mkv", &corpus),
            Some(Recognition::Recognized(Match {
                show: ShowId(9),
                title: "Another Show".to_owned(),
                episode: Episode::Only(3),
                stage: Stage::Key,
            }))
        );
    }

    #[test]
    fn a_name_no_entry_is_filed_under_leaves_the_answer_to_the_stages_below() {
        // Not a refusal. An exact key hits or misses, and a name the parser
        // shortened or a list spells differently misses it while still being
        // recognizable further down, so this stage says nothing rather than
        // saying no.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(found("Some Other Show - 03.mkv", &corpus), None);
    }

    #[test]
    fn a_mark_the_name_keeps_chooses_between_two_seasons() {
        // End to end, and the reason the marks are in the key at all. A colon
        // is the whole difference between these two entries: the file that
        // kept it names one, and the file that lost it names neither.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();

        assert_eq!(
            found("Nisekoi: - 03.mkv", &corpus),
            Some(recognized(2, "Nisekoi:", Episode::Only(3)))
        );
        assert_eq!(
            found("Nisekoi - 03.mkv", &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Nisekoi".to_owned(),
                candidates: vec![candidate(1, "Nisekoi"), candidate(2, "Nisekoi:")],
            }))
        );
    }

    #[test]
    fn a_part_is_not_the_season_of_the_same_number() {
        // Both are entries of their own on a list, the part being half of the
        // first season and the season being the whole of the second. A name
        // abbreviating the season reaches the season alone.
        let corpus: Corpus = [
            "Gokushufudou",
            "Gokushufudou Part 2",
            "Gokushufudou Season 2",
        ]
        .into_iter()
        .collect();

        assert_eq!(
            found("Gokushufudou S2 - 03.mkv", &corpus),
            Some(recognized(3, "Gokushufudou Season 2", Episode::Only(3)))
        );
    }

    #[test]
    fn a_name_the_user_gave_outranks_what_the_corpus_answers() {
        // The discriminating half: the corpus is asked the same name and
        // answers something else, so the assignment is not a fallback for
        // where nothing matched. Two entries a filename cannot tell apart are
        // what a person settles by hand, and the answer names the stage that
        // decided it and the assignment it decided by.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();
        let mut altnames = Altnames::new();
        altnames
            .name("Nisekoi", ShowId(2), &corpus)
            .expect("two entries under one key are the user's to settle");

        assert_eq!(
            decide(
                &named("Nisekoi - 03.mkv"),
                &altnames,
                &corpus,
                &Index::of(&corpus)
            ),
            Recognition::Recognized(Match {
                show: ShowId(2),
                title: "Nisekoi:".to_owned(),
                episode: Episode::Only(3),
                stage: Stage::Altname(Assignment {
                    spelled: "Nisekoi".to_owned(),
                    season: None,
                    part: None,
                    show: ShowId(2),
                    title: "Nisekoi:".to_owned(),
                }),
            })
        );
        assert_eq!(
            by_key(&named("Nisekoi - 03.mkv"), &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Nisekoi".to_owned(),
                candidates: vec![candidate(1, "Nisekoi"), candidate(2, "Nisekoi:")],
            }))
        );
    }

    #[test]
    fn a_file_the_user_named_is_found_by_what_that_file_spelled() {
        // The parser takes `S03` out of the title, so the season is in the
        // file's key and in no text the file leaves behind. A stage looking an
        // assignment up by the title alone would miss the file it was made for
        // and catch the season before it, where the corpus was already right.
        // The answer carries the assignment with the season it was bound to,
        // which is what an explanation shows a user whose first-season file
        // did not follow it.
        let corpus: Corpus = [
            "Kaguya-sama: Love is War",
            "Kaguya-sama: Love is War -Ultra Romantic-",
        ]
        .into_iter()
        .collect();
        let third = named("Kaguya-sama.Love.is.War.S03E03.1080p.WEB-DL.mkv");
        let mut altnames = Altnames::new();
        altnames
            .name_as_parsed(&third, ShowId(2), &corpus)
            .expect("the third season reaches no entry, so naming it takes nothing");

        assert_eq!(
            decide(&third, &altnames, &corpus, &Index::of(&corpus)),
            Recognition::Recognized(Match {
                show: ShowId(2),
                title: "Kaguya-sama: Love is War -Ultra Romantic-".to_owned(),
                episode: Episode::Only(3),
                stage: Stage::Altname(Assignment {
                    spelled: "Kaguya-sama Love is War".to_owned(),
                    season: Some(3),
                    part: None,
                    show: ShowId(2),
                    title: "Kaguya-sama: Love is War -Ultra Romantic-".to_owned(),
                }),
            })
        );
        assert_eq!(
            decide(
                &named("Kaguya-sama.Love.is.War.S01E03.1080p.WEB-DL.mkv"),
                &altnames,
                &corpus,
                &Index::of(&corpus)
            ),
            recognized(1, "Kaguya-sama: Love is War", Episode::Only(3))
        );
    }

    #[test]
    fn a_name_nobody_assigned_is_left_to_the_corpus() {
        // The other direction, and what stops stage one from swallowing
        // everything: a name nobody named reaches the stage below unchanged,
        // and the answer says which stage decided.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            decide(
                &named("Show Title - 03.mkv"),
                &Altnames::new(),
                &corpus,
                &Index::of(&corpus)
            ),
            recognized(1, "Show Title", Episode::Only(3))
        );
    }

    #[test]
    fn a_name_no_stage_answered_is_refused_rather_than_guessed_at() {
        // The sequence ends in a refusal, and the refusal is the scorer's.
        // `Show Title` shares one word of the five between it and the name,
        // and that is what travels: the score and not the entry, so nothing
        // below can read a near miss as a title.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            decide(
                &named("Some Other Show - 03.mkv"),
                &Altnames::new(),
                &corpus,
                &Index::of(&corpus)
            ),
            Recognition::Unrecognized(Refusal {
                parsed: "Some Other Show".to_owned(),
                best: Some(a_score(0.4)),
            })
        );
    }

    #[test]
    fn a_refusal_with_nothing_to_score_carries_no_score_rather_than_zero() {
        // No entry shares a word with the name, so the index hands the scorer
        // nothing and no comparison is made. Zero would say one was made
        // and the closest entry shared nothing, which sends a reader to the
        // name; an absence sends them to the list.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            decide(
                &named("Completely Different - 03.mkv"),
                &Altnames::new(),
                &corpus,
                &Index::of(&corpus)
            ),
            Recognition::Unrecognized(Refusal {
                parsed: "Completely Different".to_owned(),
                best: None,
            })
        );
    }

    #[test]
    fn a_name_spelling_no_title_is_refused_with_nothing_to_name() {
        // A name the parser makes nothing of reaches the end of the sequence
        // like any other and is refused there. It names nothing, because
        // there is nothing it could name, and it scores nothing, because with
        // no title the index is not asked and the scorer is handed no
        // candidate.
        let corpus: Corpus = ["Show Title"].into_iter().collect();
        let nothing = named("");
        assert_eq!(nothing.title, None);

        assert_eq!(
            decide(&nothing, &Altnames::new(), &corpus, &Index::of(&corpus)),
            Recognition::Unrecognized(Refusal {
                parsed: String::new(),
                best: None,
            })
        );
    }

    #[test]
    fn a_file_of_several_episodes_names_its_title_and_none_of_them() {
        // The title was recognized and the episode was not, which is one
        // answer rather than two. A batch reduced to no episode would arrive
        // looking like a film, and a consumer would mark the series watched.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            found("[Group] Show Title 01-12 [BD].mkv", &corpus),
            Some(recognized(1, "Show Title", Episode::Several))
        );
    }
}
