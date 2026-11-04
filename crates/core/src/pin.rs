//! Episodes of a show the user said are episodes of an entry.
//!
//! What `benshi map "<title>" ep 14-26 --anilist <id>:1-13` makes: a run of a
//! show's episodes, in the numbering its files carry, and the run of an
//! entry's episodes they are. A group that numbers a second season on from
//! the first, a show numbered by the hundred and an episode the service files
//! as a work of its own each take one pin.

use serde::{Deserialize, Serialize};

use crate::ShowId;
use crate::list::EntryId;

/// A run of episodes, from the first to the last, both included.
///
/// Never empty: a run whose last episode comes before its first is not made,
/// as it is written or as it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Written")]
pub struct Episodes {
    first: u32,
    last: u32,
}

/// A run as it is read, before it is known to be one.
#[derive(Deserialize)]
struct Written {
    first: u32,
    last: u32,
}

impl TryFrom<Written> for Episodes {
    type Error = &'static str;

    fn try_from(written: Written) -> Result<Self, Self::Error> {
        Self::new(written.first, written.last)
            .ok_or("the last episode of a run comes before its first")
    }
}

impl Episodes {
    /// The run from `first` to `last`, and none where `last` comes before
    /// `first`.
    #[must_use]
    pub const fn new(first: u32, last: u32) -> Option<Self> {
        if last < first {
            None
        } else {
            Some(Self { first, last })
        }
    }

    /// The first episode of the run.
    #[must_use]
    pub const fn first(self) -> u32 {
        self.first
    }

    /// The last episode of the run.
    #[must_use]
    pub const fn last(self) -> u32 {
        self.last
    }

    /// How many episodes the run holds, one at the fewest.
    #[must_use]
    pub fn count(self) -> u64 {
        u64::from(self.last - self.first) + 1
    }

    /// Whether the two runs share an episode.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

/// Episodes of a show that are as many episodes of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    show: ShowId,
    episodes: Episodes,
    entry: EntryId,
    entry_episodes: Episodes,
}

/// Why a pin was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unpinnable {
    /// The two runs hold different numbers of episodes, so an episode of one
    /// has no place in the other for the pin to name.
    #[error(
        "the show's {} episodes and the entry's {} are not of one length",
        .episodes.count(),
        .entry_episodes.count()
    )]
    Lengths {
        /// The show's episodes.
        episodes: Episodes,
        /// The entry's episodes.
        entry_episodes: Episodes,
    },
    /// The entry's run begins at episode 0, and an entry's episodes are
    /// numbered from 1.
    #[error("an entry's episodes are numbered from 1")]
    EpisodeZero,
}

impl Pin {
    /// Episodes of `show` that are the episodes `entry_episodes` of `entry`.
    ///
    /// # Errors
    ///
    /// [`Unpinnable`], saying which.
    pub fn new(
        show: ShowId,
        episodes: Episodes,
        entry: EntryId,
        entry_episodes: Episodes,
    ) -> Result<Self, Unpinnable> {
        if episodes.count() != entry_episodes.count() {
            return Err(Unpinnable::Lengths {
                episodes,
                entry_episodes,
            });
        }
        if entry_episodes.first() == 0 {
            return Err(Unpinnable::EpisodeZero);
        }
        Ok(Self {
            show,
            episodes,
            entry,
            entry_episodes,
        })
    }

    /// The show.
    #[must_use]
    pub const fn show(&self) -> ShowId {
        self.show
    }

    /// The show's episodes, in the numbering its files carry.
    #[must_use]
    pub const fn episodes(&self) -> Episodes {
        self.episodes
    }

    /// The entry, by the number the service gives the work.
    #[must_use]
    pub const fn entry(&self) -> EntryId {
        self.entry
    }

    /// The entry's episodes the show's are.
    #[must_use]
    pub const fn entry_episodes(&self) -> Episodes {
        self.entry_episodes
    }
}

#[cfg(test)]
mod tests {
    use super::{Episodes, Pin, Unpinnable};
    use crate::ShowId;
    use crate::list::EntryId;

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run from its first episode to its last")
    }

    #[test]
    fn a_run_holds_its_first_episode_its_last_and_those_between() {
        let episodes = run(14, 26);

        assert_eq!((episodes.first(), episodes.last()), (14, 26));
        assert_eq!(episodes.count(), 13);
        assert_eq!(run(2, 2).count(), 1);
    }

    #[test]
    fn a_run_whose_last_episode_comes_before_its_first_is_none() {
        assert_eq!(Episodes::new(26, 14), None);
    }

    #[test]
    fn runs_that_share_an_episode_overlap_and_no_others() {
        let episodes = run(14, 26);

        for sharing in [
            run(26, 30),
            run(10, 14),
            run(15, 20),
            run(1, 40),
            run(14, 26),
        ] {
            assert!(episodes.overlaps(sharing), "{sharing:?}");
            assert!(sharing.overlaps(episodes), "{sharing:?}");
        }
        for apart in [run(27, 30), run(1, 13)] {
            assert!(!episodes.overlaps(apart), "{apart:?}");
            assert!(!apart.overlaps(episodes), "{apart:?}");
        }
    }

    #[test]
    fn a_run_reads_back_as_it_was_written() {
        let written = serde_json::to_string(&run(14, 26)).expect("serialize");

        assert_eq!(written, r#"{"first":14,"last":26}"#);
        assert_eq!(
            serde_json::from_str::<Episodes>(&written).expect("deserialize"),
            run(14, 26)
        );
    }

    #[test]
    fn a_run_whose_last_episode_comes_before_its_first_is_refused_as_it_is_read() {
        assert!(serde_json::from_str::<Episodes>(r#"{"first":26,"last":14}"#).is_err());
    }

    #[test]
    fn a_pin_holds_episodes_of_a_show_as_as_many_episodes_of_an_entry() {
        let pin = Pin::new(ShowId(1), run(14, 26), EntryId(12345), run(1, 13))
            .expect("runs of one length");

        assert_eq!(pin.show(), ShowId(1));
        assert_eq!(pin.episodes(), run(14, 26));
        assert_eq!(pin.entry(), EntryId(12345));
        assert_eq!(pin.entry_episodes(), run(1, 13));
    }

    #[test]
    fn a_pin_of_runs_of_two_lengths_is_refused() {
        assert_eq!(
            Pin::new(ShowId(1), run(14, 26), EntryId(12345), run(1, 12)),
            Err(Unpinnable::Lengths {
                episodes: run(14, 26),
                entry_episodes: run(1, 12),
            })
        );
    }

    #[test]
    fn a_pin_to_an_episode_zero_of_an_entry_is_refused() {
        // An episode of a show may be 0, a special the release numbers so.
        assert!(Pin::new(ShowId(1), run(0, 0), EntryId(12345), run(1, 1)).is_ok());
        assert_eq!(
            Pin::new(ShowId(1), run(1, 13), EntryId(12345), run(0, 12)),
            Err(Unpinnable::EpisodeZero)
        );
    }
}
