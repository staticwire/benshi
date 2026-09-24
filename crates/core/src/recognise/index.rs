//! The entries a name's words reach, for a stage that decides nothing.
//!
//! Stage three narrows the corpus to a handful and hands them to the scorer. It
//! answers no name by itself: the claim it makes is that the right entry is
//! among the few it returns, and stage four is what picks one.
//!
//! **It is what a name that misses the exact key still reaches.** A key hits or
//! misses whole, so a name the parser shortened, a name carrying a subtitle the
//! list does not spell, and a name spelled a way the list does not carry all
//! miss it. Their words do not miss.
//!
//! **The words are read by the key's own rules** through
//! [`super::normalise::words_in`], so everything a key forgives is gone from
//! them as well and this stage repeats none of it. The words cannot be read out
//! of a key, because a key erases the breaks between words so that `Fate/Zero`
//! and `Fate Zero` reach one.
//!
//! **A word that too many entries carry narrows nothing and is dropped.** The
//! crowded words are the joins of Japanese and English titles rather than
//! names: in a list of five thousand entries, `no` is carried by 1542 and `the`
//! by 1616, so searching by either answers a third of the list.
//!
//! **A candidate shares two of the name's words**, or the only word there was.
//! One is not enough, because a word that narrows can still reach as many
//! entries as [`CROWDED`] allows.
//!
//! **Where a name reaches nothing here, the parser usually left it nothing.**
//! Of the filenames built from a list of five thousand entries, 622 miss the
//! exact key and 613 of those reach their entry here. The nine that do not are
//! names the parser reduced to `03`, `;` or `√A`, by reading an abbreviation as
//! an episode number or by dropping the text it could not place. Nothing this
//! stage does can find a title in `;`.

use std::collections::HashMap;

use crate::recognise::corpus::Corpus;
use crate::recognise::normalise::words_in;

/// A word narrows nothing once more than one entry in this many carries it.
///
/// A share rather than a count, because the list is the user's and its size is
/// not known here: what makes a word useless is the part of the list it
/// answers, and one count is a different part of every list.
///
/// Measured over the five thousand most popular AniList entries and the 49,428
/// filenames built from their spellings. The right entry is among the
/// candidates for 99.98 names in a hundred, half of all names reach it alone,
/// and 99 in a hundred reach 28 candidates or fewer. Without the ceiling the
/// median name reaches 78 and one name in ten reaches over 1700, which is the
/// list again rather than a handful.
pub const CROWDED: usize = 200;

/// How many of a name's words a candidate carries, where the name has that
/// many words that narrow anything.
///
/// One is not enough. A word narrows while no more than one entry in
/// [`CROWDED`] carries it, so a candidate set built on one shared word is the
/// union of what the name's words reach.
pub const SHARED: usize = 2;

/// The entries each word reaches.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Every entry, once, in whatever order the corpus hands its spellings
    /// over.
    entries: Vec<String>,
    /// Which entries a word reaches, by their place in `entries`.
    reached: HashMap<String, Vec<usize>>,
}

impl Index {
    /// The index of a corpus, built from the same spellings it files.
    ///
    /// Built from the corpus rather than beside it, so that the two stages
    /// cannot be given different lists.
    #[must_use]
    pub fn of(corpus: &Corpus) -> Self {
        let mut index = Self::default();
        let mut at = HashMap::new();
        for (spelled, title) in corpus.spellings() {
            let entry = *at.entry(title).or_insert_with(|| {
                index.entries.push(title.to_owned());
                index.entries.len() - 1
            });
            for word in words_in(spelled) {
                let reaches = index.reached.entry(word).or_default();
                if !reaches.contains(&entry) {
                    reaches.push(entry);
                }
            }
        }
        index
    }

    /// The entries a name reaches, by title, in one order.
    ///
    /// Sorted rather than left as they were found: a corpus files its entries
    /// into buckets by key, so the order they come back in is the order of a
    /// hash map, and one name would answer differently between two runs.
    ///
    /// Empty where the name spells no word the corpus knows, which is not a
    /// refusal: this stage decides nothing, and a name it cannot narrow is a
    /// name the scorer is not asked about.
    #[must_use]
    pub fn candidates(&self, spelled: &str) -> Vec<&str> {
        let asked = self.narrowing(&words_in(spelled));
        let mut shared: HashMap<usize, usize> = HashMap::new();
        for reaches in &asked {
            for &entry in *reaches {
                *shared.entry(entry).or_default() += 1;
            }
        }

        let wanted = SHARED.min(asked.len());
        let mut found: Vec<&str> = shared
            .into_iter()
            .filter(|&(_, carried)| carried >= wanted)
            .map(|(entry, _)| self.entries[entry].as_str())
            .collect();
        found.sort_unstable();
        found
    }

    /// The entries reached by each word that narrows anything.
    ///
    /// Where every word the corpus knows is crowded, the least crowded of them
    /// are used anyway: they narrow badly, and dropping them would lose the
    /// name altogether.
    fn narrowing(&self, words: &[String]) -> Vec<&[usize]> {
        let known: Vec<&[usize]> = words
            .iter()
            .filter_map(|word| self.reached.get(word).map(Vec::as_slice))
            .collect();
        let ceiling = (self.entries.len() / CROWDED).max(1);
        let uncrowded: Vec<&[usize]> = known
            .iter()
            .copied()
            .filter(|reaches| reaches.len() <= ceiling)
            .collect();
        if !uncrowded.is_empty() {
            return uncrowded;
        }

        let least = known.iter().map(|reaches| reaches.len()).min();
        known
            .into_iter()
            .filter(|reaches| Some(reaches.len()) == least)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Index;
    use crate::path::RawPath;
    use crate::recognise::by_key;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::parse::parse;

    /// A list big enough for a share of it to mean anything: four hundred
    /// entries, every one of them spelling `The` and `Show`.
    fn a_long_list() -> Corpus {
        (0..400).map(|n| format!("The Show {n}")).collect()
    }

    #[test]
    fn a_name_reaches_its_own_entry_among_a_handful() {
        // Both halves of what this stage claims, and neither alone is worth
        // anything: an index answering the whole corpus passes the first, and
        // one answering nothing passes the second.
        let corpus = a_long_list();
        let index = Index::of(&corpus);

        let found = index.candidates("The Show 137");

        assert!(
            found.contains(&"The Show 137"),
            "the right entry is among them"
        );
        assert!(found.len() < 10, "and they are a handful: {}", found.len());
    }

    #[test]
    fn a_word_too_many_entries_carry_does_not_narrow() {
        // `The` and `Show` are carried by all four hundred, so searching by
        // them answers the whole list. The number that survives is what makes
        // the answer a handful.
        let corpus = a_long_list();
        let index = Index::of(&corpus);

        assert_eq!(index.candidates("The Show 137"), ["The Show 137"]);
    }

    #[test]
    fn a_name_of_nothing_but_crowded_words_reaches_them_all() {
        // The honest answer where there is nothing to narrow by. Dropping
        // every word would answer nothing and lose the name altogether, so the
        // least crowded of them are used and the scorer is handed what they
        // reach.
        let corpus = a_long_list();
        let index = Index::of(&corpus);

        let found = index.candidates("The Show");

        assert!(found.len() > 100, "nothing here narrows: {}", found.len());
        assert!(found.contains(&"The Show 137"));
    }

    #[test]
    fn an_entry_sharing_one_word_of_several_is_not_a_candidate() {
        // Two shared words, or the name is not about that entry. `Fate` alone
        // is shared by every entry of the franchise and says nothing about
        // which.
        let corpus: Corpus = ["Fate Zero", "Fate Apocrypha", "Zero Kara Hajimeru"]
            .into_iter()
            .collect();
        let index = Index::of(&corpus);

        assert_eq!(index.candidates("Fate Zero"), ["Fate Zero"]);
    }

    #[test]
    fn a_name_of_one_word_is_answered_by_that_word_alone() {
        // Two shared words cannot be asked of a name that spells one, and a
        // rule that asked anyway would answer nothing for every single-word
        // title on the list.
        let corpus: Corpus = ["Monster", "Another", "Bleach"].into_iter().collect();
        let index = Index::of(&corpus);

        assert_eq!(index.candidates("Monster"), ["Monster"]);
    }

    #[test]
    fn a_name_the_exact_key_misses_still_reaches_its_entry() {
        // What this stage exists for. The file carries a subtitle its list
        // entry does not spell, so the key it reaches is not the entry's key
        // and stage two answers nothing at all.
        let corpus: Corpus = [
            "Shingeki no Kyojin",
            "Shingeki no Kyojin Season 2",
            "Kimi no Na wa",
        ]
        .into_iter()
        .collect();
        let index = Index::of(&corpus);
        let file = parse(&RawPath::from_bytes(
            b"[Group] Shingeki no Kyojin - The Final Season - 03 [1080p].mkv".to_vec(),
        ));

        assert_eq!(by_key(&file, &corpus), None, "the exact key misses it");
        assert!(
            index
                .candidates(file.title.as_deref().expect("the name spells a title"))
                .contains(&"Shingeki no Kyojin")
        );
    }

    #[test]
    fn an_entry_spelled_two_ways_is_one_candidate() {
        // As the corpus does it: an entry reached through two of its spellings
        // is one entry, not two. The last name spells both at once, the way a
        // release carrying the romaji title and the English one does, and a
        // second entry would answer it with the title twice.
        let mut corpus = Corpus::new();
        corpus.file("Shingeki no Kyojin", ["Attack on Titan"]);
        let index = Index::of(&corpus);

        assert_eq!(index.candidates("Attack on Titan"), ["Shingeki no Kyojin"]);
        assert_eq!(
            index.candidates("Shingeki no Kyojin"),
            ["Shingeki no Kyojin"]
        );
        assert_eq!(
            index.candidates("Shingeki no Kyojin (Attack on Titan)"),
            ["Shingeki no Kyojin"]
        );
    }

    #[test]
    fn a_word_one_entry_spells_twice_counts_once_for_it() {
        // An entry filed under two spellings that share a word would otherwise
        // be credited with that word twice: the word looks more crowded than it
        // is, and the entry looks to share more of the name's words than it
        // spells.
        let mut corpus: Corpus = ["Beta Gamma"].into_iter().collect();
        corpus.file("Alpha Show", ["Show Alpha"]);
        let index = Index::of(&corpus);

        assert!(index.candidates("Show Beta").is_empty());
    }

    #[test]
    fn candidates_come_back_in_one_order_however_they_were_filed() {
        // A corpus files into buckets by key, so what comes back from it is in
        // the order of a hash map. Left alone, one name would answer in a
        // different order between two runs of the same program.
        //
        // Six entries rather than three, because three of them come back in
        // sorted order by chance about one run in six, and a test that passes
        // one run in six against a sort that is not there proves nothing. Every
        // word here is carried by four of the six, so every candidate survives
        // and all six are ordered.
        let corpus: Corpus = [
            "Beta Gamma",
            "Gamma Alpha",
            "Alpha Beta",
            "Beta Alpha",
            "Gamma Beta",
            "Alpha Gamma",
        ]
        .into_iter()
        .collect();
        let index = Index::of(&corpus);

        assert_eq!(
            index.candidates("Alpha Beta Gamma"),
            [
                "Alpha Beta",
                "Alpha Gamma",
                "Beta Alpha",
                "Beta Gamma",
                "Gamma Alpha",
                "Gamma Beta"
            ]
        );
    }

    #[test]
    fn a_name_of_words_no_entry_spells_reaches_nothing() {
        // Not a refusal. This stage decides nothing, so a name it cannot place
        // is a name the scorer is not asked about.
        let corpus: Corpus = ["Monster", "Another"].into_iter().collect();
        let index = Index::of(&corpus);

        assert!(index.candidates("Some Other Show").is_empty());
        assert!(index.candidates("").is_empty());
    }
}
