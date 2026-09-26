//! What the daemon decided a file was, kept for `benshi why`.
//!
//! Recognition is pure logic in `benshi-core` and decides against a corpus the
//! caller supplies. Until a list arrives there is nothing to supply, so the
//! [`Recogniser`] here holds an empty corpus and no assignments, and every name
//! reaches the end of the sequence and is refused. The plumbing is what exists:
//! a round decides, the decision is left here, and a client reads it.
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
    /// Every name is refused with nothing scored. A list arriving changes what
    /// this holds and not what a round does with it.
    #[must_use]
    pub fn empty() -> Self {
        let corpus = Corpus::default();
        let index = Index::of(&corpus);
        Self {
            corpus,
            index,
            altnames: Altnames::new(),
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
    use benshi_core::recognise::parse::Episode;
    use benshi_core::recognise::{Recognition, Refusal};
    use benshi_core::{MediaRef, PlayerId};

    fn a_path(name: &str) -> RawPath {
        RawPath::from_bytes(name.as_bytes().to_vec())
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
    fn with_nothing_to_match_against_every_name_is_refused() {
        // Until a list arrives there is nothing to match against, and a
        // refusal with nothing scored is the honest answer rather than a
        // guess. It is what an explanation has to show for every file.
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
