//! The entry a name means because the user said so.
//!
//! Stage one, consulted before any heuristic, and the user overriding the
//! program rather than helping it. What it holds is what they typed: an
//! assignment keeps its own text, and the key is an index derived from it.
//! Deriving a key throws text away, and nothing turns one back into the text it
//! came from. A key kept in the text's place would leave nothing to show the
//! user, and nothing for a corrected rule of the key to be applied to.
//!
//! **An assignment is checked against the corpus while it is being made**,
//! which is the one moment the user can still do something about it. Three
//! things are refused, each named by [`Unnameable`]: text that spells no
//! letters, a title no entry carries, and text that already means one entry of
//! its own. The last is the dangerous one. An assignment names a *spelling*,
//! not a file, so every file spelling that way follows it: text already
//! reaching `Mushoku Tensei: Jobless Reincarnation Cour 2` and pointed at the
//! special that extends the same name sends every file of the cour to the
//! special, until somebody takes the assignment back.
//!
//! **Text reaching several entries is not refused**, because there the corpus
//! answers nothing: `Nisekoi` and `Nisekoi:` are two entries a filename cannot
//! tell apart, and naming one of them by hand is how a person settles what the
//! text cannot. Text reaching nothing is not refused either, and is what this
//! stage is for: an abbreviation a filename carries and no entry spells.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::recognise::corpus::Corpus;
use crate::recognise::normalise::Key;
use crate::recognise::parse::Parsed;

/// One assignment, as the user made it.
///
/// The text, and what its key holds that the text does not. An assignment made
/// from a file takes its season and its part from the parser, which has
/// already cut them out of the title, so the text alone does not tell a reader
/// why a file of another season does not follow it. The three together do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assignment {
    /// The text they gave, kept verbatim so that it can be shown back to them
    /// and written to a store.
    pub spelled: String,
    /// The season the assignment is bound to, where its key holds one.
    pub season: Option<u32>,
    /// The part the assignment is bound to, where its key holds one.
    pub part: Option<u32>,
    /// The entry it names, as the corpus spells it.
    pub title: String,
}

/// What an assignment did, for the command that made it to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    /// What was recorded.
    pub recorded: Assignment,
    /// The assignment this one displaced, where the text was already named.
    ///
    /// Reported rather than silently overwritten: a user who names the same
    /// text twice has changed their mind, and the one they are leaving is the
    /// one they would have to remember to put back.
    pub replaced: Option<Assignment>,
}

/// Why an assignment was not made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unnameable {
    /// The text spells no letters, so nothing could ever be looked up by it.
    #[error("`{spelled}` spells no letters to look anything up by")]
    Unspellable {
        /// The text that was given.
        spelled: String,
    },
    /// No entry in the corpus carries that title.
    #[error("no entry is titled `{title}`")]
    NotOnTheList {
        /// The title that was named.
        title: String,
    },
    /// The text already means one entry, and it is not the one being named.
    ///
    /// Refused rather than allowed with a warning, because what it costs is
    /// every file of the entry it takes from, and a warning is read once while
    /// the assignment answers every file after it.
    #[error("`{spelled}` already means `{entry}`, whose files it would take")]
    AlreadyMeans {
        /// The text that was given.
        spelled: String,
        /// The entry that text reaches today.
        entry: String,
    },
}

/// The entries the user has named, looked up before any heuristic.
#[derive(Debug, Clone, Default)]
pub struct Altnames {
    named: HashMap<Key, Assignment>,
}

impl Altnames {
    /// Nothing named yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Name the entry a piece of text means.
    ///
    /// # Errors
    ///
    /// [`Unnameable`], and the refusal says which of the three it is. Each is a
    /// thing the user can still correct.
    pub fn name(
        &mut self,
        spelled: &str,
        title: &str,
        corpus: &Corpus,
    ) -> Result<Named, Unnameable> {
        let key = Key::from_title(spelled).ok_or_else(|| Unnameable::Unspellable {
            spelled: spelled.to_owned(),
        })?;
        self.record(key, spelled.to_owned(), title, corpus)
    }

    /// Name the entry a file means, from what that file spelled.
    ///
    /// The way to name something without typing it: both sides of the
    /// comparison then come from the parser, so a season or a part the parser
    /// took out of the title is in the key here as well. Text typed by hand
    /// carries neither unless it spells them, and an assignment that lost the
    /// season misses the file it was made for and catches the first season
    /// instead.
    ///
    /// # Errors
    ///
    /// [`Unnameable`], as [`name`](Altnames::name).
    pub fn name_as_parsed(
        &mut self,
        parsed: &Parsed,
        title: &str,
        corpus: &Corpus,
    ) -> Result<Named, Unnameable> {
        let spelled = parsed.title.clone().unwrap_or_default();
        let key = Key::from_parsed(parsed).ok_or_else(|| Unnameable::Unspellable {
            spelled: spelled.clone(),
        })?;
        self.record(key, spelled, title, corpus)
    }

    /// File one assignment, once the corpus has been asked about it.
    fn record(
        &mut self,
        key: Key,
        spelled: String,
        title: &str,
        corpus: &Corpus,
    ) -> Result<Named, Unnameable> {
        // An entry answers to its own title, because the corpus files every
        // entry under the key of the title as well as under its other
        // spellings. A title its own key does not reach is a title no entry
        // carries.
        let carried = Key::from_title(title)
            .is_some_and(|of_title| corpus.candidates(&of_title).contains(&title));
        if !carried {
            return Err(Unnameable::NotOnTheList {
                title: title.to_owned(),
            });
        }
        if let [already] = corpus.candidates(&key).as_slice()
            && *already != title
        {
            return Err(Unnameable::AlreadyMeans {
                spelled,
                entry: (*already).to_owned(),
            });
        }

        let recorded = Assignment {
            spelled,
            season: key.season(),
            part: key.part(),
            title: title.to_owned(),
        };
        let replaced = self.named.insert(key, recorded.clone());
        Ok(Named { recorded, replaced })
    }

    /// Undo an assignment, and answer the one that was undone.
    ///
    /// Nothing where that text names nothing. Without this an assignment made
    /// in error outranks every heuristic for as long as the program runs.
    pub fn forget(&mut self, spelled: &str) -> Option<Assignment> {
        self.named.remove(&Key::from_title(spelled)?)
    }

    /// Every assignment, for a store to write and an explanation to print.
    pub fn assignments(&self) -> impl Iterator<Item = &Assignment> {
        self.named.values()
    }

    /// The assignment a key was given by hand, where one was.
    pub(super) fn assigned(&self, key: &Key) -> Option<&Assignment> {
        self.named.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::{Altnames, Assignment, Unnameable};
    use crate::path::RawPath;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::normalise::Key;
    use crate::recognise::parse::{Parsed, parse};

    /// What a file of this name spells.
    fn named(name: &str) -> Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    #[test]
    fn text_that_reaches_no_entry_is_named_freely() {
        // What this stage is for: an abbreviation no entry spells, so it
        // reaches nothing and naming it takes nothing away.
        let corpus: Corpus = ["Shingeki no Kyojin"].into_iter().collect();
        let mut altnames = Altnames::new();

        let named = altnames
            .name("SnK", "Shingeki no Kyojin", &corpus)
            .expect("a text nothing else reaches is free to name");

        assert_eq!(
            named.recorded,
            Assignment {
                spelled: "SnK".to_owned(),
                season: None,
                part: None,
                title: "Shingeki no Kyojin".to_owned(),
            }
        );
        assert_eq!(named.replaced, None);
    }

    #[test]
    fn an_assignment_made_from_a_file_carries_the_season_the_file_spelled() {
        // The text alone cannot recreate the key it was filed under: the
        // parser took `S03` out of the title, so the season is in the key and
        // nowhere in the text. It travels on the assignment, where a store
        // writing it down and an explanation showing it back can both read
        // it, and a reader of either can see why a first-season file does not
        // follow it.
        let corpus: Corpus = [
            "Kaguya-sama: Love is War",
            "Kaguya-sama: Love is War -Ultra Romantic-",
        ]
        .into_iter()
        .collect();
        let third = named("Kaguya-sama.Love.is.War.S03E03.1080p.WEB-DL.mkv");
        let mut altnames = Altnames::new();

        let named = altnames
            .name_as_parsed(&third, "Kaguya-sama: Love is War -Ultra Romantic-", &corpus)
            .expect("the third season reaches no entry, so naming it takes nothing");

        assert_eq!(
            named.recorded,
            Assignment {
                spelled: "Kaguya-sama Love is War".to_owned(),
                season: Some(3),
                part: None,
                title: "Kaguya-sama: Love is War -Ultra Romantic-".to_owned(),
            }
        );
    }

    #[test]
    fn an_assignment_made_from_a_file_carries_the_part_the_file_spelled() {
        // The part is the other thing a key holds that the text does not,
        // and it is read back from after the season marker the parser cut.
        let corpus: Corpus = ["Show Title", "Show Title Part 2"].into_iter().collect();
        let second = named("[Group] Show Title (Season 1 Part 2) - 03.mkv");
        let mut altnames = Altnames::new();

        let named = altnames
            .name_as_parsed(&second, "Show Title Part 2", &corpus)
            .expect("the file's key reaches the part it names, so naming it takes nothing");

        assert_eq!(named.recorded.spelled, "Show Title");
        assert_eq!(named.recorded.season, None);
        assert_eq!(named.recorded.part, Some(2));
    }

    #[test]
    fn text_that_already_means_one_entry_is_refused_and_the_entry_is_named() {
        // The assignment that looks perfect and takes a whole cour with it.
        // Both halves are spellings from the user's own list, so nothing about
        // the text is suspicious: `Cour 2` reaches the cour, and pointing it at
        // the special that extends the same name sends the cour's files to the
        // special.
        let corpus: Corpus = [
            "Mushoku Tensei: Jobless Reincarnation",
            "Mushoku Tensei: Jobless Reincarnation Cour 2",
            "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer",
        ]
        .into_iter()
        .collect();
        let mut altnames = Altnames::new();

        let refused = altnames.name(
            "Mushoku Tensei: Jobless Reincarnation Cour 2",
            "Mushoku Tensei: Jobless Reincarnation Cour 2 - Eris the Goblin Slayer",
            &corpus,
        );

        assert_eq!(
            refused,
            Err(Unnameable::AlreadyMeans {
                spelled: "Mushoku Tensei: Jobless Reincarnation Cour 2".to_owned(),
                entry: "Mushoku Tensei: Jobless Reincarnation Cour 2".to_owned(),
            })
        );
        assert!(altnames.assignments().next().is_none());
    }

    #[test]
    fn text_that_means_several_entries_may_be_settled_by_hand() {
        // The other side of the same rule. Here the corpus answers nothing at
        // all - two entries a filename cannot tell apart - so naming one takes
        // nothing from anybody and is how a person settles what the text
        // cannot.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();
        let mut altnames = Altnames::new();

        assert!(altnames.name("Nisekoi", "Nisekoi:", &corpus).is_ok());
        assert_eq!(
            altnames
                .assigned(&Key::from_title("Nisekoi").unwrap())
                .map(|assignment| assignment.title.as_str()),
            Some("Nisekoi:")
        );
    }

    #[test]
    fn a_title_no_entry_carries_cannot_be_named() {
        // A typo, or an English title the user's list does not carry. Caught
        // while they are still at the terminal, rather than recorded as a
        // title that outranks every heuristic and resolves to nothing.
        let corpus: Corpus = ["Nisekoi", "Nisekoi:"].into_iter().collect();
        let mut altnames = Altnames::new();

        assert_eq!(
            altnames.name("Nisekoi", "False Love", &corpus),
            Err(Unnameable::NotOnTheList {
                title: "False Love".to_owned(),
            })
        );
    }

    #[test]
    fn text_with_no_letters_cannot_be_named() {
        // `Season 2` and `P4` are a season and a part and nothing else, so
        // neither has a key. Answering nothing would drop the user's own
        // command without a word.
        let corpus: Corpus = ["Show Title"].into_iter().collect();
        let mut altnames = Altnames::new();

        assert_eq!(
            altnames.name("Season 2", "Show Title", &corpus),
            Err(Unnameable::Unspellable {
                spelled: "Season 2".to_owned(),
            })
        );
        assert!(matches!(
            altnames.name("P4", "Show Title", &corpus),
            Err(Unnameable::Unspellable { .. })
        ));
    }

    #[test]
    fn naming_one_text_twice_replaces_the_first_and_says_which() {
        let corpus: Corpus = ["Show Title", "Another Show"].into_iter().collect();
        let mut altnames = Altnames::new();

        altnames
            .name("SnK", "Show Title", &corpus)
            .expect("free to name");
        let again = altnames
            .name("SnK", "Another Show", &corpus)
            .expect("the user may change their mind");

        assert_eq!(
            again.replaced,
            Some(Assignment {
                spelled: "SnK".to_owned(),
                season: None,
                part: None,
                title: "Show Title".to_owned(),
            })
        );
        assert_eq!(altnames.assignments().count(), 1);
    }

    #[test]
    fn an_assignment_can_be_taken_back() {
        // An assignment outranks every heuristic, so one made in error is
        // permanent unless it can be undone.
        let corpus: Corpus = ["Show Title"].into_iter().collect();
        let mut altnames = Altnames::new();
        altnames
            .name("SnK", "Show Title", &corpus)
            .expect("free to name");

        let forgotten = altnames.forget("SnK");

        assert_eq!(
            forgotten.map(|assignment| assignment.title),
            Some("Show Title".to_owned())
        );
        assert!(altnames.forget("SnK").is_none());
        assert!(altnames.assignments().next().is_none());
    }

    #[test]
    fn naming_what_a_file_spelled_catches_that_file_and_not_the_season_before_it() {
        // Typing the title out of the file's name is not enough, and this is
        // the case that proves it. The parser takes `S03` out of the title, so
        // text typed as `Kaguya-sama Love is War` has no season in its key: it
        // misses the file it was made for and catches the first season, where
        // the corpus was already right. Naming what the file spelled keeps the
        // season, because both sides then come from the parser.
        let corpus: Corpus = [
            "Kaguya-sama: Love is War",
            "Kaguya-sama: Love is War -Ultra Romantic-",
        ]
        .into_iter()
        .collect();
        let third = named("Kaguya-sama.Love.is.War.S03E03.1080p.WEB-DL.mkv");
        let first = named("Kaguya-sama.Love.is.War.S01E03.1080p.WEB-DL.mkv");
        let mut altnames = Altnames::new();

        altnames
            .name_as_parsed(&third, "Kaguya-sama: Love is War -Ultra Romantic-", &corpus)
            .expect("the third season reaches no entry, so naming it takes nothing");

        assert_eq!(
            altnames
                .assigned(&Key::from_parsed(&third).unwrap())
                .map(|assignment| assignment.title.as_str()),
            Some("Kaguya-sama: Love is War -Ultra Romantic-")
        );
        assert!(
            altnames
                .assigned(&Key::from_parsed(&first).unwrap())
                .is_none()
        );
    }

    #[test]
    fn an_assignment_keeps_the_text_the_user_gave() {
        // The key is an index and not a store: several spellings reach one,
        // and nothing turns a key back into the text it was built from, so the
        // text is the only thing a correction to the key's rules can be
        // applied to again.
        let corpus: Corpus = ["Show Title"].into_iter().collect();
        let mut altnames = Altnames::new();

        altnames
            .name("ＳＨＯＷ　ＴＩＴＬＥ！", "Show Title", &corpus)
            .expect("free to name");

        assert_eq!(
            altnames.assignments().next().map(|a| a.spelled.as_str()),
            Some("ＳＨＯＷ　ＴＩＴＬＥ！")
        );
    }
}
