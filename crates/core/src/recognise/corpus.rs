//! The entries a name reaches, filed under the key each spelling reaches.
//!
//! Stage two is an exact match on the normalised key, and this is what it
//! matches against. **The caller builds it**, because this crate performs no
//! I/O and a list of entries comes from somewhere that does.
//!
//! **A key with marks matches exactly, and a key without them matches every
//! entry with the same letters, season and part, whatever their marks.** A
//! filename loses marks its entry carries, because a name written for Windows
//! cannot hold `:` or `?`. A file of `Nisekoi:` is named `Nisekoi - 03.mkv`,
//! and so is a file of `Nisekoi`. Both are candidates, and two candidates are
//! an ambiguity that is reported rather than guessed at. A key that does carry
//! marks is never widened the other way, because its own entry may simply be
//! missing from the list, and a widened match would hand the file to the season
//! before it.
//!
//! **An entry filed under several spellings is one candidate.** Two spellings
//! of one entry reach one key when the title is repeated among them, or when
//! they differ only in the marks a lookup widens over. One entry reached twice
//! is not two entries.

use std::collections::HashMap;

use crate::recognise::normalise::Key;

/// The titles recognition matches against.
///
/// Every spelling of an entry is filed under its own key, and every key answers
/// with the entry's title rather than with the spelling that was reached.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    /// Filed under the key with its marks taken off, so that a name which lost
    /// its marks still reaches the bucket its entry is in. What the bucket
    /// holds is then told apart by the marks alone: everything else about the
    /// key is what put it there.
    filed: HashMap<Key, Vec<Filed>>,
}

/// One spelling of one entry.
#[derive(Debug, Clone)]
struct Filed {
    /// The marks that spelling ended in, which is all the bucket does not
    /// already hold.
    marks: String,
    /// The spelling itself, which the key cannot be read back into: a key
    /// erases the breaks between words, and a stage that compares words needs
    /// them.
    spelled: String,
    /// The entry's title, which is what an answer names.
    title: String,
}

impl Corpus {
    /// An empty corpus, to file entries into.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// File an entry under its title and under every other spelling it carries.
    ///
    /// A spelling with no letters in it is filed nowhere: nothing could look it
    /// up, because a name spelling no letters has no key either.
    pub fn file(&mut self, title: &str, also_spelled: impl IntoIterator<Item = impl AsRef<str>>) {
        self.put(title, title);
        for spelling in also_spelled {
            self.put(spelling.as_ref(), title);
        }
    }

    /// File one spelling of one entry.
    fn put(&mut self, spelling: &str, title: &str) {
        let Some(key) = Key::from_title(spelling) else {
            return;
        };
        self.filed
            .entry(key.without_marks())
            .or_default()
            .push(Filed {
                marks: key.marks().to_owned(),
                spelled: spelling.to_owned(),
                title: title.to_owned(),
            });
    }

    /// Every spelling one entry is filed under.
    ///
    /// For the stage that compares an entry against a name word by word: an
    /// entry filed under romaji, English and native spellings meets the name on
    /// whichever of them is closest, and the title it answers with is not
    /// necessarily the one that matched.
    pub(super) fn spellings_of(&self, title: &str) -> Vec<&str> {
        let mut found: Vec<&str> = self
            .spellings()
            .filter(|(_, filed)| *filed == title)
            .map(|(spelled, _)| spelled)
            .collect();
        // Sorted for the same reason the index sorts: these come out of a hash
        // map, and a caller choosing between two of them that score alike
        // would choose differently between two runs of one program.
        found.sort_unstable();
        found
    }

    /// Every spelling filed, with the entry it belongs to.
    ///
    /// For the stage that compares words rather than keys, so that one corpus
    /// answers both and the two cannot be filed differently.
    pub(super) fn spellings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.filed
            .values()
            .flatten()
            .map(|filed| (filed.spelled.as_str(), filed.title.as_str()))
    }

    /// The entries a key reaches, in the order they were filed.
    ///
    /// Empty where nothing is filed under it, which is not a refusal: an exact
    /// key either hits or misses, and the stages below are what answer a name
    /// that misses.
    #[must_use]
    pub fn candidates(&self, key: &Key) -> Vec<&str> {
        let mut found: Vec<&str> = Vec::new();
        let Some(bucket) = self.filed.get(&key.without_marks()) else {
            return found;
        };
        for filed in bucket {
            let reached = key.marks().is_empty() || filed.marks == key.marks();
            if reached && !found.contains(&filed.title.as_str()) {
                found.push(&filed.title);
            }
        }
        found
    }
}

impl<S: AsRef<str>> FromIterator<S> for Corpus {
    /// A corpus of entries spelled one way each.
    fn from_iter<I: IntoIterator<Item = S>>(titles: I) -> Self {
        let mut corpus = Self::new();
        for title in titles {
            corpus.file(title.as_ref(), Vec::<&str>::new());
        }
        corpus
    }
}

#[cfg(test)]
mod tests {
    use super::Corpus;
    use crate::recognise::normalise::Key;

    /// The key a list entry spelled this way is filed under.
    fn listed(title: &str) -> Key {
        Key::from_title(title).expect("a title with letters in it has a key")
    }

    #[test]
    fn an_entry_answers_the_key_of_its_own_spelling() {
        let corpus: Corpus = ["Show Title", "Another Show"].into_iter().collect();

        assert_eq!(corpus.candidates(&listed("Show Title")), ["Show Title"]);
    }

    #[test]
    fn a_key_no_entry_is_filed_under_reaches_nothing() {
        // A miss is empty rather than everything. A lookup that fell back to
        // the whole corpus would make every name ambiguous, and one that
        // answered the nearest entry would write progress against a title
        // nobody named.
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert!(corpus.candidates(&listed("Some Other Show")).is_empty());
    }

    #[test]
    fn an_entry_spelled_two_ways_is_one_candidate() {
        // `file` puts the title under its own key, so a spelling that repeats
        // the title files that entry twice. Answered as they are filed, the two
        // would be an ambiguity between one entry and itself.
        let mut corpus = Corpus::new();
        corpus.file("Show Title", ["Show Title", "Shoo Taitoru"]);

        assert_eq!(corpus.candidates(&listed("Show Title")), ["Show Title"]);
        assert_eq!(corpus.candidates(&listed("Shoo Taitoru")), ["Show Title"]);
    }

    #[test]
    fn a_key_without_marks_reaches_the_entries_that_carry_them() {
        // The rule the filename side needs. A name that lost its colon reaches
        // both seasons, and which of them it is belongs to the list rather than
        // to the text.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();

        assert_eq!(
            corpus.candidates(&listed("Nisekoi")),
            ["Nisekoi", "Nisekoi:"]
        );
    }

    #[test]
    fn a_key_with_marks_reaches_only_the_entry_that_spells_them() {
        // The other half, and what makes the marks worth keeping at all: a
        // name that did keep its colon names one season exactly.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();

        assert_eq!(corpus.candidates(&listed("Nisekoi:")), ["Nisekoi:"]);
    }

    #[test]
    fn a_key_with_marks_is_not_widened_to_an_entry_without_them() {
        // A marked key is never widened, because the entry it names may be
        // missing from the list. Widening would hand a file of the sequel to
        // the season before it, which is the failure this program is built to
        // prevent.
        let corpus: Corpus = ["Nisekoi"].into_iter().collect();

        assert!(corpus.candidates(&listed("Nisekoi:")).is_empty());
    }

    #[test]
    fn entries_that_differ_by_a_season_or_a_part_are_not_candidates_together() {
        // The widening is over marks alone. A season and a part are in the key
        // and stay there, so the second season is not a candidate for a name
        // that spells the first, and the second part is not the second season.
        let corpus: Corpus = [
            "Show Title",
            "Show Title Season 2",
            "Show Title Part 2",
            "Show Title!",
        ]
        .into_iter()
        .collect();

        assert_eq!(
            corpus.candidates(&listed("Show Title Season 2")),
            ["Show Title Season 2"]
        );
        assert_eq!(
            corpus.candidates(&listed("Show Title Part 2")),
            ["Show Title Part 2"]
        );
        assert_eq!(
            corpus.candidates(&listed("Show Title")),
            ["Show Title", "Show Title!"]
        );
    }

    #[test]
    fn an_entry_hands_over_its_spellings_in_one_order() {
        // A bucket comes out of a hash map, so what is filed under one entry
        // arrives in no particular order. A stage picking between spellings
        // that score alike would pick differently between two runs.
        let mut corpus = Corpus::new();
        corpus.file("Show Title", ["Zeta Spelling", "Alpha Spelling"]);

        assert_eq!(
            corpus.spellings_of("Show Title"),
            ["Alpha Spelling", "Show Title", "Zeta Spelling"]
        );
    }

    #[test]
    fn a_spelling_with_no_letters_is_filed_nowhere() {
        // `Season 2` alone names no entry, so it has no key to be filed under
        // and nothing can look it up. The entry's other spellings are filed as
        // usual.
        let mut corpus = Corpus::new();
        corpus.file("Show Title", ["Season 2"]);

        assert_eq!(corpus.candidates(&listed("Show Title")), ["Show Title"]);
        assert!(Key::from_title("Season 2").is_none());
    }
}
