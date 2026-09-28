//! What the daemon decided a file was, kept for `benshi why`.
//!
//! Recognition is pure logic in `benshi-core` and decides against a corpus the
//! caller supplies. A [`Recogniser`] holds one, with its index and the names
//! the user gave. Against [`Recogniser::empty`] every name reaches the end of
//! the sequence and is refused. A round decides, the decision is left here,
//! and a client reads it.
//!
//! One decision is kept, the most recent, the way
//! [`Seen`](crate::detection::Seen) keeps the most recent listing and for the
//! same reason: an explanation is of what the daemon acted on, and a second
//! look would be free to disagree with it.

use std::sync::{Arc, RwLock};

use benshi_core::path::RawPath;
use benshi_core::recognise::altname::Altnames;
use benshi_core::recognise::corpus::Corpus;
use benshi_core::recognise::index::Index;
use benshi_core::recognise::parse::{Parsed, parse};
use benshi_core::recognise::{Recognition, decide};
use benshi_core::{MediaRef, PlayerId};

/// What is said when the record of the last decision cannot be trusted.
const POISONED: &str = "the last decision was poisoned by a panic in another task";

/// What a name is decided against: the corpus, its index and the assignments.
#[derive(Debug)]
pub struct Recogniser {
    corpus: Corpus,
    index: Index,
    altnames: Altnames,
}

impl Recogniser {
    /// Nothing to match against: an empty corpus and no assignments.
    ///
    /// Every name is refused with nothing scored.
    #[must_use]
    pub fn empty() -> Self {
        Self::of(Corpus::default(), Altnames::new())
    }

    /// A list to match against, and the names the user gave.
    ///
    /// The index is built here from the list, so the two cannot disagree.
    #[must_use]
    pub fn of(corpus: Corpus, altnames: Altnames) -> Self {
        let index = Index::of(&corpus);
        Self {
            corpus,
            index,
            altnames,
        }
    }

    /// What a file's name spells, and what recognition made of it.
    ///
    /// The name alone. A directory is not part of what a release spells, and a
    /// folder named after another show would otherwise carry that show's words
    /// into the name the stages read.
    #[must_use]
    pub fn decide(&self, path: &RawPath) -> (Parsed, Recognition) {
        let parsed = parse(&RawPath::from_bytes(path.file_name().to_vec()));
        let answer = decide(&parsed, &self.altnames, &self.corpus, &self.index);

        (parsed, answer)
    }
}

/// One decision: whose file, which file, what its name spelled, and the answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The source whose reading was decided.
    pub player: PlayerId,
    /// What that source had open.
    pub media: MediaRef,
    /// What the name spelled, before anything was matched.
    pub parsed: Parsed,
    /// What recognition answered.
    pub answer: Recognition,
}

/// The most recent decision, for a client asking why.
///
/// Written by the detection task for every reading it admits that names a
/// file, and read by a client asking `benshi why`. Cloning shares the record
/// rather than copying it.
#[derive(Debug, Clone, Default)]
pub struct Decided(Arc<RwLock<Option<Decision>>>);

impl Decided {
    /// A record of nothing, which is what is true before the first reading.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the record with this decision.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the record, for the reason
    /// [`Seen::record`](crate::detection::Seen::record) gives.
    pub fn record(&self, decision: Decision) {
        *self.0.write().expect(POISONED) = Some(decision);
    }

    /// The most recent decision, and none before a reading has been decided.
    ///
    /// # Panics
    ///
    /// If a panic elsewhere poisoned the record, for the same reason.
    #[must_use]
    pub fn latest(&self) -> Option<Decision> {
        self.0.read().expect(POISONED).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{Decided, Decision, Recogniser};
    use benshi_core::path::RawPath;
    use benshi_core::recognise::altname::Altnames;
    use benshi_core::recognise::corpus::Corpus;
    use benshi_core::recognise::parse::Episode;
    use benshi_core::recognise::{Match, Recognition, Refusal, Stage};
    use benshi_core::{MediaRef, PlayerId};

    fn a_path(name: &str) -> RawPath {
        RawPath::from_bytes(name.as_bytes().to_vec())
    }

    /// A list of these titles, each spelled one way.
    fn a_list_of(titles: &[&str]) -> Corpus {
        titles.iter().collect()
    }

    #[test]
    fn nothing_is_decided_before_the_first_reading() {
        assert!(Decided::new().latest().is_none());
    }

    #[test]
    fn a_decision_is_kept_where_a_client_can_read_it_and_the_next_replaces_it() {
        // `benshi why` explains the most recent decision, so the record holds
        // one and the next reading's decision takes its place.
        let decided = Decided::new();
        let recogniser = Recogniser::empty();
        let first = a_path("/anime/[Group] Show Title - 03.mkv");
        let second = a_path("/anime/[Group] Another Show - 07.mkv");

        for path in [&first, &second] {
            let (parsed, answer) = recogniser.decide(path);
            decided.record(Decision {
                player: PlayerId("mpv".to_owned()),
                media: MediaRef::LocalFile(path.clone()),
                parsed,
                answer,
            });
        }

        let kept = decided.latest().expect("a decision was recorded");
        assert_eq!(kept.media, MediaRef::LocalFile(second));
        assert_eq!(kept.parsed.title.as_deref(), Some("Another Show"));
    }

    #[test]
    fn a_name_is_decided_by_the_file_and_not_by_its_directory() {
        // A directory is not part of what a release spells. A file kept under
        // a folder named after another show would otherwise carry that show's
        // words into the name the stages read.
        let (parsed, _answer) =
            Recogniser::empty().decide(&a_path("/anime/Other Show/[Group] Show Title - 03.mkv"));

        assert_eq!(parsed.title.as_deref(), Some("Show Title"));
        assert_eq!(parsed.episode, Episode::Only(3));
    }

    #[test]
    fn a_name_the_list_holds_is_recognised_as_the_list_spells_it() {
        let recogniser = Recogniser::of(a_list_of(&["Show Title", "A Film"]), Altnames::new());

        let (_parsed, answer) =
            recogniser.decide(&a_path("/anime/[Group] Show Title - 03 [1080p].mkv"));

        assert_eq!(
            answer,
            Recognition::Recognised(Match {
                title: "Show Title".to_owned(),
                episode: Episode::Only(3),
                stage: Stage::Key,
            })
        );
    }

    #[test]
    fn a_name_the_user_gave_is_read_before_the_list() {
        let list = a_list_of(&["Show Title", "Another Show"]);
        let mut named = Altnames::new();
        named
            .name("ShoTi", "Show Title", &list)
            .expect("text that reaches no entry can be named");
        let recogniser = Recogniser::of(list, named);

        let (_parsed, answer) = recogniser.decide(&a_path("/anime/[Group] ShoTi - 03.mkv"));

        assert!(
            matches!(
                &answer,
                Recognition::Recognised(Match {
                    title,
                    episode: Episode::Only(3),
                    stage: Stage::Altname(_),
                }) if title == "Show Title"
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_name_the_key_misses_is_found_through_an_index_of_the_list() {
        // The name spells more than either title, so no key reaches it, and
        // the scorer is handed the candidates the index narrowed the list to.
        let recogniser = Recogniser::of(
            a_list_of(&["Show Title", "Show Title Cour 2 The Subtitle"]),
            Altnames::new(),
        );

        let (_parsed, answer) =
            recogniser.decide(&a_path("/anime/Show Title Cour 2 - The Subtitle - 03.mkv"));

        assert!(
            matches!(
                &answer,
                Recognition::Recognised(Match {
                    title,
                    episode: Episode::Only(3),
                    stage: Stage::Scored(_),
                }) if title == "Show Title Cour 2 The Subtitle"
            ),
            "got {answer:?}"
        );
    }

    #[test]
    fn with_nothing_to_match_against_every_name_is_refused() {
        // With nothing to match against, a refusal with nothing scored is the
        // honest answer rather than a guess. It is what an explanation has to
        // show for every file.
        let (_parsed, answer) = Recogniser::empty().decide(&a_path("[Group] Show Title - 03.mkv"));

        assert!(
            matches!(
                answer,
                Recognition::Unrecognised(Refusal { best: None, .. })
            ),
            "got {answer:?}"
        );
    }
}
