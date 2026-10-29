//! What a viewing writes to an entry of a list.
//!
//! A write carries the whole of where the account stands on a work: the
//! status, the progress, how many times it was seen again, and both dates.
//! AniList moves an entry on its own where a write leaves something out: a
//! progress of the whole count sent alone makes a new entry current, and
//! completes it the second time it is sent. So [`plan`] answers with
//! everything, decided from what the list holds of the entry and the episode
//! that was seen, and a write sent twice leaves the entry where it left it
//! once.
//!
//! **What is written, by what the entry holds and which episode was seen:**
//!
//! | The entry | The episode | Written |
//! | --- | --- | --- |
//! | none, planned, on hold, dropped, watching | above the progress, below the last | watching, that progress, the start date where there is none |
//! | the same | the last | completed, that progress, the start date where there is none, the completion date |
//! | watching, on hold, dropped, planned | at or below the progress | nothing: a conflict |
//! | completed, one episode | 1 | completed, one more time seen again |
//! | completed, several | 1 | rewatching, progress 1 |
//! | completed, several | another | nothing: no rewatch begins there |
//! | rewatching | above the progress, below the last | rewatching, that progress |
//! | rewatching | the last | completed, one more time seen again |
//! | rewatching | at or below the progress | nothing: a conflict |
//!
//! A date is the day of the viewing, which the caller names. Where the table
//! names no date, the dates are the ones the entry holds. The last episode is
//! the entry's count, and an entry with no count has no last episode. An
//! entry completed with no count is taken as one of several. An episode past
//! the count and episode 0 write nothing, whatever the entry holds, each with
//! a reason of its own. Progress counts the episodes seen from the first, so
//! it has no place for episode 0.

use benshi_core::list::Status;

/// A day of the calendar, as a list keeps the day an entry was begun or
/// completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    /// The year.
    pub year: u16,
    /// The month, from 1 to 12.
    pub month: u8,
    /// The day of the month, from 1 to 31.
    pub day: u8,
}

/// Where the account stands on a work, as its list holds it, or as a write
/// leaves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Standing {
    /// The status.
    pub status: Status,
    /// How many episodes the account has seen.
    pub progress: u32,
    /// How many times the account has seen the work again.
    pub rewatches: u32,
    /// The day the account began the work, where the list holds one.
    pub started: Option<Day>,
    /// The day the account completed the work, where the list holds one.
    pub completed: Option<Day>,
}

/// Why a viewing writes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The episode is at or below the progress the list holds.
    Conflict {
        /// The progress the list holds.
        progress: u32,
    },
    /// The list holds the work as completed, and an episode other than the
    /// first begins no rewatch.
    Completed,
    /// The episode is past the count of the work.
    PastCount {
        /// The count.
        episodes: u32,
    },
    /// The episode is 0, and progress counts the episodes seen from the
    /// first.
    EpisodeZero,
}

/// What a viewing writes, or why it writes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The whole entry, as the write leaves it.
    Write(Standing),
    /// Nothing, and why.
    Nothing(Why),
}

/// What seeing this episode on `day` writes to an entry the list holds as
/// `held`.
///
/// `held` is none where the work is on no list. `episodes` is the work's
/// count, and none where the service holds none; `episode` is in the entry's
/// numbering.
#[must_use]
pub fn plan(held: Option<&Standing>, episodes: Option<u32>, episode: u32, day: Day) -> Plan {
    if episode == 0 {
        return Plan::Nothing(Why::EpisodeZero);
    }
    if let Some(count) = episodes
        && episode > count
    {
        return Plan::Nothing(Why::PastCount { episodes: count });
    }
    let last = episodes == Some(episode);
    let held = held.copied().unwrap_or(NOWHERE);

    match held.status {
        Status::Planned | Status::OnHold | Status::Dropped | Status::Watching => {
            if episode <= held.progress {
                return Plan::Nothing(Why::Conflict {
                    progress: held.progress,
                });
            }
            let started = held.started.or(Some(day));
            Plan::Write(if last {
                Standing {
                    status: Status::Completed,
                    progress: episode,
                    started,
                    completed: Some(day),
                    ..held
                }
            } else {
                Standing {
                    status: Status::Watching,
                    progress: episode,
                    started,
                    ..held
                }
            })
        }
        Status::Completed if episode != 1 => Plan::Nothing(Why::Completed),
        Status::Completed if episodes == Some(1) => Plan::Write(Standing {
            progress: 1,
            rewatches: held.rewatches.saturating_add(1),
            ..held
        }),
        Status::Completed => Plan::Write(Standing {
            status: Status::Rewatching,
            progress: 1,
            ..held
        }),
        Status::Rewatching => {
            if episode <= held.progress {
                return Plan::Nothing(Why::Conflict {
                    progress: held.progress,
                });
            }
            Plan::Write(if last {
                Standing {
                    status: Status::Completed,
                    progress: episode,
                    rewatches: held.rewatches.saturating_add(1),
                    ..held
                }
            } else {
                Standing {
                    progress: episode,
                    ..held
                }
            })
        }
    }
}

/// A work on no list, which a viewing writes as an entry planned and not
/// begun would be written.
const NOWHERE: Standing = Standing {
    status: Status::Planned,
    progress: 0,
    rewatches: 0,
    started: None,
    completed: None,
};

#[cfg(test)]
mod tests {
    use super::{Day, Plan, Standing, Why, plan};
    use benshi_core::list::Status;

    /// The day of every viewing here.
    const TODAY: Day = Day {
        year: 2026,
        month: 10,
        day: 14,
    };

    /// A day an entry was begun before.
    const BEGUN: Day = Day {
        year: 2026,
        month: 9,
        day: 2,
    };

    /// A day an entry was completed before.
    const DONE: Day = Day {
        year: 2026,
        month: 9,
        day: 30,
    };

    /// An entry of this status at this progress, begun on [`BEGUN`] where it
    /// is under way.
    fn held(status: Status, progress: u32) -> Standing {
        Standing {
            status,
            progress,
            rewatches: 0,
            started: (progress > 0).then_some(BEGUN),
            completed: None,
        }
    }

    /// A completed entry of this progress, seen again this many times.
    fn completed(progress: u32, rewatches: u32) -> Standing {
        Standing {
            status: Status::Completed,
            progress,
            rewatches,
            started: Some(BEGUN),
            completed: Some(DONE),
        }
    }

    /// A write of this standing.
    fn write(
        status: Status,
        progress: u32,
        rewatches: u32,
        started: Option<Day>,
        completed: Option<Day>,
    ) -> Plan {
        Plan::Write(Standing {
            status,
            progress,
            rewatches,
            started,
            completed,
        })
    }

    #[test]
    fn a_work_on_no_list_seen_short_of_the_last_is_begun_today() {
        assert_eq!(
            plan(None, Some(12), 3, TODAY),
            write(Status::Watching, 3, 0, Some(TODAY), None)
        );
    }

    #[test]
    fn a_work_on_no_list_seen_at_the_last_is_begun_and_completed_today() {
        assert_eq!(
            plan(None, Some(12), 12, TODAY),
            write(Status::Completed, 12, 0, Some(TODAY), Some(TODAY))
        );
        assert_eq!(
            plan(None, Some(1), 1, TODAY),
            write(Status::Completed, 1, 0, Some(TODAY), Some(TODAY))
        );
    }

    #[test]
    fn an_entry_planned_on_hold_or_dropped_is_watching_once_an_episode_past_its_progress_is_seen() {
        for status in [
            Status::Planned,
            Status::OnHold,
            Status::Dropped,
            Status::Watching,
        ] {
            let before = held(status, 4);

            assert_eq!(
                plan(Some(&before), Some(12), 5, TODAY),
                write(Status::Watching, 5, 0, Some(BEGUN), None),
                "{status:?}"
            );
        }
    }

    #[test]
    fn an_entry_under_way_keeps_its_completion_date_and_its_rewatches() {
        let before = Standing {
            status: Status::Dropped,
            progress: 3,
            rewatches: 2,
            started: Some(BEGUN),
            completed: Some(DONE),
        };

        assert_eq!(
            plan(Some(&before), Some(12), 4, TODAY),
            write(Status::Watching, 4, 2, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn an_entry_completed_by_a_viewing_takes_its_day_over_the_one_it_held() {
        let before = Standing {
            status: Status::Dropped,
            progress: 11,
            rewatches: 2,
            started: Some(BEGUN),
            completed: Some(DONE),
        };

        assert_eq!(
            plan(Some(&before), Some(12), 12, TODAY),
            write(Status::Completed, 12, 2, Some(BEGUN), Some(TODAY))
        );
    }

    #[test]
    fn an_entry_with_no_start_date_is_begun_today() {
        let planned = held(Status::Planned, 0);
        assert_eq!(planned.started, None);

        assert_eq!(
            plan(Some(&planned), Some(12), 1, TODAY),
            write(Status::Watching, 1, 0, Some(TODAY), None)
        );
    }

    #[test]
    fn an_entry_seen_at_its_last_episode_is_completed_today() {
        for status in [
            Status::Planned,
            Status::OnHold,
            Status::Dropped,
            Status::Watching,
        ] {
            let before = held(status, 11);

            assert_eq!(
                plan(Some(&before), Some(12), 12, TODAY),
                write(Status::Completed, 12, 0, Some(BEGUN), Some(TODAY)),
                "{status:?}"
            );
        }
    }

    #[test]
    fn an_episode_at_or_below_the_progress_of_an_entry_under_way_is_a_conflict() {
        for status in [
            Status::Watching,
            Status::OnHold,
            Status::Dropped,
            Status::Planned,
        ] {
            let before = held(status, 5);

            for episode in [5, 2] {
                assert_eq!(
                    plan(Some(&before), Some(12), episode, TODAY),
                    Plan::Nothing(Why::Conflict { progress: 5 }),
                    "{status:?} {episode}"
                );
            }
        }
    }

    #[test]
    fn a_work_of_one_episode_seen_again_is_completed_one_time_more() {
        // A film seen again: the dates are the ones the list holds.
        assert_eq!(
            plan(Some(&completed(1, 2)), Some(1), 1, TODAY),
            write(Status::Completed, 1, 3, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn a_series_seen_again_from_its_first_episode_is_rewatched() {
        assert_eq!(
            plan(Some(&completed(12, 1)), Some(12), 1, TODAY),
            write(Status::Rewatching, 1, 1, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn another_episode_of_a_completed_series_writes_nothing() {
        for episode in [2, 5, 12] {
            assert_eq!(
                plan(Some(&completed(12, 0)), Some(12), episode, TODAY),
                Plan::Nothing(Why::Completed),
                "{episode}"
            );
        }
    }

    #[test]
    fn a_rewatch_goes_on_short_of_the_last_episode() {
        let rewatching = Standing {
            status: Status::Rewatching,
            progress: 3,
            ..completed(3, 1)
        };

        assert_eq!(
            plan(Some(&rewatching), Some(12), 4, TODAY),
            write(Status::Rewatching, 4, 1, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn a_rewatch_at_its_last_episode_is_completed_one_time_more() {
        let rewatching = Standing {
            status: Status::Rewatching,
            progress: 11,
            ..completed(11, 1)
        };

        assert_eq!(
            plan(Some(&rewatching), Some(12), 12, TODAY),
            write(Status::Completed, 12, 2, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn an_episode_at_or_below_the_progress_of_a_rewatch_is_a_conflict() {
        let rewatching = Standing {
            status: Status::Rewatching,
            progress: 5,
            ..completed(5, 1)
        };

        for episode in [5, 2] {
            assert_eq!(
                plan(Some(&rewatching), Some(12), episode, TODAY),
                Plan::Nothing(Why::Conflict { progress: 5 }),
                "{episode}"
            );
        }
    }

    #[test]
    fn an_entry_with_no_count_has_no_last_episode() {
        assert_eq!(
            plan(Some(&held(Status::Watching, 29)), None, 30, TODAY),
            write(Status::Watching, 30, 0, Some(BEGUN), None)
        );
        assert_eq!(
            plan(None, None, 1, TODAY),
            write(Status::Watching, 1, 0, Some(TODAY), None)
        );
    }

    #[test]
    fn an_entry_completed_with_no_count_is_taken_as_one_of_several() {
        assert_eq!(
            plan(Some(&completed(12, 0)), None, 1, TODAY),
            write(Status::Rewatching, 1, 0, Some(BEGUN), Some(DONE))
        );
    }

    #[test]
    fn an_episode_past_the_count_writes_nothing() {
        for before in [
            None,
            Some(held(Status::Watching, 11)),
            Some(completed(12, 0)),
        ] {
            assert_eq!(
                plan(before.as_ref(), Some(12), 13, TODAY),
                Plan::Nothing(Why::PastCount { episodes: 12 }),
                "{before:?}"
            );
        }
    }

    #[test]
    fn an_episode_zero_writes_nothing_whatever_the_entry_holds() {
        let rewatching = Standing {
            status: Status::Rewatching,
            progress: 3,
            ..completed(3, 1)
        };

        for before in [
            None,
            Some(held(Status::Planned, 0)),
            Some(held(Status::Watching, 5)),
            Some(completed(12, 0)),
            Some(rewatching),
        ] {
            for episodes in [Some(12), Some(1), None] {
                assert_eq!(
                    plan(before.as_ref(), episodes, 0, TODAY),
                    Plan::Nothing(Why::EpisodeZero),
                    "{before:?} {episodes:?}"
                );
            }
        }
    }
}
