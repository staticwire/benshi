//! Which episode of which entry of a list an episode of a show is.
//!
//! An episode is kept in the release's numbering, which is all a filename
//! carries. A list numbers it as the service models the work: a group that
//! numbers a second season on from the first names episode 14 of the first,
//! and the service files it as episode 1 of the second. [`resolve`] turns the
//! one into the other for one backend, from what that backend's list says of
//! the show, the rules of `anime-relations` and the pins the user made.
//!
//! **In this order, and the first that answers decides:**
//!
//! 1. a pin of that episode of that show;
//! 2. the entry the list links the show to, where the episode is within its
//!    count;
//! 3. a rule that takes that episode of that entry, wherever it sends it;
//! 4. the entry, where the episode is past its count and numbers the first
//!    season by the hundred, as `101` names episode 1, and the count holds
//!    the episode it names;
//! 5. the entry, where the list holds no count for it and the episode is
//!    above zero.
//!
//! Otherwise the episode is unresolved, with the reason. A rule is asked only
//! where the count does not answer, because a rule describes a numbering that
//! falls outside an entry's count. Where the count is not known a rule is
//! asked first, and the entry takes an episode above
//! zero that no rule takes. A rule that sends an episode to an entry the list
//! does not hold resolves to it all the same: the work is the rule's to name,
//! and the write that follows adds it to the list.
//!
//! A name that spells no episode is the one episode of an entry of one.

use benshi_core::ShowId;
use benshi_core::list::EntryId;

use crate::relations::{Redirect, Relations, Service};

/// One episode of one entry of a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// The entry, by the number the service gives the work.
    pub entry: EntryId,
    /// The episode, in the entry's numbering.
    pub episode: u32,
}

/// An episode of a show the user said is an episode of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// The show.
    pub show: ShowId,
    /// The episode, in the release's numbering, and none for a name that
    /// spells none.
    pub episode: Option<u32>,
    /// Where it goes.
    pub to: Target,
}

/// The entry a list links a show to, and what the list holds of its count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listed {
    /// The entry.
    pub entry: EntryId,
    /// How many episodes the work has, and none where the list holds no
    /// count or no longer holds the entry.
    pub episodes: Option<u32>,
}

/// What decided where an episode goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// A pin the user made.
    Pin,
    /// The entry the list links the show to.
    Entry,
    /// A rule of `anime-relations`.
    Rule,
}

/// Why an episode was not resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unresolved {
    /// No pin names the episode, and the list links the show to no entry.
    NotListed,
    /// The name spells no episode, and the list does not hold a count of one
    /// for the entry.
    NoEpisode {
        /// The entry's count, where the list holds one.
        episodes: Option<u32>,
    },
    /// The episode is outside the entry's count, and no rule takes it.
    Outside {
        /// The entry's count, where the list holds one.
        episodes: Option<u32>,
    },
    /// A rule takes the episode, and gives no number for where it goes.
    Unnumbered,
}

/// What an episode of a show resolved to, or why it did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// This episode of this entry, and what said so.
    Resolved {
        /// Where the episode goes.
        to: Target,
        /// What decided.
        by: By,
    },
    /// No episode of any entry, and why.
    Unresolved(Unresolved),
}

/// Which episode of which entry an episode of a show is, on one backend.
///
/// `episode` is in the release's numbering, `listed` what the backend's list
/// says of the show, `pins` the pins of that backend, and `rules` are read
/// in the column of `service`.
#[must_use]
pub fn resolve(
    show: ShowId,
    episode: Option<u32>,
    listed: Option<Listed>,
    pins: &[Pin],
    rules: &Relations,
    service: Service,
) -> Resolution {
    if let Some(pin) = pins
        .iter()
        .find(|pin| pin.show == show && pin.episode == episode)
    {
        return Resolution::Resolved {
            to: pin.to,
            by: By::Pin,
        };
    }
    let Some(Listed { entry, episodes }) = listed else {
        return Resolution::Unresolved(Unresolved::NotListed);
    };
    let the_entry = |episode| Resolution::Resolved {
        to: Target { entry, episode },
        by: By::Entry,
    };

    let Some(episode) = episode else {
        return match episodes {
            Some(1) => the_entry(1),
            _ => Resolution::Unresolved(Unresolved::NoEpisode { episodes }),
        };
    };
    if let Some(count) = episodes
        && (1..=count).contains(&episode)
    {
        return the_entry(episode);
    }
    match rules.redirect(service, entry, episode) {
        Some(Redirect::To { work, episode }) => Resolution::Resolved {
            to: Target {
                entry: work,
                episode,
            },
            by: By::Rule,
        },
        Some(Redirect::Unnumbered) => Resolution::Unresolved(Unresolved::Unnumbered),
        None => match episodes.map(|count| of_the_first_season(episode, count)) {
            Some(Some(numbered)) => the_entry(numbered),
            None if episode > 0 => the_entry(episode),
            _ => Resolution::Unresolved(Unresolved::Outside { episodes }),
        },
    }
}

/// The episode of the first season a release names by the hundred, `101` for
/// the first, where the entry holds it. Asked only of an episode outside the
/// count: an episode the count holds is that episode, however many there are.
fn of_the_first_season(episode: u32, count: u32) -> Option<u32> {
    let numbered = episode.checked_sub(100)?;
    (episode < 200 && (1..=count).contains(&numbered)).then_some(numbered)
}

#[cfg(test)]
mod tests {
    use super::{By, Listed, Pin, Resolution, Target, Unresolved, resolve};
    use crate::relations::{Relations, Service};
    use benshi_core::ShowId;
    use benshi_core::list::EntryId;

    /// The show every test asks about.
    const SHOW: ShowId = ShowId(1);

    /// The entry the list links [`SHOW`] to.
    const ENTRY: EntryId = EntryId(900_001);

    /// A file of these rule lines.
    fn rules(lines: &str) -> Relations {
        let text = format!("::meta\n- version: 1.3.0\n::rules\n{lines}\n");
        Relations::read(&text).unwrap_or_else(|refused| panic!("{refused}: {text}"))
    }

    /// No rules at all.
    fn no_rules() -> Relations {
        rules("")
    }

    /// The entry, listed with this count.
    fn of(episodes: Option<u32>) -> Listed {
        Listed {
            entry: ENTRY,
            episodes,
        }
    }

    /// Where an episode of [`SHOW`] goes on AniList, with no pins.
    fn on_anilist(episode: Option<u32>, listed: Option<Listed>, rules: &Relations) -> Resolution {
        resolve(SHOW, episode, listed, &[], rules, Service::AniList)
    }

    /// This episode of this entry, as decided this way.
    fn to(entry: i64, episode: u32, by: By) -> Resolution {
        Resolution::Resolved {
            to: Target {
                entry: EntryId(entry),
                episode,
            },
            by,
        }
    }

    #[test]
    fn an_episode_within_the_count_of_its_entry_is_that_episode() {
        for episode in [1, 3, 12] {
            assert_eq!(
                on_anilist(Some(episode), Some(of(Some(12))), &no_rules()),
                to(900_001, episode, By::Entry),
                "{episode}"
            );
        }
    }

    #[test]
    fn an_entry_with_no_count_has_its_episode_zero_taken_by_a_rule() {
        let special = rules("- ?|?|900001:0 -> ?|?|900003:1");

        assert_eq!(
            on_anilist(Some(5), Some(of(None)), &special),
            to(900_001, 5, By::Entry)
        );
        assert_eq!(
            on_anilist(Some(0), Some(of(None)), &special),
            to(900_003, 1, By::Rule)
        );
    }

    #[test]
    fn an_episode_a_rule_takes_and_cannot_number_is_unnumbered() {
        let past = rules("- ?|?|900001:13-? -> ?|?|900002:4294967295-?");

        assert_eq!(
            on_anilist(Some(14), Some(of(None)), &past),
            Resolution::Unresolved(Unresolved::Unnumbered)
        );
    }

    #[test]
    fn an_episode_numbered_by_the_hundred_is_of_the_first_season() {
        // A release that numbers its seasons by the hundred names the first
        // episode of the first season 101.
        let twelve = Some(of(Some(12)));

        assert_eq!(
            on_anilist(Some(101), twelve, &no_rules()),
            to(900_001, 1, By::Entry)
        );
        assert_eq!(
            on_anilist(Some(112), twelve, &no_rules()),
            to(900_001, 12, By::Entry)
        );
        for outside in [100, 113, 201] {
            assert_eq!(
                on_anilist(Some(outside), twelve, &no_rules()),
                Resolution::Unresolved(Unresolved::Outside { episodes: Some(12) }),
                "{outside}"
            );
        }
    }

    #[test]
    fn an_episode_the_count_holds_is_that_episode_however_many_there_are() {
        for count in [150, 500] {
            assert_eq!(
                on_anilist(Some(101), Some(of(Some(count))), &no_rules()),
                to(900_001, 101, By::Entry),
                "{count}"
            );
        }
        assert_eq!(
            on_anilist(Some(101), Some(of(Some(100))), &no_rules()),
            to(900_001, 1, By::Entry)
        );
        assert_eq!(
            on_anilist(Some(230), Some(of(Some(150))), &no_rules()),
            Resolution::Unresolved(Unresolved::Outside {
                episodes: Some(150)
            })
        );
        assert_eq!(
            on_anilist(Some(101), Some(of(None)), &no_rules()),
            to(900_001, 101, By::Entry)
        );
    }

    #[test]
    fn a_rule_that_takes_an_episode_past_a_hundred_is_asked_first() {
        let later = rules("- ?|?|900001:101-? -> ?|?|900002:1-?");

        assert_eq!(
            on_anilist(Some(101), Some(of(Some(12))), &later),
            to(900_002, 1, By::Rule)
        );
    }

    #[test]
    fn a_count_of_zero_holds_no_episode() {
        assert_eq!(
            on_anilist(None, Some(of(Some(0))), &no_rules()),
            Resolution::Unresolved(Unresolved::NoEpisode { episodes: Some(0) })
        );
        assert_eq!(
            on_anilist(Some(1), Some(of(Some(0))), &no_rules()),
            Resolution::Unresolved(Unresolved::Outside { episodes: Some(0) })
        );
    }

    #[test]
    fn a_pin_answers_before_a_rule_and_the_first_pin_before_the_next() {
        let past = rules("- ?|?|900001:2 -> ?|?|900007:1");
        let pins = [
            Pin {
                show: SHOW,
                episode: Some(2),
                to: Target {
                    entry: EntryId(900_006),
                    episode: 1,
                },
            },
            Pin {
                show: SHOW,
                episode: Some(2),
                to: Target {
                    entry: EntryId(900_008),
                    episode: 1,
                },
            },
        ];

        assert_eq!(
            on_anilist(Some(2), Some(of(Some(1))), &past),
            to(900_007, 1, By::Rule)
        );
        assert_eq!(
            resolve(
                SHOW,
                Some(2),
                Some(of(Some(1))),
                &pins,
                &past,
                Service::AniList
            ),
            to(900_006, 1, By::Pin)
        );
    }

    #[test]
    fn an_episode_past_the_count_that_no_rule_takes_is_outside() {
        assert_eq!(
            on_anilist(Some(13), Some(of(Some(12))), &no_rules()),
            Resolution::Unresolved(Unresolved::Outside { episodes: Some(12) })
        );
    }

    #[test]
    fn an_episode_past_the_count_goes_where_a_rule_sends_it() {
        // A group numbered the second season on from the first, and the
        // service files the second as an entry of its own, which need not be
        // on the list.
        let continuous = rules("- ?|?|900001:13-24 -> ?|?|900002:1-12!");

        assert_eq!(
            on_anilist(Some(14), Some(of(Some(12))), &continuous),
            to(900_002, 2, By::Rule)
        );
        assert_eq!(
            on_anilist(Some(24), Some(of(Some(12))), &continuous),
            to(900_002, 12, By::Rule)
        );
    }

    #[test]
    fn a_rule_is_not_asked_of_an_episode_within_the_count() {
        let within = rules("- ?|?|900001:1-12 -> ?|?|900002:1-12");

        assert_eq!(
            on_anilist(Some(3), Some(of(Some(12))), &within),
            to(900_001, 3, By::Entry)
        );
    }

    #[test]
    fn two_episodes_of_a_release_go_to_the_one_special_a_rule_names() {
        let special = rules("- ?|?|900001:11-12 -> ?|?|900003:1");

        for episode in [11, 12] {
            assert_eq!(
                on_anilist(Some(episode), Some(of(Some(10))), &special),
                to(900_003, 1, By::Rule),
                "{episode}"
            );
        }
    }

    #[test]
    fn an_episode_zero_goes_where_a_rule_sends_it_and_is_outside_without_one() {
        let prologue = rules("- ?|?|900001:0 -> ?|?|900004:1");

        assert_eq!(
            on_anilist(Some(0), Some(of(Some(12))), &prologue),
            to(900_004, 1, By::Rule)
        );
        assert_eq!(
            on_anilist(Some(0), Some(of(Some(12))), &no_rules()),
            Resolution::Unresolved(Unresolved::Outside { episodes: Some(12) })
        );
    }

    #[test]
    fn a_long_numbering_goes_where_an_open_rule_sends_it() {
        let open = rules("- ?|?|900001:171-? -> ?|?|900005:1-?!");

        assert_eq!(
            on_anilist(Some(180), Some(of(Some(170))), &open),
            to(900_005, 10, By::Rule)
        );
    }

    #[test]
    fn a_rule_to_a_work_the_service_does_not_number_is_unresolved() {
        let unnumbered = rules("- 900011|?|900001:13 -> 900012|?|?:1");

        assert_eq!(
            on_anilist(Some(13), Some(of(Some(12))), &unnumbered),
            Resolution::Unresolved(Unresolved::Unnumbered)
        );
    }

    #[test]
    fn the_rules_are_read_in_the_column_of_the_service() {
        let both = rules("- 900011|?|900001:13 -> 900012|?|900002:1");
        let listed = |entry| {
            Some(Listed {
                entry: EntryId(entry),
                episodes: Some(12),
            })
        };

        assert_eq!(
            resolve(
                SHOW,
                Some(13),
                listed(900_011),
                &[],
                &both,
                Service::MyAnimeList
            ),
            to(900_012, 1, By::Rule)
        );
        assert_eq!(
            resolve(
                SHOW,
                Some(13),
                listed(900_001),
                &[],
                &both,
                Service::AniList
            ),
            to(900_002, 1, By::Rule)
        );
        assert_eq!(
            resolve(
                SHOW,
                Some(13),
                listed(900_011),
                &[],
                &both,
                Service::AniList
            ),
            Resolution::Unresolved(Unresolved::Outside { episodes: Some(12) })
        );
    }

    #[test]
    fn an_entry_with_no_count_takes_an_episode_above_zero_that_no_rule_takes() {
        // A series still airing has no count on the service. Each episode
        // above zero is its own, but where a rule takes one it decides.
        let continuous = rules("- ?|?|900001:13-24 -> ?|?|900002:1-12");

        assert_eq!(
            on_anilist(Some(30), Some(of(None)), &continuous),
            to(900_001, 30, By::Entry)
        );
        assert_eq!(
            on_anilist(Some(14), Some(of(None)), &continuous),
            to(900_002, 2, By::Rule)
        );
        assert_eq!(
            on_anilist(Some(0), Some(of(None)), &no_rules()),
            Resolution::Unresolved(Unresolved::Outside { episodes: None })
        );
    }

    #[test]
    fn a_name_with_no_episode_is_the_one_episode_of_an_entry_of_one() {
        assert_eq!(
            on_anilist(None, Some(of(Some(1))), &no_rules()),
            to(900_001, 1, By::Entry)
        );
    }

    #[test]
    fn a_name_with_no_episode_of_an_entry_of_several_or_of_no_count_is_unresolved() {
        for episodes in [Some(2), None] {
            assert_eq!(
                on_anilist(None, Some(of(episodes)), &no_rules()),
                Resolution::Unresolved(Unresolved::NoEpisode { episodes }),
                "{episodes:?}"
            );
        }
    }

    #[test]
    fn a_show_the_list_links_to_no_entry_is_not_listed() {
        assert_eq!(
            on_anilist(Some(3), None, &no_rules()),
            Resolution::Unresolved(Unresolved::NotListed)
        );
    }

    #[test]
    fn a_pin_comes_before_the_entry_and_the_rules() {
        // The entry's count holds episode 2 and a rule takes it, and the pin
        // still decides.
        let pin = Pin {
            show: SHOW,
            episode: Some(2),
            to: Target {
                entry: EntryId(900_006),
                episode: 1,
            },
        };
        let rule = rules("- ?|?|900001:2 -> ?|?|900007:1");

        assert_eq!(
            resolve(
                SHOW,
                Some(2),
                Some(of(Some(2))),
                &[pin],
                &rule,
                Service::AniList
            ),
            to(900_006, 1, By::Pin)
        );
        assert_eq!(
            resolve(
                SHOW,
                Some(1),
                Some(of(Some(2))),
                &[pin],
                &rule,
                Service::AniList
            ),
            to(900_001, 1, By::Entry)
        );
    }

    #[test]
    fn a_pin_resolves_a_show_the_list_links_to_no_entry() {
        let pin = Pin {
            show: SHOW,
            episode: None,
            to: Target {
                entry: EntryId(900_008),
                episode: 1,
            },
        };

        assert_eq!(
            resolve(SHOW, None, None, &[pin], &no_rules(), Service::AniList),
            to(900_008, 1, By::Pin)
        );
    }

    #[test]
    fn a_pin_of_another_show_or_another_episode_is_passed_over() {
        let elsewhere = |show, episode| Pin {
            show,
            episode,
            to: Target {
                entry: EntryId(900_009),
                episode: 1,
            },
        };
        let pins = [
            elsewhere(ShowId(2), Some(3)),
            elsewhere(SHOW, Some(4)),
            elsewhere(SHOW, None),
        ];

        assert_eq!(
            resolve(
                SHOW,
                Some(3),
                Some(of(Some(12))),
                &pins,
                &no_rules(),
                Service::AniList
            ),
            to(900_001, 3, By::Entry)
        );
    }
}
