//! What a filename turned out to be, and which stage decided it.
//!
//! The pipeline has four stages: a mapping the user made by hand, an exact match
//! on a normalised key, a token index that narrows the corpus to a handful, and
//! a score over what is left. **Only three of them can answer.** The index
//! decides nothing - it hands candidates to the scorer - so [`Stage`] has three
//! variants and not four. Why they run in that order is written on [`Stage`].
//!
//! Nothing here computes a recognition yet. The answer is defined first so that
//! every stage below is written against a shape that already has to explain
//! itself, which is the same order the timeline was built in.
//!
//! **A stage is named rather than numbered.** The criteria speak of "stage one",
//! and that number is not carried here: inserting a stage renumbers every stage
//! after it, while a name says what decided and does not move.

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
    /// The episode in the release's numbering, where the name carried one.
    ///
    /// [`Option`] and not [`Known`](crate::Known), which is the one place in
    /// this workspace the difference has to be argued rather than assumed.
    /// `Known` exists because a source that *cannot* report a value and one that
    /// did not report it are different facts about that source. A filename is
    /// not a source and declares no capabilities, so the third case would have
    /// nothing to mean, and a name either spells an episode or does not.
    pub episode: Option<u32>,
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

/// What recognition answers with.
#[derive(Debug, Clone, PartialEq)]
pub enum Recognition {
    /// A title, and the stage that decided it.
    Recognised(Match),
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
    use super::{Match, Recognition, Refusal, Score, Stage};

    /// A score the tests use where the exact value is not the point.
    fn a_score(value: f64) -> Score {
        Score::new(value).expect("the value is between nought and one")
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
            episode: Some(3),
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
        // `Option` and not `Known`, deliberately. `Known` exists because a
        // source that cannot report a value and one that did not report it are
        // different facts. A filename is not a source and declares no
        // capabilities, so the third case would have nothing to mean.
        let film = Match {
            title: "A Film".to_owned(),
            episode: None,
            stage: Stage::Key,
        };

        assert_eq!(film.episode, None);
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
}
