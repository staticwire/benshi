//! The rules of `anime-relations`: an episode of one work that is an episode
//! of another.
//!
//! A release numbers its episodes as its group chose. A group that numbers a
//! second season on from the first names its first episode 14, of a series of
//! thirteen, and a group that counts a special into a series names it as one
//! of the series. The rules of `erengy/anime-relations` say where each such
//! episode belongs, for the works of three services. The file is compiled in,
//! from that repository at commit
//! `1eae4b63008d9e91fb9b5a9ced22ed20037f479f`, version 1.3.0, and is in the
//! public domain under CC0 1.0, whose text is beside it in
//! `data/anime-relations.LICENSE`.
//!
//! A rule is written as the file writes it:
//!
//! ```text
//! - 900001|900002|900003:14-25 -> 900004|900005|900006:1-12!
//! ```
//!
//! A work is numbered by MyAnimeList, by Kitsu and by AniList, in that order,
//! and `?` is a number the file does not know. `~` in the second half
//! repeats the number of the first. The episodes are one number or a range,
//! and a range that ends in `?` has no last episode. An `!` at the end makes
//! a second rule, from the second work numbered as the first to the second
//! work: the rule above also sends episode 14 of `900006` to its episode 1.
//!
//! **Every rule is read or none is.** A range of the second half is one
//! episode, which every episode of the first goes to, or as long as the
//! range of the first, which it takes episode by episode; a range is open on
//! both sides or on neither. A file that writes a rule any other way is
//! refused at the line, because a rule read some other way would send an
//! episode where its author did not.

use std::sync::LazyLock;

use benshi_core::list::EntryId;

/// The rules this build carries, as the file of `anime-relations` writes them.
pub const BUNDLED: &str = include_str!("../data/anime-relations.txt");

/// A service whose numbers a rule carries, one column each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// The first column.
    MyAnimeList,
    /// The second column.
    Kitsu,
    /// The third column.
    AniList,
}

impl Service {
    /// The column of this service in a rule.
    const fn column(self) -> usize {
        match self {
            Self::MyAnimeList => 0,
            Self::Kitsu => 1,
            Self::AniList => 2,
        }
    }
}

/// Episodes of one work, from the first to the last, and on with no end
/// where the rule writes `?` for the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episodes {
    /// The first episode.
    pub first: u32,
    /// The last episode, and none where the range has no end.
    pub last: Option<u32>,
}

impl Episodes {
    /// Whether the episodes hold this one.
    fn holds(self, episode: u32) -> bool {
        episode >= self.first && self.last.is_none_or(|last| episode <= last)
    }
}

/// One half of a rule: a work as each service numbers it, and its episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    /// The work, by the number of each service in the order of
    /// [`Service`], and none where the file does not know the number.
    pub work: [Option<EntryId>; 3],
    /// Its episodes.
    pub episodes: Episodes,
}

/// A rule, as the file writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The work and the episodes the rule takes.
    pub from: End,
    /// The work and the episodes it sends them to.
    pub to: End,
    /// Whether the rule ends in `!`, and so takes the same episodes of the
    /// second work as well.
    pub also_to_itself: bool,
}

/// Where a rule sends an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirect {
    /// This episode of this work.
    To {
        /// The work, by the number of the service that asked.
        work: EntryId,
        /// The episode of it.
        episode: u32,
    },
    /// A rule takes the episode, and gives no number for where it goes: it
    /// writes `?` for the work on the service that asked, or the episode it
    /// sends to is past `u32::MAX`.
    Unnumbered,
}

/// Why a file of rules was not read, and at which line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {reason}")]
pub struct Error {
    /// The line, counted from one, and 0 for a file with no line.
    pub line: usize,
    /// What is wrong with it.
    pub reason: Reason,
}

/// What is wrong with a line of a file of rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Reason {
    /// The line stands before the first section.
    #[error("line is in no section")]
    NoSection,
    /// A section this reader does not know.
    #[error("section {0:?} is not one of meta and rules")]
    Section(String),
    /// A line of the rules section that does not begin with `- `, as a rule
    /// does.
    #[error("{0:?} is not a rule")]
    NotRule(String),
    /// A rule with no arrow between its two halves.
    #[error("rule has no arrow")]
    NoArrow,
    /// A half of a rule that is not three numbers and episodes.
    #[error("{0:?} is not three numbers and episodes")]
    Half(String),
    /// A number of a work that is not digits within the range of
    /// [`EntryId`], `?` or, in the second half, `~`.
    #[error("{0:?} is not a number of a work")]
    Work(String),
    /// Episodes that are not a number or a range, or name an episode past
    /// `u32::MAX`.
    #[error("{0:?} is not an episode or a range of them")]
    Episodes(String),
    /// A range that ends before it begins.
    #[error("range {0:?} ends before it begins")]
    Backwards(String),
    /// A range open on one side of a rule and closed on the other.
    #[error("range is open on one side only")]
    OpenOnOneSide,
    /// A second half neither one episode nor as long as the first.
    #[error("second half is neither one episode nor as long as first half")]
    Lengths,
    /// A file that names no version in its meta section, or names only an
    /// empty one.
    #[error("file names no version")]
    NoVersion,
}

/// The rules of a file, in the order it writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relations {
    version: String,
    rules: Vec<Rule>,
}

impl Relations {
    /// The rules of a file written as `anime-relations` writes one.
    ///
    /// # Errors
    ///
    /// The first line that is refused, with what is wrong with it: a line
    /// before every section, a section other than `meta` and `rules`, or a
    /// line of `rules` that is not a rule written as the module says.
    /// Comments and blank lines are passed over anywhere, and so is every
    /// line of `meta` but its version. A file that names no version, or only
    /// an empty one, is refused at its last line, which is line 0 for an
    /// empty file.
    pub fn read(text: &str) -> Result<Self, Error> {
        let mut section = None;
        let mut version = None;
        let mut rules = Vec::new();
        let mut lines = 0;

        for (index, written) in text.lines().enumerate() {
            let line = index + 1;
            lines = line;
            let at = |reason| Error { line, reason };
            let written = written.trim();
            if written.is_empty() || written.starts_with('#') {
                continue;
            }
            if let Some(name) = written.strip_prefix("::") {
                section = match name {
                    "meta" => Some(Section::Meta),
                    "rules" => Some(Section::Rules),
                    other => return Err(at(Reason::Section(other.to_owned()))),
                };
                continue;
            }
            match section {
                None => return Err(at(Reason::NoSection)),
                Some(Section::Meta) => {
                    if let Some(named) = written.strip_prefix("- version:").map(str::trim)
                        && !named.is_empty()
                    {
                        version = Some(named.to_owned());
                    }
                }
                Some(Section::Rules) => rules.push(rule(written).map_err(at)?),
            }
        }

        let version = version.ok_or(Error {
            line: lines,
            reason: Reason::NoVersion,
        })?;
        Ok(Self { version, rules })
    }

    /// The version the file names.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Every rule, in the order the file writes them.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Where the rules send this episode of this work, as `service` numbers
    /// works, and nothing where no rule takes it.
    ///
    /// The first rule that takes the episode decides, in the order of the
    /// file, and the second rule an `!` makes counts right after the rule
    /// that makes it, numbered or [`Redirect::Unnumbered`].
    #[must_use]
    pub fn redirect(&self, service: Service, work: EntryId, episode: u32) -> Option<Redirect> {
        let column = service.column();
        self.rules
            .iter()
            .find(|rule| rule.takes(column, work, episode))
            .map(|rule| rule.sends(column, episode))
    }
}

impl Rule {
    /// Whether this rule takes this episode of a work numbered in this
    /// column.
    fn takes(&self, column: usize, work: EntryId, episode: u32) -> bool {
        self.from.episodes.holds(episode)
            && (self.from.work[column] == Some(work)
                || self.also_to_itself && self.to.work[column] == Some(work))
    }

    /// Where this rule sends an episode it takes, as this column numbers
    /// works.
    fn sends(&self, column: usize, episode: u32) -> Redirect {
        let Some(sent_to) = self.to.work[column] else {
            return Redirect::Unnumbered;
        };
        let to = self.to.episodes;
        let sent = if to.last == Some(to.first) {
            Some(to.first)
        } else {
            to.first.checked_add(episode - self.from.episodes.first)
        };

        sent.map_or(Redirect::Unnumbered, |episode| Redirect::To {
            work: sent_to,
            episode,
        })
    }
}

/// A section of the file.
#[derive(Debug, Clone, Copy)]
enum Section {
    /// `::meta`, whose `version` is the one entry read.
    Meta,
    /// `::rules`, where every line but a comment or a blank one is a rule.
    Rules,
}

/// One line of the rules section, read as a rule.
fn rule(written: &str) -> Result<Rule, Reason> {
    let body = written
        .strip_prefix("- ")
        .ok_or_else(|| Reason::NotRule(written.to_owned()))?;
    let (from, to) = body.split_once(" -> ").ok_or(Reason::NoArrow)?;
    let (to, also_to_itself) = to.strip_suffix('!').map_or((to, false), |to| (to, true));
    let from = half(from, None)?;
    let to = half(to, Some(&from.work))?;

    match (from.episodes.last, to.episodes.last) {
        (None, None) => {}
        (Some(_), None) | (None, Some(_)) => return Err(Reason::OpenOnOneSide),
        (Some(last), Some(sent_last)) => {
            let sent = sent_last - to.episodes.first;
            if sent != 0 && sent != last - from.episodes.first {
                return Err(Reason::Lengths);
            }
        }
    }

    Ok(Rule {
        from,
        to,
        also_to_itself,
    })
}

/// One half of a rule: three numbers of a work and its episodes.
///
/// `first` is the work of the first half, which a `~` in the second repeats,
/// and none for the first half, where a `~` is refused.
fn half(written: &str, first: Option<&[Option<EntryId>; 3]>) -> Result<End, Reason> {
    let not_a_half = || Reason::Half(written.to_owned());
    let (numbers, episodes) = written.split_once(':').ok_or_else(not_a_half)?;
    let numbers: Vec<&str> = numbers.split('|').collect();
    if numbers.len() != 3 {
        return Err(not_a_half());
    }

    let mut work = [None; 3];
    for (column, number) in numbers.into_iter().enumerate() {
        work[column] = match (number, first) {
            ("?", _) => None,
            ("~", Some(first)) => first[column],
            (digits, _) => Some(EntryId(
                whole(digits).ok_or_else(|| Reason::Work(digits.to_owned()))?,
            )),
        };
    }

    Ok(End {
        work,
        episodes: range(episodes)?,
    })
}

/// Episodes written as one number, as a range, or as a range open at its
/// end.
fn range(written: &str) -> Result<Episodes, Reason> {
    let not_episodes = || Reason::Episodes(written.to_owned());
    let episode = |digits: &str| whole(digits).ok_or_else(not_episodes);

    match written.split_once('-') {
        None => {
            let only = episode(written)?;
            Ok(Episodes {
                first: only,
                last: Some(only),
            })
        }
        Some((first, "?")) => Ok(Episodes {
            first: episode(first)?,
            last: None,
        }),
        Some((first, last)) => {
            let (first, last) = (episode(first)?, episode(last)?);
            if last < first {
                return Err(Reason::Backwards(written.to_owned()));
            }
            Ok(Episodes {
                first,
                last: Some(last),
            })
        }
    }
}

/// A number written in decimal digits and nothing else.
fn whole<N: std::str::FromStr>(digits: &str) -> Option<N> {
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The rules this build carries.
///
/// # Panics
///
/// Where [`BUNDLED`] does not read, which its tests rule out.
#[must_use]
pub fn bundled() -> &'static Relations {
    static READ: LazyLock<Relations> = LazyLock::new(|| {
        Relations::read(BUNDLED)
            .unwrap_or_else(|refused| panic!("bundled anime-relations does not read: {refused}"))
    });
    &READ
}

#[cfg(test)]
mod tests {
    use super::{Episodes, Error, Reason, Redirect, Relations, Service, bundled};
    use benshi_core::list::EntryId;

    /// A file of these rule lines, after a meta section that names its
    /// version, so the first of them is line 4.
    fn file(lines: &str) -> String {
        format!("::meta\n- version: 1.3.0\n::rules\n{lines}\n")
    }

    /// The rules of a file of these rule lines.
    fn rules(lines: &str) -> Relations {
        let text = file(lines);
        Relations::read(&text).unwrap_or_else(|refused| panic!("{refused}: {text}"))
    }

    /// What a file of these rule lines is refused with.
    fn refusal(lines: &str) -> Error {
        Relations::read(&file(lines)).expect_err("the file is refused")
    }

    /// Where the rules send an episode of a work numbered by AniList.
    fn on_anilist(relations: &Relations, work: i64, episode: u32) -> Option<Redirect> {
        relations.redirect(Service::AniList, EntryId(work), episode)
    }

    /// This episode of this work.
    fn to(work: i64, episode: u32) -> Redirect {
        Redirect::To {
            work: EntryId(work),
            episode,
        }
    }

    #[test]
    fn every_rule_of_the_bundled_file_is_read() {
        let bundled = bundled();

        assert_eq!(bundled.version(), "1.3.0");
        assert_eq!(bundled.rules().len(), 549);
    }

    #[test]
    fn the_bundled_file_reads_as_counted_over_its_text() {
        // Counted over the text with no reader of its own: 255 rules end in
        // `!`, 26 have no AniList number for the work they take and 26 none
        // for the work they send to, and 6 have an open range.
        let rules = bundled().rules();
        let anilist = Service::AniList.column();

        assert_eq!(rules.iter().filter(|rule| rule.also_to_itself).count(), 255);
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.from.work[anilist].is_none())
                .count(),
            26
        );
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.to.work[anilist].is_none())
                .count(),
            26
        );
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.from.episodes.last.is_none())
                .count(),
            6
        );
    }

    #[test]
    fn every_rule_of_the_bundled_file_that_takes_several_episodes_to_one_sends_each_there() {
        let mut several_to_one = 0;
        for rule in bundled().rules() {
            let (from, to) = (rule.from.episodes, rule.to.episodes);
            let (Some(last), Some(only)) = (from.last, to.last) else {
                continue;
            };
            if last == from.first || only != to.first {
                continue;
            }
            several_to_one += 1;
            for service in [Service::MyAnimeList, Service::Kitsu, Service::AniList] {
                let (Some(work), Some(sent_to)) = (
                    rule.from.work[service.column()],
                    rule.to.work[service.column()],
                ) else {
                    continue;
                };
                for episode in from.first..=last {
                    assert_eq!(
                        bundled().redirect(service, work, episode),
                        Some(Redirect::To {
                            work: sent_to,
                            episode: to.first,
                        }),
                        "{rule:?} {service:?} {episode}"
                    );
                }
            }
        }
        assert_eq!(several_to_one, 4);
    }

    #[test]
    fn two_episodes_go_to_one() {
        // A special the group counted as two episodes of the series.
        let relations = rules("- 900001|900002|900003:11-12 -> 900004|900005|900006:1");

        assert_eq!(on_anilist(&relations, 900_003, 11), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&relations, 900_003, 12), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&relations, 900_003, 10), None);
        assert_eq!(on_anilist(&relations, 900_003, 13), None);
    }

    #[test]
    fn an_episode_zero_goes_to_the_first() {
        let relations = rules("- 900001|900002|900003:0 -> 900004|900005|900006:1");

        assert_eq!(on_anilist(&relations, 900_003, 0), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&relations, 900_003, 1), None);
    }

    #[test]
    fn a_range_goes_to_a_range_episode_by_episode() {
        let relations = rules("- 900001|900002|900003:14-25 -> 900004|900005|900006:1-12");

        assert_eq!(on_anilist(&relations, 900_003, 14), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&relations, 900_003, 20), Some(to(900_006, 7)));
        assert_eq!(on_anilist(&relations, 900_003, 25), Some(to(900_006, 12)));
        assert_eq!(on_anilist(&relations, 900_003, 13), None);
        assert_eq!(on_anilist(&relations, 900_003, 26), None);
    }

    #[test]
    fn an_open_range_takes_every_episode_from_its_first_on() {
        let relations = rules("- 900001|900002|900003:171-? -> 900004|900005|900006:1-?");

        assert_eq!(on_anilist(&relations, 900_003, 171), Some(to(900_006, 1)));
        assert_eq!(
            on_anilist(&relations, 900_003, 1000),
            Some(to(900_006, 830))
        );
        assert_eq!(on_anilist(&relations, 900_003, 170), None);
    }

    #[test]
    fn each_service_reads_its_own_column() {
        let relations = rules("- 900001|900002|900003:14 -> 900004|900005|900006:1");

        assert_eq!(
            relations.redirect(Service::MyAnimeList, EntryId(900_001), 14),
            Some(to(900_004, 1))
        );
        assert_eq!(
            relations.redirect(Service::Kitsu, EntryId(900_002), 14),
            Some(to(900_005, 1))
        );
        assert_eq!(on_anilist(&relations, 900_003, 14), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&relations, 900_001, 14), None);
        assert_eq!(
            relations.redirect(Service::MyAnimeList, EntryId(900_003), 14),
            None
        );
    }

    #[test]
    fn a_rule_that_ends_in_a_bang_takes_the_second_work_numbered_as_the_first() {
        // A group that numbers the second season on from the first names it
        // under its own title too.
        let bang = rules("- 900001|900002|900003:14-25 -> 900004|900005|900006:1-12!");
        let plain = rules("- 900001|900002|900003:14-25 -> 900004|900005|900006:1-12");

        assert_eq!(on_anilist(&bang, 900_006, 14), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&bang, 900_006, 25), Some(to(900_006, 12)));
        assert_eq!(on_anilist(&bang, 900_006, 1), None);
        assert_eq!(on_anilist(&bang, 900_009, 14), None);
        assert_eq!(on_anilist(&bang, 900_003, 14), Some(to(900_006, 1)));
        assert_eq!(on_anilist(&plain, 900_006, 14), None);
        assert!(bang.rules()[0].also_to_itself);
        assert!(!plain.rules()[0].also_to_itself);
    }

    #[test]
    fn a_tilde_repeats_the_number_of_the_first_work() {
        let relations = rules("- 900001|900002|900003:13 -> ~|~|~:12");

        assert_eq!(on_anilist(&relations, 900_003, 13), Some(to(900_003, 12)));
        assert_eq!(
            relations.redirect(Service::Kitsu, EntryId(900_002), 13),
            Some(to(900_002, 12))
        );
    }

    #[test]
    fn a_column_written_as_a_question_mark_takes_no_episode() {
        let relations = rules("- ?|900002|?:13-24 -> ?|900005|900006:1-12");

        assert_eq!(
            relations.redirect(Service::Kitsu, EntryId(900_002), 13),
            Some(to(900_005, 1))
        );
        assert_eq!(
            relations.redirect(Service::MyAnimeList, EntryId(900_002), 13),
            None
        );
        assert_eq!(on_anilist(&relations, 900_002, 13), None);
    }

    #[test]
    fn a_work_sent_to_as_a_question_mark_is_said_so() {
        // The rule takes the episode, and writes `?` for the AniList number
        // of where it goes: an answer apart from no rule at all.
        let relations = rules("- 900001|?|900003:13 -> 900004|?|?:1!");

        assert_eq!(
            on_anilist(&relations, 900_003, 13),
            Some(Redirect::Unnumbered)
        );
        assert_eq!(
            relations.redirect(Service::MyAnimeList, EntryId(900_001), 13),
            Some(to(900_004, 1))
        );
        assert_eq!(
            relations.redirect(Service::MyAnimeList, EntryId(900_004), 13),
            Some(to(900_004, 1))
        );
    }

    #[test]
    fn the_first_rule_in_the_file_decides_and_a_bang_counts_right_after_its_rule() {
        // The bundled file holds one such pair: a rule of `!` takes episodes
        // 60 to 75 of its second work numbered as its first, and a later rule
        // takes 17 to 75 of that work to a third.
        let relations = rules(
            "- 900001|?|900001:60-75 -> 900002|?|900002:1-16!\n\
             - 900002|?|900002:17-75 -> 900003|?|900003:1-59",
        );

        assert_eq!(on_anilist(&relations, 900_002, 60), Some(to(900_002, 1)));
        assert_eq!(on_anilist(&relations, 900_002, 59), Some(to(900_003, 43)));
    }

    #[test]
    fn a_rule_is_read_as_the_file_writes_it() {
        let relations = rules(
            "# A comment.\n\n\
             - 900001|?|900003:13-? -> ~|900005|?:1-?!",
        );

        let rule = &relations.rules()[0];
        assert_eq!(
            rule.from.work,
            [Some(EntryId(900_001)), None, Some(EntryId(900_003))]
        );
        assert_eq!(
            rule.to.work,
            [Some(EntryId(900_001)), Some(EntryId(900_005)), None]
        );
        assert_eq!(
            rule.from.episodes,
            Episodes {
                first: 13,
                last: None
            }
        );
        assert_eq!(
            rule.to.episodes,
            Episodes {
                first: 1,
                last: None
            }
        );
        assert!(rule.also_to_itself);
        assert_eq!(relations.rules().len(), 1);
    }

    #[test]
    fn a_line_written_another_way_is_refused_where_it_stands() {
        let cases = [
            (
                "900001|900002|900003:13 -> 900004|900005|900006:1",
                Reason::NotRule("900001|900002|900003:13 -> 900004|900005|900006:1".to_owned()),
            ),
            (
                "- 900001|900002|900003:13 900004|900005|900006:1",
                Reason::NoArrow,
            ),
            (
                "- 900001|900002:13 -> 900004|900005|900006:1",
                Reason::Half("900001|900002:13".to_owned()),
            ),
            (
                "- 900001|nine|900003:13 -> 900004|900005|900006:1",
                Reason::Work("nine".to_owned()),
            ),
            (
                "- ~|900002|900003:13 -> 900004|900005|900006:1",
                Reason::Work("~".to_owned()),
            ),
            (
                "- 900001|900002|900003:x -> 900004|900005|900006:1",
                Reason::Episodes("x".to_owned()),
            ),
            (
                "- 900001|900002|+900003:13 -> 900004|900005|900006:1",
                Reason::Work("+900003".to_owned()),
            ),
            (
                "- 900001|900002|900003:+13 -> 900004|900005|900006:1",
                Reason::Episodes("+13".to_owned()),
            ),
            (
                "- 900001|900002|900003:13-12 -> 900004|900005|900006:1",
                Reason::Backwards("13-12".to_owned()),
            ),
            (
                "- 900001|900002|900003:13-? -> 900004|900005|900006:1-12",
                Reason::OpenOnOneSide,
            ),
            (
                "- 900001|900002|900003:13-24 -> 900004|900005|900006:1-11",
                Reason::Lengths,
            ),
            (
                "- 900001|900002|900003:13-24 -> 900004|900005|900006:1-?",
                Reason::OpenOnOneSide,
            ),
            (
                "- 900001|900002|900003:13-? -> 900004|900005|900006:1",
                Reason::OpenOnOneSide,
            ),
            (
                "- 900001|900002|900003|900007:13 -> 900004|900005|900006:1",
                Reason::Half("900001|900002|900003|900007:13".to_owned()),
            ),
            (
                "- 900001|900002|900003:4294967296 -> 900004|900005|900006:1",
                Reason::Episodes("4294967296".to_owned()),
            ),
            (
                "- 9223372036854775808|900002|900003:13 -> 900004|900005|900006:1",
                Reason::Work("9223372036854775808".to_owned()),
            ),
        ];

        for (line, reason) in cases {
            assert_eq!(refusal(line), Error { line: 4, reason }, "{line}");
        }
    }

    #[test]
    fn the_largest_episode_a_rule_can_name_reads() {
        let relations = rules("- 900001|900002|900003:4294967295 -> 900004|900005|900006:1");

        assert_eq!(
            on_anilist(&relations, 900_003, u32::MAX),
            Some(to(900_006, 1))
        );
    }

    #[test]
    fn a_rule_that_takes_an_episode_it_cannot_number_decides_and_numbers_it_not() {
        let relations = rules(
            "- 900001|900002|900003:0-? -> 900004|900005|900006:5-?\n\
             - 900001|900002|900003:100-? -> 900007|900008|900009:1-?",
        );

        assert_eq!(
            on_anilist(&relations, 900_003, u32::MAX - 5),
            Some(to(900_006, u32::MAX))
        );
        assert_eq!(
            on_anilist(&relations, 900_003, u32::MAX),
            Some(Redirect::Unnumbered)
        );
    }

    #[test]
    fn lines_of_the_meta_section_but_its_version_are_passed_over() {
        let relations = Relations::read(
            "::meta\n- version: 1.3.0\nhello\n- 900001|900002|900003:13 -> 900004|900005|900006:1\n::rules\n",
        )
        .expect("the file reads");

        assert!(relations.rules().is_empty());
    }

    #[test]
    fn a_file_out_of_its_sections_or_with_no_version_is_refused() {
        assert_eq!(
            Relations::read(""),
            Err(Error {
                line: 0,
                reason: Reason::NoVersion,
            })
        );
        assert_eq!(
            Relations::read("- 900001|900002|900003:13 -> ~|~|~:12\n"),
            Err(Error {
                line: 1,
                reason: Reason::NoSection,
            })
        );
        assert_eq!(
            Relations::read("::meta\n- version: 1.3.0\n::other\n"),
            Err(Error {
                line: 3,
                reason: Reason::Section("other".to_owned()),
            })
        );
        assert_eq!(
            Relations::read("::meta\n- last_modified: 2026-10-11\n::rules\n"),
            Err(Error {
                line: 3,
                reason: Reason::NoVersion,
            })
        );
        assert_eq!(
            Relations::read("::meta\n- version:  \n::rules\n"),
            Err(Error {
                line: 3,
                reason: Reason::NoVersion,
            })
        );
    }

    #[test]
    fn a_refusal_names_its_line_and_what_is_wrong() {
        assert_eq!(
            refusal("- 900001|900002|900003:13-12 -> 900004|900005|900006:1").to_string(),
            "line 4: range \"13-12\" ends before it begins"
        );
    }
}
