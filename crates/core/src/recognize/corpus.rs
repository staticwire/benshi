//! The entries a name reaches, filed under the key each spelling reaches.
//!
//! Stage two is an exact match on the normalized key, and this is what it
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
//! is not two entries. **Two entries that share a title are two candidates**,
//! because an entry is the show it is filed as, and its title is what a person
//! is shown.

use std::collections::HashMap;

use crate::ShowId;
use crate::recognize::normalize::{Key, without_type_words};
use crate::recognize::parse::Kind;
use crate::settle::Facts;

/// The shows recognition matches against.
///
/// Every spelling of an entry is filed under its own key, and every key answers
/// with the entry's show rather than with the spelling that was reached.
#[derive(Debug, Clone, Default)]
pub struct Corpus {
    /// Filed under the key with its marks taken off, so that a name which lost
    /// its marks still reaches the bucket its entry is in. What the bucket
    /// holds is then told apart by the marks alone: everything else about the
    /// key is what put it there.
    filed: HashMap<Key, Vec<Filed>>,
    /// The title of every show filed, which is what an answer shows a person.
    titles: HashMap<ShowId, String>,
    /// What the list holds of each show filed with its facts, which settles
    /// whether a file is of it.
    facts: HashMap<ShowId, Facts>,
    /// Each spelling that `without_type_words` takes type words out of, filed
    /// again without them under each kind they name, for a name that spells
    /// that kind: where the parser leaves the word out of the title it
    /// reports, the key would miss.
    typed: HashMap<Kind, HashMap<Key, Vec<Filed>>>,
    /// The numbers each show's spellings carry, a spelling of no letters
    /// among them, which a year a name spells is read against.
    numbers: HashMap<ShowId, Vec<u32>>,
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
    /// The show the entry is, which is what an answer names.
    show: ShowId,
}

impl Corpus {
    /// An empty corpus, to file entries into.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// File an entry as a show, under its title and under every other spelling
    /// it carries.
    ///
    /// A spelling with no letters in it is filed nowhere: nothing could look it
    /// up, because a name spelling no letters has no key either.
    pub fn file(
        &mut self,
        show: ShowId,
        title: &str,
        also_spelled: impl IntoIterator<Item = impl AsRef<str>>,
    ) {
        self.titles.insert(show, title.to_owned());
        self.put(title, show);
        self.keep_numbers(title, show);
        for spelling in also_spelled {
            self.put(spelling.as_ref(), show);
            self.keep_numbers(spelling.as_ref(), show);
        }
    }

    /// Keep the numbers one spelling of one entry carries.
    fn keep_numbers(&mut self, spelling: &str, show: ShowId) {
        let carried = spelling
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|digits| digits.parse::<u32>().ok());
        self.numbers.entry(show).or_default().extend(carried);
    }

    /// File one spelling of one entry, and again without the type words
    /// `without_type_words` takes out of it.
    fn put(&mut self, spelling: &str, show: ShowId) {
        file_into(&mut self.filed, spelling, spelling, show);
        if let Some((untyped, kinds)) = without_type_words(spelling) {
            for kind in kinds {
                file_into(
                    self.typed.entry(kind).or_default(),
                    &untyped,
                    spelling,
                    show,
                );
            }
        }
    }

    /// File what the list holds of a show, in place of what was filed of it
    /// before.
    pub fn file_facts(&mut self, show: ShowId, facts: Facts) {
        self.facts.insert(show, facts);
    }

    /// What the list holds of a show, and nothing where it was filed with no
    /// facts.
    #[must_use]
    pub(crate) fn facts_of(&self, show: ShowId) -> Option<&Facts> {
        self.facts.get(&show)
    }

    /// Whether a run of digits in a spelling of a show reads as this number,
    /// as `Show Title 2032` carries 2032 and not 203.
    #[must_use]
    pub(crate) fn spells_number(&self, show: ShowId, number: u32) -> bool {
        self.numbers
            .get(&show)
            .is_some_and(|numbers| numbers.contains(&number))
    }

    /// The title a show was filed with, and nothing where it was not filed.
    #[must_use]
    pub fn title_of(&self, show: ShowId) -> Option<&str> {
        self.titles.get(&show).map(String::as_str)
    }

    /// Every spelling one entry is filed under.
    ///
    /// For the stage that compares an entry against a name word by word: an
    /// entry filed under romaji, English and native spellings meets the name on
    /// the closest of those that stage compares, and the title it answers with
    /// is not necessarily the one that matched.
    pub(super) fn spellings_of(&self, show: ShowId) -> Vec<&str> {
        let mut found: Vec<&str> = self
            .spellings()
            .filter(|&(_, filed)| filed == show)
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
    pub(super) fn spellings(&self) -> impl Iterator<Item = (&str, ShowId)> {
        self.filed
            .values()
            .flatten()
            .map(|filed| (filed.spelled.as_str(), filed.show))
    }

    /// The entries a key reaches through a spelling that held a type word of
    /// this kind, filed with the word taken out, in the order they were
    /// filed.
    ///
    /// For a name that spells that kind, and only for one. Each kind is filed
    /// apart: a name of an OVA reaches a spelling here only through a type
    /// word of an extra.
    #[must_use]
    pub(crate) fn typed_candidates(&self, kind: Kind, key: &Key) -> Vec<ShowId> {
        self.typed
            .get(&kind)
            .map_or_else(Vec::new, |typed| reached(typed, key))
    }

    /// The entries a key reaches, in the order they were filed.
    ///
    /// Empty where nothing is filed under it, which is not a refusal: an exact
    /// key either hits or misses, and the stages below are what answer a name
    /// that misses.
    #[must_use]
    pub fn candidates(&self, key: &Key) -> Vec<ShowId> {
        reached(&self.filed, key)
    }
}

/// File `filed_as` under its key, as one spelling of one entry spelled
/// `spelling`.
fn file_into(map: &mut HashMap<Key, Vec<Filed>>, filed_as: &str, spelling: &str, show: ShowId) {
    let Some(key) = Key::from_title(filed_as) else {
        return;
    };
    map.entry(key.without_marks()).or_default().push(Filed {
        marks: key.marks().to_owned(),
        spelled: spelling.to_owned(),
        show,
    });
}

/// The entries a key reaches in one map of spellings, in the order they
/// were filed.
fn reached(map: &HashMap<Key, Vec<Filed>>, key: &Key) -> Vec<ShowId> {
    let mut found: Vec<ShowId> = Vec::new();
    let Some(bucket) = map.get(&key.without_marks()) else {
        return found;
    };
    for filed in bucket {
        let reached = key.marks().is_empty() || filed.marks == key.marks();
        if reached && !found.contains(&filed.show) {
            found.push(filed.show);
        }
    }
    found
}

impl<S: AsRef<str>> FromIterator<S> for Corpus {
    /// A corpus of entries spelled one way each, filed as shows numbered from
    /// one in the order they are given.
    fn from_iter<I: IntoIterator<Item = S>>(titles: I) -> Self {
        let mut corpus = Self::new();
        for (show, title) in (1..).zip(titles) {
            corpus.file(ShowId(show), title.as_ref(), Vec::<&str>::new());
        }
        corpus
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Corpus;
    use crate::ShowId;
    use crate::recognize::normalize::Key;
    use crate::recognize::parse::Kind;
    use crate::settle::Facts;

    /// The key a list entry spelled this way is filed under.
    fn listed(title: &str) -> Key {
        Key::from_title(title).expect("a title with letters in it has a key")
    }

    /// The titles of the entries this spelling's key reaches, in the order
    /// the corpus answers them.
    fn reached<'c>(corpus: &'c Corpus, spelled: &str) -> Vec<&'c str> {
        corpus
            .candidates(&listed(spelled))
            .into_iter()
            .map(|show| corpus.title_of(show).expect("a candidate is a show filed"))
            .collect()
    }

    #[test]
    fn a_spelling_with_a_type_word_is_reached_without_it_by_a_name_of_a_type() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title Season 3", Vec::<&str>::new());
        corpus.file(
            ShowId(2),
            "Show Title Season 3 OVA",
            ["Show Title Season 3 Specials"],
        );
        corpus.file(ShowId(3), "Show Title The Movie", Vec::<&str>::new());

        let season = listed("Show Title Season 3");
        assert_eq!(corpus.candidates(&season), [ShowId(1)]);
        assert_eq!(corpus.typed_candidates(Kind::Extra, &season), [ShowId(2)]);
        assert_eq!(
            corpus.typed_candidates(Kind::Film, &listed("Show Title The")),
            [ShowId(3)]
        );
        assert_eq!(
            corpus.typed_candidates(Kind::Extra, &listed("Show Title The")),
            Vec::<ShowId>::new(),
            "a film is reached by a name of a film alone"
        );
        assert_eq!(
            corpus.typed_candidates(Kind::Extra, &listed("Show Title Season 3 OVA")),
            Vec::<ShowId>::new(),
            "under its kind, a spelling is filed only with its type word taken out"
        );
    }

    #[test]
    fn a_number_a_spelling_carries_is_known_of_its_show_alone() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title 2032", ["Other Title (1987 Release)"]);
        corpus.file(ShowId(2), "Second Show", Vec::<&str>::new());
        corpus.file(ShowId(3), "Third Show", ["1999"]);

        assert!(corpus.spells_number(ShowId(1), 2032));
        assert!(corpus.spells_number(ShowId(1), 1987));
        assert!(
            corpus.spells_number(ShowId(3), 1999),
            "a spelling of no letters"
        );
        assert!(!corpus.spells_number(ShowId(1), 203), "part of a number");
        assert!(!corpus.spells_number(ShowId(1), 2033));
        assert!(
            !corpus.spells_number(ShowId(2), 2032),
            "another show's number"
        );
    }

    #[test]
    fn an_entry_answers_the_key_of_its_own_spelling() {
        let corpus: Corpus = ["Show Title", "Another Show"].into_iter().collect();

        assert_eq!(reached(&corpus, "Show Title"), ["Show Title"]);
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
        corpus.file(ShowId(1), "Show Title", ["Show Title", "Shoo Taitoru"]);

        assert_eq!(corpus.candidates(&listed("Show Title")), [ShowId(1)]);
        assert_eq!(corpus.candidates(&listed("Shoo Taitoru")), [ShowId(1)]);
    }

    #[test]
    fn two_entries_of_one_title_are_two_candidates() {
        // A title does not name an entry: AniList holds entries that share a
        // romaji title. Told apart by the show each is, the two stay two.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(7), "Show Title", ["Shoo Taitoru"]);
        corpus.file(ShowId(9), "Show Title", Vec::<&str>::new());

        assert_eq!(
            corpus.candidates(&listed("Show Title")),
            [ShowId(7), ShowId(9)]
        );
        assert_eq!(corpus.title_of(ShowId(9)), Some("Show Title"));
        assert_eq!(
            corpus.spellings_of(ShowId(7)),
            ["Shoo Taitoru", "Show Title"]
        );
        assert_eq!(corpus.spellings_of(ShowId(9)), ["Show Title"]);
    }

    #[test]
    fn a_key_without_marks_reaches_the_entries_that_carry_them() {
        // The rule the filename side needs. A name that lost its colon reaches
        // both seasons, and which of them it is belongs to the list rather than
        // to the text.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();

        assert_eq!(reached(&corpus, "Nisekoi"), ["Nisekoi", "Nisekoi:"]);
    }

    #[test]
    fn a_key_with_marks_reaches_only_the_entry_that_spells_them() {
        // The other half, and what makes the marks worth keeping at all: a
        // name that did keep its colon names one season exactly.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();

        assert_eq!(reached(&corpus, "Nisekoi:"), ["Nisekoi:"]);
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
            reached(&corpus, "Show Title Season 2"),
            ["Show Title Season 2"]
        );
        assert_eq!(reached(&corpus, "Show Title Part 2"), ["Show Title Part 2"]);
        assert_eq!(
            reached(&corpus, "Show Title"),
            ["Show Title", "Show Title!"]
        );
    }

    #[test]
    fn an_entry_hands_over_its_spellings_in_one_order() {
        // A bucket comes out of a hash map, so what is filed under one entry
        // arrives in no particular order. A stage picking between spellings
        // that score alike would pick differently between two runs.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", ["Zeta Spelling", "Alpha Spelling"]);

        assert_eq!(
            corpus.spellings_of(ShowId(1)),
            ["Alpha Spelling", "Show Title", "Zeta Spelling"]
        );
    }

    #[test]
    fn a_show_answers_the_facts_it_was_filed_with() {
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", Vec::<&str>::new());
        corpus.file(ShowId(2), "Another Show", Vec::<&str>::new());
        let first = Facts {
            episodes: Some(12),
            episode_length: Some(Duration::from_mins(24)),
            year: Some(2019),
            released: true,
            format: None,
        };
        let again = Facts {
            episodes: Some(13),
            ..first
        };

        corpus.file_facts(ShowId(1), first);
        assert_eq!(corpus.facts_of(ShowId(1)), Some(&first));
        corpus.file_facts(ShowId(1), again);
        assert_eq!(corpus.facts_of(ShowId(1)), Some(&again));
        assert_eq!(corpus.facts_of(ShowId(2)), None);
    }

    #[test]
    fn a_spelling_with_no_letters_is_filed_nowhere() {
        // `Season 2` alone names no entry, so it has no key to be filed under
        // and nothing can look it up. The entry's other spellings are filed as
        // usual.
        let mut corpus = Corpus::new();
        corpus.file(ShowId(1), "Show Title", ["Season 2"]);

        assert_eq!(reached(&corpus, "Show Title"), ["Show Title"]);
        assert!(Key::from_title("Season 2").is_none());
    }
}
