//! Whether a file is of the show recognition named, or of which of several,
//! by what the list holds of each show's entry.
//!
//! Recognition reads what a path spells and nothing else, so it answers a
//! trailer of a work as the work, and a name that spells no episode as the
//! whole of a series. The list knows more of each entry than any name says: whether the
//! work has been released, the year it began, how many episodes it has and
//! how long one is. [`settle`] stands between recognition and the session and
//! asks those facts of the answer.
//!
//! **Five facts refuse a file, asked in this order, and the first that
//! refuses is the one an explanation names:**
//!
//! - a work that has not been released takes no file;
//! - a year the name spells and the work did not begin in refuses it, unless a
//!   title of the work carries the same number, which is then the title's;
//! - a name that spells a film or an extra, `Movie` or `OVA`, is no episode of
//!   a series broadcast on television;
//! - a name that spells no episode is the one episode of a work of one, and
//!   names nothing of a work of several;
//! - a file shorter than half of one episode of the work is not an episode.
//!
//! **Of several shows, the facts choose one where they refuse every other.**
//! A name that reaches several entries is asked the same five facts of each,
//! with the episode the name spells, and one more: an episode past the count
//! of a show takes that show out, so the fifth episode of a title a film and
//! a series share is the series'. Where one is left, it is the answer;
//! where several or none are, the name stays unsettled, and the shows the
//! facts refused are named with the fact that refused each.
//!
//! **A fact that is not known refuses nothing.** A work filed with no facts at
//! all, a year one side does not say, a count the service does not hold and a
//! length either side does not report each leave the answer as recognition
//! gave it. A fact is asked only where both sides of it are known.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::list::{Entry, Format};
use crate::recognize::corpus::Corpus;
use crate::recognize::parse::{Episode, Kind, Parsed};
use crate::recognize::{Ambiguity, Recognition};
use crate::timeline::duration_of;
use crate::{Known, PlayerSnapshot, ShowId};

/// What a list holds of a work that settles whether a file is of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// How many episodes the work has, and none where the service holds no
    /// count.
    pub episodes: Option<u32>,
    /// How long one episode is, and none where the service does not say.
    pub episode_length: Option<Duration>,
    /// The year the work began, and none where the service does not say.
    pub year: Option<u16>,
    /// Whether any of the work has been released.
    pub released: bool,
    /// The format the service lists the work in, and none where it does not
    /// say.
    pub format: Option<Format>,
}

impl From<&Entry> for Facts {
    fn from(entry: &Entry) -> Self {
        Self {
            episodes: entry.episodes,
            episode_length: entry.episode_length,
            year: entry.year,
            released: entry.released,
            format: entry.format,
        }
    }
}

/// The fact of an entry that refused a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fact {
    /// The work has not been released.
    NotReleased,
    /// The name spells a year the work did not begin in.
    Year {
        /// The year the name spells.
        spelled: u32,
        /// The year the work began.
        began: u16,
    },
    /// The name spells a film or an extra, and the work is a series broadcast
    /// on television.
    Kind(Kind),
    /// The name spells no episode, and the work has more than one.
    Episodes(u32),
    /// The name spells an episode past the count of the work.
    ///
    /// Asked only among several shows. A show recognition named alone keeps
    /// an episode past its count, because a release can number a later
    /// season on from the first.
    PastCount {
        /// The episode the name spells.
        episode: u32,
        /// How many episodes the work has.
        episodes: u32,
    },
    /// The file is shorter than half of one episode of the work.
    Length {
        /// How long the file is.
        file: Duration,
        /// How long one episode of the work is.
        episode: Duration,
    },
}

/// What the facts of the entries made of what recognition answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// No fact of the entry refused the show recognition named.
    Stands {
        /// The show recognition named: the one the corpus filed the entry as,
        /// or the one kept for a name the list does not hold.
        show: ShowId,
        /// The episode in the release's numbering, as it was read from the
        /// name.
        episode: Episode,
    },
    /// A fact of the entry refused the show recognition named.
    Refused {
        /// The show recognition named.
        show: ShowId,
        /// The fact that refused it, the first of the five that did.
        fact: Fact,
    },
    /// Of several shows recognition named, the facts refused all but this
    /// one, and the file is of it.
    Chosen {
        /// The show the facts let stand.
        show: ShowId,
        /// The episode in the release's numbering, as it was read from the
        /// name.
        episode: Episode,
        /// Every other show recognition named, with the fact that refused
        /// it, in the order recognition named them.
        refused: Vec<(ShowId, Fact)>,
    },
    /// Of several shows recognition named, the facts refused these, and left
    /// none or more than one.
    Undecided {
        /// The shows the facts refused, with the fact that refused each, in
        /// the order recognition named them.
        refused: Vec<(ShowId, Fact)>,
    },
    /// Recognition named no show, or several that the facts refused none
    /// of, so there was nothing to settle.
    Unnamed,
}

/// Ask the facts of the entry recognition named whether the file is of it,
/// or the facts of each of several which one it is of.
///
/// `spelled` is the parse the answer was decided by, and `reading` the
/// reading of the file, whose length is the file's where its own position
/// does not contradict it. The facts are those `corpus` holds of each show.
#[must_use]
pub fn settle(
    answer: &Recognition,
    spelled: &Parsed,
    reading: &PlayerSnapshot,
    corpus: &Corpus,
) -> Settled {
    let found = match answer {
        Recognition::Recognized(found) => found,
        Recognition::Ambiguous(several) => return choose(several, spelled, reading, corpus),
        Recognition::Unrecognized(_) => return Settled::Unnamed,
    };
    let year = year_against(corpus, found.show, spelled.year);
    let refused = corpus.facts_of(found.show).and_then(|facts| {
        refusal(
            facts,
            year,
            spelled.kind,
            found.episode,
            duration_of(reading),
        )
    });

    match refused {
        Some(fact) => Settled::Refused {
            show: found.show,
            fact,
        },
        None => Settled::Stands {
            show: found.show,
            episode: found.episode,
        },
    }
}

/// Of several shows recognition named, the one the facts let stand where
/// they refuse every other.
fn choose(
    several: &Ambiguity,
    spelled: &Parsed,
    reading: &PlayerSnapshot,
    corpus: &Corpus,
) -> Settled {
    let length = duration_of(reading);
    let mut left = Vec::new();
    let mut refused = Vec::new();
    for candidate in &several.candidates {
        let year = year_against(corpus, candidate.show, spelled.year);
        let fact = corpus.facts_of(candidate.show).and_then(|facts| {
            refusal(facts, year, spelled.kind, spelled.episode, length)
                .or_else(|| past_count(facts, spelled.episode))
        });
        match fact {
            Some(fact) => refused.push((candidate.show, fact)),
            None => left.push(candidate.show),
        }
    }

    if refused.is_empty() {
        return Settled::Unnamed;
    }

    match left.as_slice() {
        [show] => Settled::Chosen {
            show: *show,
            episode: spelled.episode,
            refused,
        },
        _ => Settled::Undecided { refused },
    }
}

/// An episode the name spells past the count of a show, which takes one of
/// several shows out, and none where the show holds it or its count is not
/// known.
fn past_count(facts: &Facts, episode: Episode) -> Option<Fact> {
    if let Episode::Only(episode) = episode
        && let Some(episodes) = facts.episodes
        && episode > episodes
    {
        return Some(Fact::PastCount { episode, episodes });
    }

    None
}

/// The year a name spells, as a year to ask of a show, and none where a
/// title of the show carries the same number.
fn year_against(corpus: &Corpus, show: ShowId, year: Option<u32>) -> Option<u32> {
    year.filter(|&year| !corpus.spells_number(show, year))
}

/// The first of the five facts that refuses a file, and none where none
/// does.
///
/// `year` is the one the name spells where no title of the show carries it,
/// `kind` the one the name spells, `episode` the name's episode, and
/// `length` the file's.
fn refusal(
    facts: &Facts,
    year: Option<u32>,
    kind: Option<Kind>,
    episode: Episode,
    length: Known<Duration>,
) -> Option<Fact> {
    if !facts.released {
        return Some(Fact::NotReleased);
    }
    if let (Some(spelled), Some(began)) = (year, facts.year)
        && spelled != u32::from(began)
    {
        return Some(Fact::Year { spelled, began });
    }
    if let Some(kind) = kind
        && matches!(facts.format, Some(Format::Tv | Format::TvShort))
    {
        return Some(Fact::Kind(kind));
    }
    if episode == Episode::Absent
        && let Some(episodes) = facts.episodes
        && episodes > 1
    {
        return Some(Fact::Episodes(episodes));
    }
    // A length so long that twice it overflows is no short file.
    if let (Known::Value(file), Some(one)) = (length, facts.episode_length)
        && file.checked_mul(2).is_some_and(|twice| twice < one)
    {
        return Some(Fact::Length { file, episode: one });
    }

    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Fact, Facts, Settled, settle};
    use crate::clock::Timestamp;
    use crate::list::{Entry, EntryId, Format, Status};
    use crate::path::RawPath;
    use crate::recognize::altname::Altnames;
    use crate::recognize::corpus::Corpus;
    use crate::recognize::index::Index;
    use crate::recognize::parse::{Episode, Kind, Parsed, parse};
    use crate::recognize::{Ambiguity, Candidate, Recognition, Refusal, decide};
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, ShowId};

    /// The show every test files its one entry as.
    const SHOW: ShowId = ShowId(1);

    /// One episode of the works these tests file, where a test needs one.
    const EPISODE: Duration = Duration::from_mins(24);

    /// The facts of a series of twelve episodes of 24 minutes, released,
    /// begun in 2019, which every rule lets a file of an episode of through.
    fn a_series() -> Facts {
        Facts {
            episodes: Some(12),
            episode_length: Some(EPISODE),
            year: Some(2019),
            released: true,
            format: None,
        }
    }

    /// What a file of this name spells.
    fn named(name: &str) -> Parsed {
        parse(&RawPath::from_bytes(name.as_bytes().to_vec()))
    }

    /// A reading of a file that is this long, at no position.
    fn a_file(length: Known<Duration>) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/a file.mkv".to_vec())),
            state: PlayState::Playing,
            position: Known::NotReported,
            duration: length,
            observed_at: Timestamp::epoch(),
        }
    }

    /// A corpus of one entry, `Show Title`, filed as [`SHOW`] with these
    /// facts.
    fn listed(facts: Facts) -> Corpus {
        let mut corpus = Corpus::new();
        corpus.file(SHOW, "Show Title", Vec::<&str>::new());
        corpus.file_facts(SHOW, facts);
        corpus
    }

    /// What the facts of `corpus` make of a file of this name and length,
    /// after recognition decided it against the same corpus.
    ///
    /// Recognition is asked here rather than written out, so a name that does
    /// not reach the entry settles as [`Settled::Unnamed`] and never passes as
    /// refused.
    fn settled(name: &str, length: Known<Duration>, corpus: &Corpus) -> Settled {
        let spelled = named(name);
        let answer = decide(&spelled, &Altnames::new(), corpus, &Index::of(corpus));
        settle(&answer, &spelled, &a_file(length), corpus)
    }

    /// A length of this many seconds, as a reading reports one.
    fn seconds(of: u64) -> Known<Duration> {
        Known::Value(Duration::from_secs(of))
    }

    #[test]
    fn a_file_of_an_episode_stands_as_recognition_named_it() {
        assert_eq!(
            settled(
                "[Group] Show Title - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(a_series())
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn a_work_not_yet_released_takes_no_file() {
        // A name can reach an entry the service lists as not yet released,
        // a sequel announced under the title of the series. No file is of
        // it, whatever the name spells.
        let announced = Facts {
            released: false,
            ..a_series()
        };

        assert_eq!(
            settled(
                "[Group] Show Title - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(announced)
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::NotReleased,
            }
        );
    }

    #[test]
    fn a_year_the_work_did_not_begin_in_refuses_the_file() {
        // The key drops a year, so a remake reaches the entry of its
        // original. The year the name spells is the one fact that tells them
        // apart, and the entry began in another.
        let spelled = named("[Group] Show Title (2001) - 03 [1080p].mkv");
        assert_eq!(spelled.year, Some(2001));

        assert_eq!(
            settled(
                "[Group] Show Title (2001) - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(a_series())
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Year {
                    spelled: 2001,
                    began: 2019,
                },
            }
        );
    }

    #[test]
    fn the_year_the_work_began_lets_the_file_through() {
        assert_eq!(
            settled(
                "[Group] Show Title (2019) - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(a_series())
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn a_year_a_title_of_the_work_spells_refuses_nothing() {
        // The number is the title's: a work can be named for a year it did
        // not begin in.
        let mut corpus = Corpus::new();
        corpus.file(SHOW, "Show Title 2032", Vec::<&str>::new());
        corpus.file_facts(
            SHOW,
            Facts {
                year: Some(1987),
                ..a_series()
            },
        );
        let name = "[Group] Show Title 2032 - 03 [1080p].mkv";
        assert_eq!(named(name).year, Some(2032));

        assert_eq!(
            settled(name, Known::Value(EPISODE), &corpus),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn a_year_another_spelling_of_the_work_carries_refuses_nothing() {
        let mut corpus = Corpus::new();
        corpus.file(SHOW, "Show Title", ["Other Title (1987 Release Title)"]);
        corpus.file_facts(
            SHOW,
            Facts {
                episodes: Some(1),
                year: Some(1984),
                ..a_series()
            },
        );
        let name = "Other Title (1987 Release Title).mkv";
        assert_eq!(named(name).year, Some(1987));

        assert_eq!(
            settled(name, Known::Value(EPISODE), &corpus),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Absent,
            }
        );
    }

    #[test]
    fn a_name_that_spells_an_extra_or_a_film_refuses_a_series() {
        let series = Facts {
            format: Some(Format::Tv),
            ..a_series()
        };
        for (name, kind) in [
            ("[Group] Show Title OVA - 01 [1080p].mkv", Kind::Extra),
            ("[Group] Show Title Movie [1080p].mkv", Kind::Film),
        ] {
            assert_eq!(
                settled(name, Known::NotReported, &listed(series)),
                Settled::Refused {
                    show: SHOW,
                    fact: Fact::Kind(kind),
                },
                "{name}"
            );
        }
    }

    #[test]
    fn a_type_word_of_an_episode_title_refuses_no_series() {
        let series = Facts {
            format: Some(Format::Tv),
            ..a_series()
        };

        assert_eq!(
            settled(
                "[Group] Show Title - 07 - Special Training [1080p].mkv",
                Known::NotReported,
                &listed(series)
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(7),
            }
        );
    }

    #[test]
    fn the_kind_is_asked_after_the_release_and_the_year() {
        let series = Facts {
            format: Some(Format::Tv),
            ..a_series()
        };
        for (facts, fact) in [
            (
                Facts {
                    released: false,
                    ..series
                },
                Fact::NotReleased,
            ),
            (
                series,
                Fact::Year {
                    spelled: 2001,
                    began: 2019,
                },
            ),
        ] {
            assert_eq!(
                settled(
                    "[Group] Show Title OVA (2001) [1080p].mkv",
                    Known::NotReported,
                    &listed(facts)
                ),
                Settled::Refused { show: SHOW, fact },
                "{fact:?}"
            );
        }
    }

    #[test]
    fn a_name_that_spells_an_extra_stands_for_an_ova_a_special_an_ona_and_an_unknown_format() {
        for format in [
            Some(Format::Ova),
            Some(Format::Special),
            Some(Format::Ona),
            None,
        ] {
            assert_eq!(
                settled(
                    "[Group] Show Title OVA - 01 [1080p].mkv",
                    Known::NotReported,
                    &listed(Facts {
                        format,
                        ..a_series()
                    })
                ),
                Settled::Stands {
                    show: SHOW,
                    episode: Episode::Only(1),
                },
                "{format:?}"
            );
        }
    }

    #[test]
    fn a_short_series_is_a_series_to_a_name_that_spells_an_extra() {
        assert_eq!(
            settled(
                "[Group] Show Title OVA - 01 [1080p].mkv",
                Known::NotReported,
                &listed(Facts {
                    format: Some(Format::TvShort),
                    ..a_series()
                })
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Kind(Kind::Extra),
            }
        );
    }

    #[test]
    fn a_year_one_side_does_not_say_refuses_nothing() {
        let undated = Facts {
            year: None,
            ..a_series()
        };

        assert_eq!(
            settled(
                "[Group] Show Title (2001) - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(undated)
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
        assert_eq!(named("[Group] Show Title - 03 [1080p].mkv").year, None);
        assert_eq!(
            settled(
                "[Group] Show Title - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(a_series())
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn a_name_with_no_episode_names_nothing_of_an_ova_of_two() {
        // A name that spells no episode does not say which of the two
        // episodes it is.
        let ova = Facts {
            episodes: Some(2),
            episode_length: Some(Duration::from_mins(40)),
            year: None,
            released: true,
            format: None,
        };
        assert_eq!(
            named("[Group] Show Title OVA [1080p].mkv").episode,
            Episode::Absent
        );

        assert_eq!(
            settled(
                "[Group] Show Title OVA [1080p].mkv",
                Known::Value(Duration::from_mins(40)),
                &listed(ova)
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Episodes(2),
            }
        );
    }

    #[test]
    fn a_name_with_no_episode_is_the_one_episode_of_a_work_of_one() {
        // A film spells no episode, and the release's numbering keeps it
        // that way: it stands with none rather than as the first.
        let film = Facts {
            episodes: Some(1),
            episode_length: Some(Duration::from_mins(95)),
            year: Some(2019),
            released: true,
            format: None,
        };

        assert_eq!(
            settled(
                "[Group] Show Title (2019) [1080p].mkv",
                seconds(95 * 60),
                &listed(film)
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Absent,
            }
        );
    }

    #[test]
    fn a_count_the_service_does_not_hold_refuses_no_name_with_no_episode() {
        let uncounted = Facts {
            episodes: None,
            ..a_series()
        };

        assert_eq!(
            settled(
                "[Group] Show Title [1080p].mkv",
                Known::Value(EPISODE),
                &listed(uncounted)
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Absent,
            }
        );
    }

    #[test]
    fn a_name_that_spells_an_episode_is_not_asked_the_count() {
        // The count refuses a name with no episode alone, so an episode past
        // the count stands.
        let short = Facts {
            episodes: Some(2),
            ..a_series()
        };

        assert_eq!(
            settled(
                "[Group] Show Title - 25 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(short)
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(25),
            }
        );
    }

    #[test]
    fn a_trailer_is_not_the_episode_of_its_work() {
        // The case the length is asked for: a trailer of 1.9 minutes beside
        // an OVA of one episode of 40, which its name reaches and which a
        // name with no episode names whole.
        let ova = Facts {
            episodes: Some(1),
            episode_length: Some(Duration::from_mins(40)),
            year: None,
            released: true,
            format: None,
        };

        assert_eq!(
            settled(
                "[Group] Show Title OVA [1080p].mkv",
                seconds(114),
                &listed(ova)
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Length {
                    file: Duration::from_secs(114),
                    episode: Duration::from_mins(40),
                },
            }
        );
    }

    #[test]
    fn a_length_too_long_to_double_is_no_short_file() {
        // A length read from a recording has no bound, and twice the longest
        // does not fit.
        assert_eq!(
            settled(
                "[Group] Show Title - 03 [1080p].mkv",
                Known::Value(Duration::MAX),
                &listed(a_series())
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn files_near_the_length_of_an_episode_stay_episodes() {
        // A file of an episode runs a little under or over the length the
        // list holds for its entry, and a file of exactly half is not shorter
        // than half.
        for fraction in [0.94, 1.0, 1.04, 0.5] {
            let length = EPISODE.mul_f64(fraction);

            assert_eq!(
                settled(
                    "[Group] Show Title - 03 [1080p].mkv",
                    Known::Value(length),
                    &listed(a_series())
                ),
                Settled::Stands {
                    show: SHOW,
                    episode: Episode::Only(3),
                },
                "{fraction}"
            );
        }
    }

    #[test]
    fn a_file_a_second_short_of_half_an_episode_is_refused() {
        let length = Duration::from_secs(EPISODE.as_secs() / 2 - 1);

        assert_eq!(
            settled(
                "[Group] Show Title - 03 [1080p].mkv",
                Known::Value(length),
                &listed(a_series())
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Length {
                    file: length,
                    episode: EPISODE,
                },
            }
        );
    }

    #[test]
    fn a_length_either_side_does_not_report_refuses_nothing() {
        let unmeasured = Facts {
            episode_length: None,
            ..a_series()
        };
        let stands = Settled::Stands {
            show: SHOW,
            episode: Episode::Only(3),
        };
        let name = "[Group] Show Title - 03 [1080p].mkv";

        assert_eq!(settled(name, seconds(60), &listed(unmeasured)), stands);
        for length in [Known::NotReported, Known::Unsupported] {
            assert_eq!(
                settled(name, length, &listed(a_series())),
                stands,
                "{length:?}"
            );
        }
    }

    #[test]
    fn a_length_its_own_position_contradicts_is_not_the_files() {
        // A length below the position it arrives with describes media
        // playback has run past, so it says nothing of how long the file is.
        let spelled = named("[Group] Show Title - 03 [1080p].mkv");
        let corpus = listed(a_series());
        let answer = decide(&spelled, &Altnames::new(), &corpus, &Index::of(&corpus));
        let reading = PlayerSnapshot {
            position: Known::Value(Duration::from_mins(20)),
            ..a_file(seconds(60))
        };

        assert_eq!(
            settle(&answer, &spelled, &reading, &corpus),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(3),
            }
        );
    }

    #[test]
    fn the_first_fact_that_refuses_is_the_one_named() {
        // Not released, another year, no episode of several and a file far
        // too short, all at once: the facts are asked in that order.
        let everything_wrong = Facts {
            episodes: Some(12),
            episode_length: Some(EPISODE),
            year: Some(2019),
            released: false,
            format: None,
        };
        let name = "[Group] Show Title (2001) [1080p].mkv";

        assert_eq!(
            settled(name, seconds(60), &listed(everything_wrong)),
            Settled::Refused {
                show: SHOW,
                fact: Fact::NotReleased,
            }
        );
        assert_eq!(
            settled(name, seconds(60), &listed(a_series())),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Year {
                    spelled: 2001,
                    began: 2019,
                },
            }
        );
        assert_eq!(
            settled(
                "[Group] Show Title [1080p].mkv",
                seconds(60),
                &listed(a_series())
            ),
            Settled::Refused {
                show: SHOW,
                fact: Fact::Episodes(12),
            }
        );
    }

    #[test]
    fn a_show_filed_with_no_facts_stands_whatever_the_file() {
        let corpus: Corpus = ["Show Title"].into_iter().collect();

        assert_eq!(
            settled("[Group] Show Title (2001) [1080p].mkv", seconds(1), &corpus),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Absent,
            }
        );
    }

    #[test]
    fn a_name_recognition_refused_or_found_twice_settles_nothing() {
        // The facts settle the show recognition named, and these named none.
        let corpus = listed(a_series());
        let spelled = named("[Group] Show Title - 03 [1080p].mkv");
        let refused = Recognition::Unrecognized(Refusal {
            parsed: "Show Title".to_owned(),
            best: None,
        });
        let several = Recognition::Ambiguous(Ambiguity {
            parsed: "Show Title".to_owned(),
            candidates: vec![
                Candidate {
                    show: SHOW,
                    title: "Show Title".to_owned(),
                },
                Candidate {
                    show: ShowId(2),
                    title: "Show Title".to_owned(),
                },
            ],
        });

        for answer in [refused, several] {
            assert_eq!(
                settle(&answer, &spelled, &a_file(Known::Value(EPISODE)), &corpus),
                Settled::Unnamed,
                "{answer:?}"
            );
        }
    }

    /// A corpus of these entries, filed as shows numbered from one in the
    /// order given, each with its facts where it has any.
    fn listed_as(entries: &[(&str, Option<Facts>)]) -> Corpus {
        let mut corpus = Corpus::new();
        for ((title, facts), show) in entries.iter().zip(1..) {
            corpus.file(ShowId(show), title, Vec::<&str>::new());
            if let Some(facts) = facts {
                corpus.file_facts(ShowId(show), *facts);
            }
        }
        corpus
    }

    /// A film begun in this year, released or not.
    fn a_film(year: u16, released: bool) -> Facts {
        Facts {
            episodes: Some(1),
            episode_length: Some(Duration::from_mins(124)),
            year: Some(year),
            released,
            format: None,
        }
    }

    #[test]
    fn of_two_entries_of_one_title_the_one_released_is_the_answer() {
        // Two films of one title, and the service lists the second as not
        // yet released. The name reaches both, and only the first can be
        // what is playing.
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(1988, true))),
            ("Show Title", Some(a_film(2027, false))),
        ]);
        let name = "Show Title (1988).mkv";
        assert!(matches!(
            decide(&named(name), &Altnames::new(), &corpus, &Index::of(&corpus)),
            Recognition::Ambiguous(_)
        ));

        assert_eq!(
            settled(name, seconds(124 * 60), &corpus),
            Settled::Chosen {
                show: ShowId(1),
                episode: Episode::Absent,
                refused: vec![(ShowId(2), Fact::NotReleased)],
            }
        );
    }

    #[test]
    fn a_year_chooses_between_a_remake_and_its_original() {
        // The key drops the year, so the name reaches both, and the year it
        // spells is the one fact that tells them apart.
        let corpus = listed_as(&[
            (
                "Show Title",
                Some(Facts {
                    year: Some(2001),
                    ..a_series()
                }),
            ),
            ("Show Title (2019)", Some(a_series())),
        ]);

        assert_eq!(
            settled(
                "[Group] Show Title (2019) - 05 [1080p].mkv",
                Known::Value(EPISODE),
                &corpus
            ),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(5),
                refused: vec![(
                    ShowId(1),
                    Fact::Year {
                        spelled: 2019,
                        began: 2001,
                    }
                )],
            }
        );
    }

    #[test]
    fn a_length_chooses_between_two_works_of_one_title() {
        // An OVA of one episode of 40 minutes and a film of 124, both named
        // with no episode: a file of 40 minutes is under half the film.
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(2019, true))),
            (
                "Show Title",
                Some(Facts {
                    episode_length: Some(Duration::from_mins(40)),
                    ..a_film(2019, true)
                }),
            ),
        ]);

        assert_eq!(
            settled("Show Title.mkv", seconds(40 * 60), &corpus),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Absent,
                refused: vec![(
                    ShowId(1),
                    Fact::Length {
                        file: Duration::from_mins(40),
                        episode: Duration::from_mins(124),
                    }
                )],
            }
        );
    }

    #[test]
    fn a_show_with_no_facts_can_be_the_one_left() {
        // A fact that is not known refuses nothing, so a show filed with none
        // stands beside one the facts refused.
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(2027, false))),
            ("Show Title", None),
        ]);

        assert_eq!(
            settled("Show Title.mkv", seconds(124 * 60), &corpus),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Absent,
                refused: vec![(ShowId(1), Fact::NotReleased)],
            }
        );
    }

    #[test]
    fn shows_the_facts_refuse_every_one_of_leave_nothing() {
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(2027, false))),
            (
                "Show Title",
                Some(Facts {
                    episodes: Some(2),
                    ..a_film(1988, true)
                }),
            ),
        ]);

        assert_eq!(
            settled("Show Title.mkv", seconds(124 * 60), &corpus),
            Settled::Undecided {
                refused: vec![
                    (ShowId(1), Fact::NotReleased),
                    (ShowId(2), Fact::Episodes(2)),
                ],
            }
        );
    }

    #[test]
    fn shows_the_facts_refuse_some_of_leave_the_rest_undecided() {
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(1988, true))),
            ("Show Title", Some(a_film(2027, false))),
            ("Show Title", Some(a_film(1988, true))),
        ]);

        assert_eq!(
            settled("Show Title.mkv", seconds(124 * 60), &corpus),
            Settled::Undecided {
                refused: vec![(ShowId(2), Fact::NotReleased)],
            }
        );
    }

    #[test]
    fn an_episode_past_a_candidate_s_count_takes_it_out() {
        // A film and a series of one title: the fifth episode is the
        // series'. No length is known, so the film's length refuses nothing.
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(1983, true))),
            (
                "Show Title",
                Some(Facts {
                    episodes: Some(50),
                    ..a_series()
                }),
            ),
        ]);

        assert_eq!(
            settled("Show Title - 05.mkv", Known::NotReported, &corpus),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(5),
                refused: vec![(
                    ShowId(1),
                    Fact::PastCount {
                        episode: 5,
                        episodes: 1,
                    }
                )],
            }
        );
    }

    #[test]
    fn of_several_shows_a_fact_of_the_five_is_named_before_the_count() {
        // The film is refused by its length and by its count, and the length
        // is asked first.
        let corpus = listed_as(&[
            ("Show Title", Some(a_film(1983, true))),
            (
                "Show Title",
                Some(Facts {
                    episodes: Some(50),
                    ..a_series()
                }),
            ),
        ]);

        assert_eq!(
            settled("Show Title - 05.mkv", Known::Value(EPISODE), &corpus),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(5),
                refused: vec![(
                    ShowId(1),
                    Fact::Length {
                        file: EPISODE,
                        episode: Duration::from_mins(124),
                    }
                )],
            }
        );
    }

    #[test]
    fn the_season_that_holds_the_episode_is_chosen_over_one_too_short() {
        let corpus = listed_as(&[
            ("Show Title", Some(a_series())),
            (
                "Show Title",
                Some(Facts {
                    episodes: Some(13),
                    ..a_series()
                }),
            ),
        ]);

        assert_eq!(
            settled(
                "[Group] Show Title - 13 [1080p].mkv",
                Known::Value(EPISODE),
                &corpus
            ),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(13),
                refused: vec![(
                    ShowId(1),
                    Fact::PastCount {
                        episode: 13,
                        episodes: 12,
                    }
                )],
            }
        );
    }

    #[test]
    fn a_candidate_with_no_count_holds_any_episode() {
        let corpus = listed_as(&[
            (
                "Show Title",
                Some(Facts {
                    episodes: None,
                    ..a_series()
                }),
            ),
            ("Show Title", Some(a_series())),
        ]);

        assert_eq!(
            settled(
                "[Group] Show Title - 13 [1080p].mkv",
                Known::Value(EPISODE),
                &corpus
            ),
            Settled::Chosen {
                show: ShowId(1),
                episode: Episode::Only(13),
                refused: vec![(
                    ShowId(2),
                    Fact::PastCount {
                        episode: 13,
                        episodes: 12,
                    }
                )],
            }
        );
    }

    #[test]
    fn an_episode_past_the_count_of_the_one_entry_named_is_not_asked() {
        // A release can number a later season on from the first.
        assert_eq!(
            settled(
                "[Group] Show Title - 13 [1080p].mkv",
                Known::Value(EPISODE),
                &listed(a_series())
            ),
            Settled::Stands {
                show: SHOW,
                episode: Episode::Only(13),
            }
        );
    }

    #[test]
    fn of_a_series_and_its_ova_a_name_that_spells_ova_is_the_ova() {
        let corpus = listed_as(&[
            (
                "Show Title",
                Some(Facts {
                    format: Some(Format::Tv),
                    ..a_series()
                }),
            ),
            (
                "Show Title (OVA)",
                Some(Facts {
                    episodes: Some(2),
                    format: Some(Format::Ova),
                    ..a_series()
                }),
            ),
        ]);

        assert_eq!(
            settled(
                "[Group] Show Title OVA - 01 [1080p].mkv",
                Known::NotReported,
                &corpus
            ),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(1),
                refused: vec![(ShowId(1), Fact::Kind(Kind::Extra))],
            }
        );
    }

    #[test]
    fn a_year_a_candidate_s_title_carries_is_not_asked_of_it() {
        // Two entries of one title, one not yet released. The number in the
        // name is the title's, so it refuses neither, and the facts choose.
        let corpus = listed_as(&[
            (
                "Show Title 2032",
                Some(Facts {
                    year: Some(1987),
                    ..a_series()
                }),
            ),
            (
                "Show Title 2032",
                Some(Facts {
                    released: false,
                    ..a_series()
                }),
            ),
        ]);

        assert_eq!(
            settled(
                "[Group] Show Title 2032 - 03 [1080p].mkv",
                Known::Value(EPISODE),
                &corpus
            ),
            Settled::Chosen {
                show: ShowId(1),
                episode: Episode::Only(3),
                refused: vec![(ShowId(2), Fact::NotReleased)],
            }
        );
    }

    #[test]
    fn the_facts_of_an_entry_are_the_list_s() {
        let entry = Entry {
            id: EntryId(900_001),
            title: "Show Title".to_owned(),
            spellings: Vec::new(),
            format: Some(Format::Ova),
            episodes: Some(2),
            episode_length: Some(Duration::from_mins(40)),
            year: Some(2011),
            released: false,
            status: Status::Planned,
            progress: 0,
            rewatches: 0,
            score: None,
        };

        assert_eq!(
            Facts::from(&entry),
            Facts {
                episodes: Some(2),
                episode_length: Some(Duration::from_mins(40)),
                year: Some(2011),
                released: false,
                format: Some(Format::Ova),
            }
        );
    }
}
