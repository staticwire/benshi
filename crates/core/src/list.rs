//! What a list holds.
//!
//! A list is the entries of one account on one service. An entry is two
//! things at once: a work as the service knows it, and where the account
//! stands on it.

use std::time::Duration;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

/// An entry, by the number the service gives the work: `Media.id` on
/// AniList.
///
/// The number of the work and not of the account's entry for it, so it is
/// the same on every account and outlives the entry leaving a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub i64);

/// One entry of a list, as the service gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The work.
    pub id: EntryId,
    /// The title a person is shown.
    pub title: String,
    /// Every other spelling the service gives of the title.
    pub spellings: Vec<String>,
    /// What kind of release the work is, and none where the service does
    /// not say or names one this build does not know.
    pub format: Option<Format>,
    /// How many episodes the work has, and none where the service holds no
    /// count.
    pub episodes: Option<u32>,
    /// How long one episode is, and none where the service does not say.
    pub episode_length: Option<Duration>,
    /// The year the work began, and none where the service does not say.
    pub year: Option<u16>,
    /// Whether any of the work has been released.
    pub released: bool,
    /// Where the account stands on the work.
    pub status: Status,
    /// How many episodes the account has seen.
    pub progress: u32,
    /// How many times the account has seen the work again.
    pub rewatches: u32,
    /// The account's score, and none where it gave none.
    pub score: Option<Rating>,
}

/// One entry of a list with the days the account began and completed the
/// work.
///
/// Readings of one work hold both days, as a write carries them. Readings of
/// the whole list leave the days out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dated {
    /// The entry.
    pub entry: Entry,
    /// The day the account began the work, where the list holds one.
    pub started: Option<Day>,
    /// The day the account completed the work, where the list holds one.
    pub completed: Option<Day>,
}

/// Where an account stands on one work, as a write tells a list.
///
/// The score is not in it: a write leaves the score as the list holds it.
/// Both days are, and a day the write does not carry is none on the list
/// once it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Write {
    /// The work.
    pub id: EntryId,
    /// Where the account stands on it.
    pub status: Status,
    /// How many episodes the account has seen.
    pub progress: u32,
    /// How many times the account has seen the work again.
    pub rewatches: u32,
    /// The day the account began the work.
    pub started: Option<Day>,
    /// The day the account completed the work.
    pub completed: Option<Day>,
}

/// Calendar day, as lists keep the day an entry was begun or completed.
///
/// Lists can keep the year of a day alone, or its year and month, as AniList
/// keeps a day given so. Days are written back as they were read, so that a
/// write leaves such a day as the account gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Day {
    /// The year.
    pub year: u16,
    /// The month, from 1 to 12, where the list keeps one.
    pub month: Option<u8>,
    /// The day of the month, from 1 to 31, where the list keeps one.
    pub day: Option<u8>,
}

impl Day {
    /// The day named by its year, its month and its day of the month.
    #[must_use]
    pub const fn new(year: u16, month: u8, day: u8) -> Self {
        Self {
            year,
            month: Some(month),
            day: Some(day),
        }
    }
}

/// What kind of release a work is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// `TV` on AniList.
    Tv,
    /// `TV_SHORT` on AniList.
    TvShort,
    /// `MOVIE` on AniList.
    Film,
    /// `SPECIAL` on AniList.
    Special,
    /// `OVA` on AniList.
    Ova,
    /// `ONA` on AniList.
    Ona,
    /// `MUSIC` on AniList.
    Music,
}

/// Where a show stands on a list.
///
/// `Rewatching` is a status here because it is one on AniList and on
/// Shikimori. MyAnimeList and Kitsu keep it as a flag beside the status. The
/// other five are on all four lists, which name them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Meant to be watched and not begun.
    Planned,
    /// Being watched.
    Watching,
    /// Being watched again.
    Rewatching,
    /// Watched to the end.
    Completed,
    /// Set aside, to be taken up again.
    OnHold,
    /// Given up.
    Dropped,
}

/// A score in whole points, from 1 to 100.
///
/// On AniList each user chooses the format a score is shown in, and the list
/// takes and answers a score in a hundred points whichever was chosen.
/// MyAnimeList and Shikimori score out of ten and Kitsu out of twenty, and
/// both multiply into a hundred exactly. Zero is not a rating: an entry
/// with no score carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rating(u8);

impl Rating {
    /// A rating, or nothing where the points are not from 1 to 100.
    #[must_use]
    pub fn new(points: u8) -> Option<Self> {
        (1..=100).contains(&points).then_some(Self(points))
    }

    /// The points, from 1 to 100.
    #[must_use]
    pub const fn points(self) -> u8 {
        self.0
    }
}

/// Read through [`Rating::new`], so that a score out of range is refused
/// where a derived implementation would let it in.
impl<'de> Deserialize<'de> for Rating {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let points = u8::deserialize(deserializer)?;
        Self::new(points).ok_or_else(|| {
            D::Error::custom(format_args!(
                "rating is from 1 to 100, and this is {points}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Day, EntryId, Rating, Status, Write};

    #[test]
    fn a_rating_is_from_one_to_a_hundred() {
        // Zero is no rating, and no rating is `None`: two ways to say it
        // would be one too many.
        assert_eq!(Rating::new(0), None);
        assert_eq!(Rating::new(1).map(Rating::points), Some(1));
        assert_eq!(Rating::new(100).map(Rating::points), Some(100));
        assert_eq!(Rating::new(101), None);
    }

    #[test]
    fn a_rating_read_out_of_range_is_refused_with_its_points() {
        for points in [0, 101] {
            let refused = serde_json::from_str::<Rating>(&points.to_string())
                .expect_err("no rating")
                .to_string();

            assert_eq!(
                refused,
                format!("rating is from 1 to 100, and this is {points}")
            );
        }
        assert_eq!(
            serde_json::from_str::<Rating>("80")
                .map(Rating::points)
                .ok(),
            Some(80)
        );
    }

    #[test]
    fn a_write_reads_back_as_it_was_written() {
        // The store keeps the write a backend decided as this text.
        let write = Write {
            id: EntryId(12345),
            status: Status::Rewatching,
            progress: 3,
            rewatches: 2,
            started: Some(Day::new(2026, 9, 2)),
            completed: None,
        };

        let written = serde_json::to_string(&write).expect("serialize");

        assert_eq!(
            written,
            r#"{"id":12345,"status":"rewatching","progress":3,"rewatches":2,"started":{"year":2026,"month":9,"day":2},"completed":null}"#
        );
        assert_eq!(
            serde_json::from_str::<Write>(&written).expect("deserialize"),
            write
        );
    }

    #[test]
    fn a_day_kept_without_its_month_reads_back_as_it_was() {
        let year_alone = Day {
            year: 2015,
            month: None,
            day: None,
        };

        let written = serde_json::to_string(&year_alone).expect("serialize");

        assert_eq!(written, r#"{"year":2015,"month":null,"day":null}"#);
        assert_eq!(
            serde_json::from_str::<Day>(&written).expect("deserialize"),
            year_alone
        );
    }

    #[test]
    fn a_whole_day_names_its_month_and_its_day() {
        assert_eq!(
            Day::new(2026, 11, 16),
            Day {
                year: 2026,
                month: Some(11),
                day: Some(16),
            }
        );
    }
}
