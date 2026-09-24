//! Which of a handful of candidates the name is about, where one stands out.
//!
//! Stage four, and the last that can answer. It is given the candidates stage
//! three narrowed to and decides between them, or decides that it cannot.
//!
//! **The text it scores is the whole name**, through
//! [`Parsed::text`](super::parse::Parsed::text), which is the filename with the
//! release's own words struck out. A title ends at the first thing the parser
//! recognises, so a file of a special whose name extends a series' name reports
//! the series and nothing of the subtitle that tells the two apart.
//!
//! **A candidate is judged by how much of the name it accounts for**, as the
//! words the two share against the words they spell between them, and an entry
//! is judged by its best spelling. A file named in English and an entry filed
//! under romaji, English and native spellings meet on whichever of them the
//! name is closest to.
//!
//! **A candidate answers by the distance it puts between itself and the next
//! one.** Two entries of one franchise both score highly against a name
//! carrying the franchise's words, and a hair between the first two is an
//! ambiguity. Measured on the same names, a score threshold set at 0.90 answers
//! 29 in a hundred right and writes the wrong title for four; the margin
//! answers 68 right and writes one wrong.
//!
//! **The margin is wider where what is compared is short**, because the
//! arithmetic is coarser there. A score is the shared words over the words the
//! two spell between them, so each word counts for more the fewer of them there
//! are: where the winner spells two words of a name that spells three, a
//! runner-up adding a word of its own and sharing no more of the name lands
//! 0.13 behind, which the ordinary margin reads as a decision. The same
//! runner-up lands 0.06 behind a winner of seven words. What counts as short is
//! the shorter of the name and the spelling that won.
//!
//! Measured over two filenames for every spelling of the five thousand most
//! popular AniList entries, 49,428 in all: a spelling of three words or more
//! loses a word in one of the two and its tail in the other. Of the 26,286
//! names the exact key did not answer, the right entry is answered for 68 in a
//! hundred, 31 are refused, and the wrong title is written for one.

use std::collections::HashSet;

use crate::recognise::corpus::Corpus;
use crate::recognise::normalise::words_in;
use crate::recognise::parse::Parsed;
use crate::recognise::{Match, Score, Stage};

/// How many words a comparison has to spell before the ordinary margin applies.
///
/// Counted on the shorter of the two sides: the name, and the spelling the best
/// candidate won with.
pub const SHORT: usize = 3;

/// How far the best candidate has to be ahead of the next one.
pub const AHEAD: f64 = 0.10;

/// How far ahead the best candidate has to be where what is compared is short.
pub const AHEAD_SHORT: f64 = 0.20;

/// How alike the best candidate has to be at all, whatever its lead.
///
/// A name reaching one candidate has nothing to be ahead of, so the margin
/// alone would answer any name sharing a single word with a single entry. Over
/// the measured names this floor refuses 330 answers the margin allows, 321 of
/// them right. Every name there is built from a spelling the corpus holds, and
/// the file this floor is for is the one whose entry is on no list.
pub const NEAR_ENOUGH: f64 = 0.70;

/// How much of the two the name and the spelling have in common.
///
/// The words they share against the words they spell between them, so that
/// neither a long entry nor a long name is favoured by its length alone.
///
/// # Panics
///
/// Never. Two sets cannot share more than they hold between them, so the share
/// is a proportion and [`Score::new`] accepts it.
#[must_use]
pub fn alike(name: &[String], spelling: &[String]) -> Score {
    let name: HashSet<&String> = name.iter().collect();
    let spelling: HashSet<&String> = spelling.iter().collect();
    let between = name.len() + spelling.len();
    if between == 0 {
        return Score::new(0.0).expect("nought is a proportion");
    }
    let shared = name.intersection(&spelling).count();
    let of = |count: usize| f64::from(u32::try_from(count).unwrap_or(u32::MAX));
    Score::new(2.0 * of(shared) / of(between))
        .expect("what two sets share cannot exceed what they hold")
}

/// The entry a name is about, where one candidate stands out from the next.
///
/// Nothing where the best two run level: entries of one franchise score alike
/// against a name carrying the franchise's words, and picking the higher of two
/// that are neck and neck is guessing.
#[must_use]
pub fn by_score(parsed: &Parsed, candidates: &[&str], corpus: &Corpus) -> Option<Match> {
    let name = words_in(&parsed.text);
    let mut judged: Vec<(f64, usize, &str)> = candidates
        .iter()
        .map(|title| {
            let (score, words) = corpus
                .spellings_of(title)
                .into_iter()
                .map(|spelling| {
                    let spelled = words_in(spelling);
                    (alike(&name, &spelled).value(), spelled.len())
                })
                .max_by(|one, other| one.0.total_cmp(&other.0))
                .unwrap_or((0.0, 0));
            (score, words, *title)
        })
        .collect();
    // Stable, so candidates that score alike keep the order they arrived in.
    judged.sort_by(|one, other| other.0.total_cmp(&one.0));

    let &(best, words, title) = judged.first()?;
    let runner_up = judged.get(1).map_or(0.0, |&(score, ..)| score);
    let ahead = if words.min(name.len()) < SHORT {
        AHEAD_SHORT
    } else {
        AHEAD
    };
    if best < NEAR_ENOUGH || best - runner_up < ahead {
        return None;
    }
    Some(Match {
        title: title.to_owned(),
        episode: parsed.episode,
        stage: Stage::Scored(Score::new(best)?),
    })
}

#[cfg(test)]
mod tests {
    use super::{NEAR_ENOUGH, alike, by_score};
    use crate::path::RawPath;
    use crate::recognise::Stage;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::normalise::words_in;
    use crate::recognise::parse::{Episode, parse};

    /// What a file of this name spells.
    fn named(name: &str) -> crate::recognise::parse::Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    #[test]
    fn a_short_entry_has_to_win_by_more_than_a_long_one() {
        // The whole reason the margin moves. In both corpora the runner-up
        // spells one word more than the winner and shares no more of the name,
        // so the gap between them is what that one word alone is worth: 0.13
        // where the winner spells two words, 0.11 where it spells four. The
        // short winner is refused and the long one is answered.
        let short: Corpus = ["Alpha Beta", "Alpha Beta Delta"].into_iter().collect();
        let long: Corpus = ["Alpha Beta Gamma Delta", "Alpha Beta Gamma Delta Epsilon"]
            .into_iter()
            .collect();

        let by_the_short = by_score(
            &named("Alpha Beta Gamma - 03.mkv"),
            &["Alpha Beta", "Alpha Beta Delta"],
            &short,
        );
        let by_the_long = by_score(
            &named("Alpha Beta Gamma Delta - 03.mkv"),
            &["Alpha Beta Gamma Delta", "Alpha Beta Gamma Delta Epsilon"],
            &long,
        );

        assert_eq!(by_the_short, None, "a two-word entry needs a wider win");
        assert!(
            by_the_long.is_some(),
            "the same win is enough for four words"
        );
    }

    #[test]
    fn a_candidate_with_nothing_to_beat_still_has_to_be_near_enough() {
        // A name reaching one entry has no runner-up, so a margin measured
        // against nothing would answer every name sharing a word with a single
        // entry. `Some Other Show` shares `show` and nothing else.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        let alone = by_score(&named("Some Other Show - 03.mkv"), &["Show Title"], &corpus);

        assert_eq!(alone, None);
        assert!(alike(&words_in("Some Other Show"), &words_in("Show Title")).value() < NEAR_ENOUGH);
    }

    #[test]
    fn two_candidates_running_level_are_no_answer() {
        // An unresolved mapping is reported, never guessed, and this stage
        // reports it by not answering: the sequence below it refuses.
        let corpus: Corpus = ["Show Title Alpha", "Show Title Beta"]
            .into_iter()
            .collect();

        assert_eq!(
            by_score(
                &named("Show Title - 03.mkv"),
                &["Show Title Alpha", "Show Title Beta"],
                &corpus
            ),
            None
        );
    }

    #[test]
    fn the_answer_carries_the_score_it_won_with_and_names_the_stage() {
        let corpus: Corpus = ["Shingeki no Kyojin", "Kimi no Na wa"]
            .into_iter()
            .collect();

        let found = by_score(
            &named("[Group] Shingeki no Kyojin - 03 [1080p].mkv"),
            &["Shingeki no Kyojin", "Kimi no Na wa"],
            &corpus,
        )
        .expect("one candidate stands out");

        assert_eq!(found.title, "Shingeki no Kyojin");
        assert_eq!(found.episode, Episode::Only(3));
        assert!(matches!(found.stage, Stage::Scored(_)));
    }

    #[test]
    fn the_name_is_scored_rather_than_the_title_the_parser_left() {
        // What this stage exists for. A title ends at the first thing the
        // parser recognises, so this file reports `Show Title` and nothing of
        // the subtitle after it. Judged on that title the series wins, and the
        // special it extends is never answered at all.
        let corpus: Corpus = ["Show Title", "Show Title Cour 2 The Subtitle"]
            .into_iter()
            .collect();
        let file = named("Show Title Cour 2 - The Subtitle - 03.mkv");
        assert_eq!(file.title.as_deref(), Some("Show Title"));

        let found = by_score(
            &file,
            &["Show Title", "Show Title Cour 2 The Subtitle"],
            &corpus,
        )
        .expect("the subtitle is in the name even where the title lost it");

        assert_eq!(found.title, "Show Title Cour 2 The Subtitle");
    }

    #[test]
    fn what_the_release_spells_does_not_count_against_a_candidate() {
        // The name is judged on the text the release's own words were struck
        // out of. Counted whole, a group, a resolution and an extension are
        // four words no entry spells, and a two-word entry would lose most of
        // its score to them.
        let corpus: Corpus = ["Show Title", "Another Show"].into_iter().collect();

        let found = by_score(
            &named("[SubsPlease] Show Title - 03 (1080p) [A1B2C3D4].mkv"),
            &["Show Title", "Another Show"],
            &corpus,
        )
        .expect("the release's own words are not the name");

        assert_eq!(found.title, "Show Title");
    }

    #[test]
    fn an_entry_is_judged_by_the_spelling_the_name_is_closest_to() {
        // A file named in English against an entry filed under romaji and
        // English: the entry meets the name on whichever spelling is closest,
        // not on the one its title is written in.
        let mut corpus: Corpus = ["Kimi no Na wa"].into_iter().collect();
        corpus.file("Shingeki no Kyojin", ["Attack on Titan"]);

        let found = by_score(
            &named("Attack on Titan - 03.mkv"),
            &["Shingeki no Kyojin", "Kimi no Na wa"],
            &corpus,
        )
        .expect("the English spelling is what matched");

        assert_eq!(found.title, "Shingeki no Kyojin");
    }

    #[test]
    fn a_name_sharing_everything_scores_one_and_sharing_nothing_scores_nothing() {
        let show = words_in("Show Title");

        assert!((alike(&show, &show).value() - 1.0).abs() < f64::EPSILON);
        assert!(alike(&show, &words_in("Something Else")).value() < f64::EPSILON);
    }
}
