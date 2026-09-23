//! What a filename turned out to be, and which stage decided it.
//!
//! The pipeline has four stages: a mapping the user made by hand, an exact match
//! on a normalised key, a token index that narrows the corpus to a handful, and
//! a score over what is left. **Only three of them can answer.** The index
//! decides nothing - it hands candidates to the scorer - so [`Stage`] has three
//! variants and not four. Why they run in that order is written on [`Stage`].
//!
//! The answer is defined for every stage, so that each one is written against a
//! shape that already has to explain itself. [`by_key`] is the only stage here
//! that computes one.
//!
//! **A stage is named rather than numbered.** The criteria speak of "stage one",
//! and that number is not carried here: inserting a stage renumbers every stage
//! after it, while a name says what decided and does not move.

pub mod corpus;
pub mod normalise;
pub mod parse;

use crate::recognise::corpus::Corpus;
use crate::recognise::normalise::Key;
use crate::recognise::parse::{Episode, Parsed};

/// What an exact match on the normalised key answers, where it answers at all.
///
/// Nothing where the name reaches no entry: an exact key hits or misses, and a
/// name that misses is what the stages below are for. A refusal is the whole
/// sequence's answer rather than this stage's, so nothing here says a name was
/// not recognised - only that this stage did not recognise it.
///
/// Several entries under one key is an answer, and the answer is that the name
/// does not say. Two list entries share a key honestly - a remake spelled like
/// its original, a season told from the one before it by a mark a filename
/// cannot carry - and picking whichever was filed first writes progress against
/// a title nobody named.
#[must_use]
pub fn by_key(parsed: &Parsed, corpus: &Corpus) -> Option<Recognition> {
    let spelled = parsed.title.as_deref()?;
    let candidates = corpus.candidates(&Key::from_parsed(parsed)?);

    match candidates.as_slice() {
        [] => None,
        [title] => Some(Recognition::Recognised(Match {
            title: (*title).to_owned(),
            episode: parsed.episode,
            stage: Stage::Key,
        })),
        several => Some(Recognition::Ambiguous(Ambiguity {
            parsed: spelled.to_owned(),
            candidates: several.iter().map(|title| (*title).to_owned()).collect(),
        })),
    }
}

/// How well a candidate title matched, from nothing to exactly.
///
/// A proportion and nothing else can be one, so it is checked on the way in and
/// private afterwards. **Refused rather than clamped**, for the reason
/// [`WatchedPolicyError`](crate::timeline::WatchedPolicyError) gives: a number
/// quietly moved into range makes an explanation disagree with what actually
/// happened, and the whole purpose of carrying a score is to be believed.
///
/// Not serialisable, deliberately. A derived `Deserialize` on a newtype is
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stage {
    /// A mapping the user made by hand.
    Altname,
    /// An exact match on the normalised key.
    Key,
    /// The best candidate the scorer found, and how well it scored.
    ///
    /// The score lives inside the stage rather than beside it, so that a stage
    /// which did not score has no score to lose and none to invent. Nought
    /// would read as the worst possible match and one as a perfect comparison
    /// that never happened; both are false of an exact match.
    Scored(Score),
}

/// A filename that was recognised.
///
/// The title is the corpus entry that matched, rather than the text the filename
/// spelled. A [`Refusal`] carries that second thing, and confusing the two is how
/// a near miss gets written down as a title.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    /// The title as the corpus spells it.
    pub title: String,
    /// The episode in the release's numbering, exactly as the name spelled it.
    ///
    /// The parser's four answers travel here whole, because this is what a
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

/// Why a filename was not recognised.
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
    /// Absent and low are different answers and lead different places. Absent is
    /// reserved for the index having offered nothing to score at all, which
    /// points at the corpus; a low score says the corpus was searched and
    /// nothing in it was close, which points at the name or at a missing entry.
    /// Reserved rather than enforced: nothing here can stop a caller writing
    /// `None` after scoring, and the stage that will honour it is not written.
    pub best: Option<Score>,
}

/// Several entries one name reaches, and none of them chosen.
///
/// Apart from a [`Refusal`] because the two send a reader to different places.
/// A refusal says the corpus was searched and nothing in it was the title; an
/// ambiguity says the corpus holds the title more than once and the name does
/// not say which. What settles it is the entries' own data - a format, an
/// episode count, a date - which is the list's to answer and not the text's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguity {
    /// The title as the filename spelled it.
    pub parsed: String,
    /// The entries it reaches, as the corpus spells them, in the order the
    /// corpus filed them.
    ///
    /// More than one wherever [`by_key`] built it: one candidate is a [`Match`]
    /// and none is no ambiguity at all. Reserved rather than enforced, as
    /// [`Refusal::best`] is - the field is public and nothing here refuses a
    /// shorter list.
    pub candidates: Vec<String>,
}

/// What recognition answers with.
#[derive(Debug, Clone, PartialEq)]
pub enum Recognition {
    /// A title, and the stage that decided it.
    Recognised(Match),
    /// Several titles, and nothing in the name to choose between them.
    ///
    /// A variant of its own rather than a refusal carrying a list, because a
    /// consumer that treats it as a refusal loses the one thing that makes it
    /// answerable later, and a consumer that treats it as a match has to invent
    /// which candidate it meant.
    Ambiguous(Ambiguity),
    /// Nothing matched, and why.
    ///
    /// A variant rather than an `Err`, because an unrecognised filename is a
    /// fact about the filename and not a failure of the program. Nothing
    /// retries it, nothing logs it as a fault, and something downstream has to
    /// tell the user about it.
    Unrecognised(Refusal),
}

#[cfg(test)]
mod tests {
    use super::{Ambiguity, Match, Recognition, Refusal, Score, Stage, by_key};
    use crate::path::RawPath;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::parse::{Episode, Parsed, parse};

    /// A score the tests use where the exact value is not the point.
    fn a_score(value: f64) -> Score {
        Score::new(value).expect("the value is between nought and one")
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

    /// A recognition of this title by this stage, for comparing against.
    fn recognised(title: &str, episode: Episode) -> Recognition {
        Recognition::Recognised(Match {
            title: title.to_owned(),
            episode,
            stage: Stage::Key,
        })
    }

    #[test]
    fn an_answer_names_the_stage_that_decided_and_how_well_it_scored() {
        // The whole reason a recognition is inspectable. A score on its own
        // cannot be compared against another stage's, because an exact match on
        // a key and a best guess among candidates are not the same kind of
        // certainty, and a stage on its own does not say how close the call
        // was. The two travel together or neither is worth printing.
        let scored = Recognition::Recognised(Match {
            title: "Show".to_owned(),
            episode: Episode::Only(3),
            stage: Stage::Scored(a_score(0.91)),
        });

        let Recognition::Recognised(found) = &scored else {
            panic!("the answer is a recognition")
        };
        assert_eq!(found.stage, Stage::Scored(a_score(0.91)));
    }

    #[test]
    fn a_stage_that_does_not_score_carries_no_score_to_lose() {
        // An exact match has no score, and a type that carried one anyway would
        // need a value invented for it - nought, which reads as the worst
        // possible match, or one, which reads as a perfect comparison that
        // never happened. Neither is true, so the stage holds the score where
        // there is one and nothing where there is not.
        let manual = Stage::Altname;
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
        // Unrecognised is a fact about the filename, not a failure of the
        // program, so it is a variant rather than an `Err`. It carries what the
        // parser made of the name, because that is the first thing anybody
        // asking why will want, and how close the best candidate came.
        let nothing_close = Recognition::Unrecognised(Refusal {
            parsed: "Some Show".to_owned(),
            best: Some(a_score(0.41)),
        });
        let nothing_to_compare = Recognition::Unrecognised(Refusal {
            parsed: "Some Show".to_owned(),
            best: None,
        });

        assert_ne!(nothing_close, nothing_to_compare);
    }

    #[test]
    fn a_score_outside_nought_to_one_is_refused_and_each_bound_is_not() {
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
            Some(recognised("Show Title", Episode::Only(3)))
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
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned()
                ],
            }))
        );
    }

    #[test]
    fn a_name_no_entry_is_filed_under_leaves_the_answer_to_the_stages_below() {
        // Not a refusal. An exact key hits or misses, and a name the parser
        // shortened or a list spells differently misses it while still being
        // recognisable further down, so this stage says nothing rather than
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
            Some(recognised("Nisekoi:", Episode::Only(3)))
        );
        assert_eq!(
            found("Nisekoi - 03.mkv", &corpus),
            Some(Recognition::Ambiguous(Ambiguity {
                parsed: "Nisekoi".to_owned(),
                candidates: vec!["Nisekoi".to_owned(), "Nisekoi:".to_owned()],
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
            Some(recognised("Gokushufudou Season 2", Episode::Only(3)))
        );
    }

    #[test]
    fn a_file_of_several_episodes_names_its_title_and_none_of_them() {
        // The title was recognised and the episode was not, which is one
        // answer rather than two. A batch reduced to no episode would arrive
        // looking like a film, and a consumer would mark the series watched.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            found("[Group] Show Title 01-12 [BD].mkv", &corpus),
            Some(recognised("Show Title", Episode::Several))
        );
    }
}
