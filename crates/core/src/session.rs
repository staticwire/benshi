//! What each player has open, and what a store is to do about it.
//!
//! A [`Session`] takes a reading together with what recognition decided
//! about it, and answers with [`Effect`]s: a question for a store, a total
//! to keep, an episode to record, a sitting to close. It carries out none of
//! them, so this crate stays free of I/O and a test reads the whole answer.
//! The caller carries each one out, in the order given, and brings the
//! answer to a question back through [`Session::resumed`].
//!
//! **A sitting is one player with one episode open in one file.** It begins
//! at the first reading that names the episode. It is over at the first
//! reading from that player that names anything else or names no episode,
//! and when the caller says the player is [gone](Session::gone). Each player
//! has a sitting of its own, and two players with one episode open are two
//! sittings, each keeping its total under the one title and episode.
//!
//! **Nothing is kept before the store has answered.** The first reading of a
//! sitting asks what was kept of the episode in the sittings before it, and
//! the count goes on from the answer. A total that is kept takes the place
//! of the one kept before it. Before the answer this sitting is all a
//! session knows of the episode, and a total of that would take the place of
//! everything the earlier sittings added up to.
//!
//! **An episode is recorded once in a sitting**, at the first reading whose
//! total the [`WatchedPolicy`] counts as watched, and that total is kept in
//! the same answer ahead of it. The sitting goes on keeping its total after
//! that and records nothing more. A sitting told by the store that the
//! episode registered already records nothing at all.
//!
//! **What names no episode begins no sitting.** That is a reading that names
//! no file, a reading recognition was not asked about, a name it refused or
//! found under several entries, a file of several episodes, and a half
//! episode. Nothing is kept or recorded for any of them, and the one effect
//! such a reading answers with is the [`Close`](Effect::Close) of a sitting it
//! ended.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::path::RawPath;
use crate::recognise::Recognition;
use crate::recognise::parse::Episode;
use crate::timeline::{Timeline, WatchedPolicy};
use crate::{MediaRef, PlayerId, PlayerSnapshot};

/// What a store is to do, in answer to a reading or to a player that is
/// gone.
///
/// A title is the title as the corpus spells it. An episode is in the
/// release's numbering, and a film has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Ask what was kept of an episode, and bring the answer to
    /// [`Session::resumed`] with the three fields of the question.
    Resume {
        /// The player that opened the episode.
        player: PlayerId,
        /// The title of what it opened.
        title: String,
        /// The episode it opened.
        episode: Option<u32>,
    },
    /// Keep the total watched of an episode, in place of the one kept
    /// before.
    Keep {
        /// The title of what is open.
        title: String,
        /// The episode that is open.
        episode: Option<u32>,
        /// The file that is open.
        media: RawPath,
        /// The total the sitting resumed from and every interval it has
        /// counted since.
        watched: Duration,
    },
    /// Record that an episode was seen.
    ///
    /// An [`Effect::Keep`] of the same episode stands ahead of it in the
    /// same answer, so a store that marks the total it holds for the episode
    /// holds one by then.
    Record {
        /// The title of what was seen.
        title: String,
        /// The episode that was seen.
        episode: Option<u32>,
        /// The file it was seen in.
        media: RawPath,
    },
    /// Close the sitting of an episode.
    Close {
        /// The title of what was open.
        title: String,
        /// The episode that was open.
        episode: Option<u32>,
    },
}

/// What a store had kept of an episode, as [`Session::resumed`] takes it.
///
/// The file the total was kept for is no part of it. A total is of the
/// episode, whichever file it was watched in, and a sitting counts on from
/// it in the file that is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resumed {
    /// What the store had kept of the episode when the question was asked.
    pub watched: Duration,
    /// Whether the episode registered on that total already. A sitting that
    /// resumes from one that did records nothing.
    pub registered: bool,
}

/// What a reading names, borrowed from the reading and the decision.
///
/// Borrowed because every reading is asked what it names and a sitting
/// begins at few of them.
#[derive(Debug, Clone, Copy)]
struct Named<'reading> {
    /// The file that is open.
    media: &'reading RawPath,
    /// The title as the corpus spells it.
    title: &'reading str,
    /// The episode, and none for a film.
    episode: Option<u32>,
}

impl<'reading> Named<'reading> {
    /// What this reading names, and nothing where it names no episode.
    ///
    /// Both matches are written out, so that an answer recognition gains and
    /// a case an episode gains each stop the build here.
    fn by(
        reading: &'reading PlayerSnapshot,
        decision: Option<&'reading Recognition>,
    ) -> Option<Self> {
        let MediaRef::LocalFile(media) = &reading.media else {
            return None;
        };
        let found = match decision? {
            Recognition::Recognised(found) => found,
            Recognition::Ambiguous(_) | Recognition::Unrecognised(_) => return None,
        };
        let episode = match found.episode {
            Episode::Only(number) => Some(number),
            Episode::Absent => None,
            // Neither is an episode to record, and with no episode either
            // would be recorded as a film.
            Episode::Several | Episode::NotWhole => return None,
        };

        Some(Self {
            media,
            title: &found.title,
            episode,
        })
    }
}

/// How far a sitting has got with its total.
#[derive(Debug)]
enum Count {
    /// The store was asked and has not answered. The reading is the last
    /// one of the sitting, which the count will go on from.
    Asked(PlayerSnapshot),
    /// The store answered, and readings are counted.
    Counting {
        /// What the readings since the answer add up to, on top of it.
        timeline: Timeline,
        /// The total in the last [`Effect::Keep`], or the answer's before
        /// the first.
        kept: Duration,
        /// Whether the episode registered, in this sitting or by the answer.
        registered: bool,
    },
}

/// One player with one episode open in one file.
#[derive(Debug)]
struct Sitting {
    /// The file that is open.
    media: RawPath,
    /// The title as the corpus spells it.
    title: String,
    /// The episode, and none for a film.
    episode: Option<u32>,
    /// How far the total has got.
    count: Count,
}

impl Sitting {
    /// A sitting that begins at this reading and asks the store.
    fn of(named: Named<'_>, reading: &PlayerSnapshot) -> Self {
        Self {
            media: named.media.clone(),
            title: named.title.to_owned(),
            episode: named.episode,
            count: Count::Asked(reading.clone()),
        }
    }

    /// Go on from what the store had, where the sitting is waiting for it.
    fn resumed(&mut self, kept: Option<Resumed>) {
        let Count::Asked(asked) = &self.count else {
            return;
        };
        let Resumed {
            watched,
            registered,
        } = kept.unwrap_or(Resumed {
            watched: Duration::ZERO,
            registered: false,
        });

        // The reading is folded in so that the next one is measured against
        // it. It adds nothing, being the first the timeline sees.
        let mut timeline = Timeline::resuming(MediaRef::LocalFile(self.media.clone()), watched);
        timeline.advance(asked);
        self.count = Count::Counting {
            timeline,
            kept: watched,
            registered,
        };
    }

    /// Fold one more reading of the sitting in, and answer with what it
    /// changes.
    ///
    /// The total is kept where it grew, and where the episode registers
    /// whether it grew or not: a total the store answered with can be past
    /// the policy before a reading adds to it.
    fn counted(&mut self, reading: &PlayerSnapshot, policy: &WatchedPolicy) -> Vec<Effect> {
        let (timeline, kept, registered) = match &mut self.count {
            Count::Asked(asked) => {
                asked.clone_from(reading);
                return Vec::new();
            }
            Count::Counting {
                timeline,
                kept,
                registered,
            } => (timeline, kept, registered),
        };

        let progress = timeline.advance(reading);
        let registers = !*registered && policy.counts_as_watched(&progress);
        let mut effects = Vec::new();
        if progress.watched > *kept || registers {
            *kept = progress.watched;
            effects.push(Effect::Keep {
                title: self.title.clone(),
                episode: self.episode,
                media: self.media.clone(),
                watched: progress.watched,
            });
        }
        if registers {
            *registered = true;
            effects.push(Effect::Record {
                title: self.title.clone(),
                episode: self.episode,
                media: self.media.clone(),
            });
        }

        effects
    }

    /// Whether a reading that names this is a reading of this sitting.
    fn is_of(&self, named: Named<'_>) -> bool {
        self.media == *named.media && self.title == named.title && self.episode == named.episode
    }

    /// The question the sitting begins with.
    fn resume(&self, player: &PlayerId) -> Effect {
        Effect::Resume {
            player: player.clone(),
            title: self.title.clone(),
            episode: self.episode,
        }
    }

    /// What the sitting ends with.
    fn close(self) -> Effect {
        Effect::Close {
            title: self.title,
            episode: self.episode,
        }
    }
}

/// The sittings that are open, one for each player with an episode open.
#[derive(Debug)]
pub struct Session {
    /// What each player has open.
    sittings: BTreeMap<PlayerId, Sitting>,
    /// When an episode counts as watched.
    policy: WatchedPolicy,
}

impl Session {
    /// A session with nothing open, which decides by this policy.
    #[must_use]
    pub const fn new(policy: WatchedPolicy) -> Self {
        Self {
            sittings: BTreeMap::new(),
            policy,
        }
    }

    /// Take one reading with what was decided about it, and answer with what
    /// a store is to do.
    ///
    /// `decision` is what recognition answered for the file the reading
    /// names, and nothing where recognition was not asked. The effects are
    /// in the order they are to be carried out: a sitting that is over is
    /// closed before the next one asks, and a total is kept before its
    /// episode is recorded.
    #[must_use = "an effect that is dropped is one nobody carries out"]
    pub fn advance(
        &mut self,
        reading: &PlayerSnapshot,
        decision: Option<&Recognition>,
    ) -> Vec<Effect> {
        let named = Named::by(reading, decision);
        let mut effects = Vec::new();

        let over = self
            .sittings
            .get(&reading.player)
            .is_some_and(|sitting| named.is_none_or(|named| !sitting.is_of(named)));
        if over && let Some(sitting) = self.sittings.remove(&reading.player) {
            effects.push(sitting.close());
        }

        let Some(named) = named else {
            return effects;
        };
        if let Some(sitting) = self.sittings.get_mut(&reading.player) {
            effects.extend(sitting.counted(reading, &self.policy));
        } else {
            let sitting = Sitting::of(named, reading);
            effects.push(sitting.resume(&reading.player));
            self.sittings.insert(reading.player.clone(), sitting);
        }

        effects
    }

    /// Take the answer to an [`Effect::Resume`].
    ///
    /// `player`, `title` and `episode` are the question's own, and `kept` is
    /// what the store had of the episode, with nothing where it had none.
    /// The count goes on from the total in the answer and from the last
    /// reading before it. What the sitting played before that reading is not
    /// counted. The interval from it is counted as [`Timeline`] counts any
    /// interval: as the state that reading reported, and not at all where it
    /// is longer than [`OBSERVED_LIMIT`](crate::timeline::OBSERVED_LIMIT).
    ///
    /// An answer is dropped where the player no longer has that episode
    /// open, and where the sitting has its answer already. Taken, the first
    /// would give one episode what was watched of another.
    pub fn resumed(
        &mut self,
        player: &PlayerId,
        title: &str,
        episode: Option<u32>,
        kept: Option<Resumed>,
    ) {
        if let Some(sitting) = self.sittings.get_mut(player)
            && sitting.title == title
            && sitting.episode == episode
        {
            sitting.resumed(kept);
        }
    }

    /// Close what these players had open.
    ///
    /// For a player that left and for one that has nothing open any more,
    /// neither of which sends a reading to say so. A player with no sitting
    /// is passed over.
    #[must_use = "an effect that is dropped is one nobody carries out"]
    pub fn gone(&mut self, players: &[PlayerId]) -> Vec<Effect> {
        players
            .iter()
            .filter_map(|player| self.sittings.remove(player))
            .map(Sitting::close)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Effect, Resumed, Session};
    use crate::clock::{Clock, TestClock};
    use crate::path::RawPath;
    use crate::recognise::altname::Altnames;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::index::Index;
    use crate::recognise::parse::{Episode, parse};
    use crate::recognise::{Ambiguity, Match, Recognition, Refusal, Stage, decide};
    use crate::timeline::{WATCHED_FALLBACK, WatchedPolicy};
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;

    /// The interval between two readings of one player.
    const SECOND: Duration = Duration::from_secs(1);

    /// The length every reading here reports.
    const LENGTH: Duration = Duration::from_secs(200);

    /// The reading of a sitting that reaches what [`half`] asks of [`LENGTH`].
    ///
    /// Half of two hundred seconds is a hundred, above the minute that is
    /// the least a policy asks of a file this long. The first reading of a
    /// sitting adds nothing, so with readings a second apart the hundredth
    /// second is watched at the reading of this index.
    const ASKED_AT: usize = 100;

    /// The title the list holds for the series.
    const SERIES: &str = "Show Title";

    /// The title the list holds for the film.
    const FILM: &str = "A Film";

    /// The title the list holds for a second series.
    const SECOND_SERIES: &str = "Second Series";

    const THIRD: &str = "/anime/[Group] Show Title - 03 [1080p].mkv";
    const THIRD_BY_OTHERS: &str = "/anime/[Others] Show Title - 03 [720p].mkv";
    const FOURTH: &str = "/anime/[Group] Show Title - 04 [1080p].mkv";
    const THIRD_OF_THE_SECOND: &str = "/anime/[Group] Second Series - 03 [1080p].mkv";
    const THE_FILM: &str = "/anime/[Group] A Film [1080p].mkv";
    const A_BATCH: &str = "/anime/[Group] Show Title 01-12 [BD].mkv";
    const A_HALF: &str = "/anime/[Group] Show Title - 05.5 [1080p].mkv";
    const NOT_LISTED: &str = "/anime/[Group] Some Other Show - 03 [1080p].mkv";

    /// What a run answers where it answers nothing, named because an empty
    /// array otherwise needs its element type spelt out at every use.
    const NOTHING: [Effect; 0] = [];

    /// The readings that recorded an episode in a run that recorded none.
    const AT_NO_READING: [usize; 0] = [];

    /// A policy asking for half of the length, written out so that no test
    /// here moves with the default.
    fn half() -> WatchedPolicy {
        WatchedPolicy::new(50, WATCHED_FALLBACK).expect("half is in range, and so is the fallback")
    }

    fn a_path(name: &str) -> RawPath {
        RawPath::from_bytes(name.as_bytes().to_vec())
    }

    /// One player, read a second apart on a clock of its own.
    struct Player {
        id: PlayerId,
        clock: TestClock,
    }

    impl Player {
        fn named(id: &str) -> Self {
            Self {
                id: PlayerId(id.to_owned()),
                clock: TestClock::new(),
            }
        }

        /// The next reading, a second after the one before it.
        ///
        /// No position, because nothing here is about one: watched time is
        /// counted from the instants and the states.
        fn reading(&self, media: MediaRef, state: PlayState) -> PlayerSnapshot {
            self.clock.advance(SECOND);
            PlayerSnapshot {
                player: self.id.clone(),
                media,
                state,
                position: Known::NotReported,
                duration: Known::Value(LENGTH),
                observed_at: self.clock.now(),
            }
        }

        fn playing(&self, name: &str) -> PlayerSnapshot {
            self.reading(MediaRef::LocalFile(a_path(name)), PlayState::Playing)
        }

        fn paused(&self, name: &str) -> PlayerSnapshot {
            self.reading(MediaRef::LocalFile(a_path(name)), PlayState::Paused)
        }

        fn stopped(&self, name: &str) -> PlayerSnapshot {
            self.reading(MediaRef::LocalFile(a_path(name)), PlayState::Stopped)
        }
    }

    /// What recognition answers for the file a reading names, against a list
    /// of two series and one film, and nothing where it names no file.
    ///
    /// The name and never the directory, which is how the daemon asks.
    fn decided(reading: &PlayerSnapshot) -> Option<Recognition> {
        let MediaRef::LocalFile(path) = &reading.media else {
            return None;
        };
        let list: Corpus = [SERIES, SECOND_SERIES, FILM].into_iter().collect();
        let name = RawPath::from_bytes(path.file_name().to_vec());

        Some(decide(
            &parse(&name),
            &Altnames::new(),
            &list,
            &Index::of(&list),
        ))
    }

    /// A decision written out by hand, for a test that is about the decision.
    fn recognised(title: &str, episode: Episode) -> Recognition {
        Recognition::Recognised(Match {
            title: title.to_owned(),
            episode,
            stage: Stage::Key,
        })
    }

    /// What the session answers to one reading, with no answer given back.
    fn advance(session: &mut Session, reading: &PlayerSnapshot) -> Vec<Effect> {
        session.advance(reading, decided(reading).as_ref())
    }

    /// What the session answers to one reading, with `kept` given back to
    /// every question it asks before anything else happens.
    fn answering(
        session: &mut Session,
        reading: &PlayerSnapshot,
        kept: Option<Resumed>,
    ) -> Vec<Effect> {
        let effects = advance(session, reading);
        for effect in &effects {
            if let Effect::Resume {
                player,
                title,
                episode,
            } = effect
            {
                session.resumed(player, title, *episode, kept);
            }
        }
        effects
    }

    /// A run of readings of one file, all playing, and what each was answered.
    fn watch(
        session: &mut Session,
        player: &Player,
        name: &str,
        readings: usize,
        kept: Option<Resumed>,
    ) -> Vec<Vec<Effect>> {
        (0..readings)
            .map(|_| answering(session, &player.playing(name), kept))
            .collect()
    }

    /// The readings of a run that recorded an episode.
    fn recorded_at(answers: &[Vec<Effect>]) -> Vec<usize> {
        answers
            .iter()
            .enumerate()
            .filter(|(_, effects)| {
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Record { .. }))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn resume(player: &Player, title: &str, episode: Option<u32>) -> Effect {
        Effect::Resume {
            player: player.id.clone(),
            title: title.to_owned(),
            episode,
        }
    }

    fn keep(title: &str, episode: Option<u32>, name: &str, seconds: u64) -> Effect {
        Effect::Keep {
            title: title.to_owned(),
            episode,
            media: a_path(name),
            watched: Duration::from_secs(seconds),
        }
    }

    fn record(title: &str, episode: Option<u32>, name: &str) -> Effect {
        Effect::Record {
            title: title.to_owned(),
            episode,
            media: a_path(name),
        }
    }

    fn close(title: &str, episode: Option<u32>) -> Effect {
        Effect::Close {
            title: title.to_owned(),
            episode,
        }
    }

    #[test]
    fn the_first_reading_of_an_episode_asks_what_was_kept_of_it() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [resume(&player, SERIES, Some(3))]);
    }

    #[test]
    fn a_sitting_begins_at_a_reading_whatever_state_it_reports() {
        // What begins a sitting is what the reading names. A player that
        // opens a file paused, or reports it stopped, has it open.
        for state in [PlayState::Playing, PlayState::Paused, PlayState::Stopped] {
            let mut session = Session::new(half());
            let player = Player::named("mpv");
            let reading = player.reading(MediaRef::LocalFile(a_path(THIRD)), state);

            let effects = advance(&mut session, &reading);

            assert_eq!(effects, [resume(&player, SERIES, Some(3))], "{state:?}");
        }
    }

    #[test]
    fn nothing_is_kept_before_the_question_is_answered() {
        // A total kept here would hold this sitting alone, and the store puts
        // what it is given in the place of what it had. The first assertion is
        // what stops the second from passing over a session that answers
        // nothing at all.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let first = advance(&mut session, &player.playing(THIRD));
        let after: Vec<Effect> = (0..5)
            .flat_map(|_| advance(&mut session, &player.playing(THIRD)))
            .collect();

        assert_eq!(first, [resume(&player, SERIES, Some(3))]);
        assert_eq!(after, NOTHING);
    }

    #[test]
    fn the_count_goes_on_from_the_answer_and_from_the_reading_that_asked() {
        // The second between the reading that asked and the one after it was
        // played, so it is counted on top of what the store had.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Resumed {
            watched: Duration::from_secs(61),
            registered: false,
        };

        answering(&mut session, &player.playing(THIRD), Some(kept));
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 62)]);
    }

    #[test]
    fn with_nothing_kept_the_count_starts_from_nothing() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        answering(&mut session, &player.playing(THIRD), None);
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 1)]);
    }

    #[test]
    fn an_answer_that_comes_late_counts_from_the_last_reading_before_it() {
        // Three readings and then the answer. The two seconds between them
        // are not counted, and the second after the third of them is.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        for _ in 0..3 {
            advance(&mut session, &player.playing(THIRD));
        }
        session.resumed(&player.id, SERIES, Some(3), None);
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 1)]);
    }

    #[test]
    fn an_answer_given_twice_is_taken_once() {
        // The second answer would start the count again from what it says.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let again = Resumed {
            watched: Duration::from_secs(50),
            registered: false,
        };

        watch(&mut session, &player, THIRD, 4, None);
        session.resumed(&player.id, SERIES, Some(3), Some(again));
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 4)]);
    }

    #[test]
    fn an_answer_about_an_episode_no_longer_open_is_dropped() {
        // The player moved on before the answer about the third episode
        // arrived: to the next episode, where the episode alone differs, and
        // to the third of another series, where the title alone does. Taken,
        // the answer would give what is open now what was watched of the
        // third, and register it.
        let of_the_third = Resumed {
            watched: Duration::from_secs(99),
            registered: false,
        };

        for (next, title, episode) in [
            (FOURTH, SERIES, Some(4)),
            (THIRD_OF_THE_SECOND, SECOND_SERIES, Some(3)),
        ] {
            let mut session = Session::new(half());
            let player = Player::named("mpv");

            advance(&mut session, &player.playing(THIRD));
            advance(&mut session, &player.playing(next));
            session.resumed(&player.id, SERIES, Some(3), Some(of_the_third));
            let still_asking = advance(&mut session, &player.playing(next));
            session.resumed(&player.id, title, episode, None);
            let counted = advance(&mut session, &player.playing(next));

            assert_eq!(still_asking, NOTHING, "moved on to {next}");
            assert_eq!(counted, [keep(title, episode, next, 1)]);
        }
    }

    #[test]
    fn a_total_the_store_holds_already_is_not_kept_again() {
        // Paused from the reading that asked, so the first two readings
        // after the answer add nothing to it, and the store holds that total
        // as it is. The interval out of the pause began paused and adds
        // nothing either. The last assertion is what shows the sitting
        // counts at all.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Resumed {
            watched: Duration::from_secs(61),
            registered: false,
        };

        answering(&mut session, &player.paused(THIRD), Some(kept));
        let paused = advance(&mut session, &player.paused(THIRD));
        let out_of_the_pause = advance(&mut session, &player.playing(THIRD));
        let playing = advance(&mut session, &player.playing(THIRD));

        assert_eq!(paused, NOTHING);
        assert_eq!(out_of_the_pause, NOTHING);
        assert_eq!(playing, [keep(SERIES, Some(3), THIRD, 62)]);
    }

    #[test]
    fn a_reading_that_adds_nothing_keeps_nothing() {
        // An interval counts as the state it began in. The one into the pause
        // began playing and is kept; the ones that began paused or stopped
        // add nothing, the one out of the stop included, and a store written
        // to every second of a pause is written to for nothing. The last
        // interval began playing and is kept on top of the first.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        answering(&mut session, &player.playing(THIRD), None);
        let into_the_pause = advance(&mut session, &player.paused(THIRD));
        let inside_it = advance(&mut session, &player.paused(THIRD));
        let into_the_stop = advance(&mut session, &player.stopped(THIRD));
        let inside_the_stop = advance(&mut session, &player.stopped(THIRD));
        let out_of_it = advance(&mut session, &player.playing(THIRD));
        let playing = advance(&mut session, &player.playing(THIRD));

        assert_eq!(into_the_pause, [keep(SERIES, Some(3), THIRD, 1)]);
        assert_eq!(inside_it, NOTHING);
        assert_eq!(into_the_stop, NOTHING);
        assert_eq!(inside_the_stop, NOTHING);
        assert_eq!(out_of_it, NOTHING);
        assert_eq!(playing, [keep(SERIES, Some(3), THIRD, 2)]);
    }

    #[test]
    fn an_episode_is_recorded_once_at_the_reading_that_reaches_the_policy() {
        // Which reading and how many times, together: the readings after it
        // are past the policy too, and each of them would record the episode
        // again. The total is kept before the episode is recorded, in that
        // order, because the store marks the total it holds.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let answers = watch(&mut session, &player, THIRD, 150, None);

        assert_eq!(recorded_at(&answers), [ASKED_AT]);
        assert_eq!(answers[ASKED_AT - 1], [keep(SERIES, Some(3), THIRD, 99)]);
        assert_eq!(
            answers[ASKED_AT],
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
        assert_eq!(answers[ASKED_AT + 1], [keep(SERIES, Some(3), THIRD, 101)]);
    }

    #[test]
    fn the_policy_a_session_is_given_is_the_one_it_decides_by() {
        // Four fifths of two hundred seconds. A session deciding by half
        // would record the episode sixty readings earlier.
        let four_fifths = WatchedPolicy::new(80, WATCHED_FALLBACK)
            .expect("four fifths is the most that is in range");
        let mut session = Session::new(four_fifths);
        let player = Player::named("mpv");

        let answers = watch(&mut session, &player, THIRD, 200, None);

        assert_eq!(recorded_at(&answers), [160]);
    }

    #[test]
    fn two_sittings_record_an_episode_neither_reaches_alone() {
        // Sixty seconds and then forty. The first sitting closes short of the
        // policy, and the second is given what the first kept.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let first = watch(&mut session, &player, THIRD, 61, None);
        let closed = session.gone(std::slice::from_ref(&player.id));
        let kept = Resumed {
            watched: Duration::from_secs(60),
            registered: false,
        };
        let second = watch(&mut session, &player, THIRD, 61, Some(kept));

        assert_eq!(first[60], [keep(SERIES, Some(3), THIRD, 60)]);
        assert_eq!(recorded_at(&first), AT_NO_READING);
        assert_eq!(closed, [close(SERIES, Some(3))]);
        assert_eq!(recorded_at(&second), [40]);
        assert_eq!(
            second[40],
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn a_total_past_the_policy_already_is_kept_and_then_recorded() {
        // The total the store answers with can be past the policy with the
        // episode not registered, where the policy in force when it was kept
        // asked for more. Both readings are paused, so the second adds
        // nothing, and the total is kept all the same: the store marks the
        // total it holds, and the one it holds has to be this one.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Resumed {
            watched: Duration::from_secs(150),
            registered: false,
        };

        answering(&mut session, &player.paused(THIRD), Some(kept));
        let effects = advance(&mut session, &player.paused(THIRD));

        assert_eq!(
            effects,
            [
                keep(SERIES, Some(3), THIRD, 150),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn an_episode_that_registered_before_is_not_recorded_again() {
        // What a process that was killed leaves: the episode registered and
        // the sitting never closed. The sitting that begins after it goes on
        // from that total, which the second assertion shows, and has nothing
        // to record.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Resumed {
            watched: Duration::from_secs(120),
            registered: true,
        };

        let answers = watch(&mut session, &player, THIRD, 30, Some(kept));

        assert_eq!(recorded_at(&answers), AT_NO_READING);
        assert_eq!(answers[1], [keep(SERIES, Some(3), THIRD, 121)]);
    }

    #[test]
    fn another_file_closes_the_episode_and_asks_about_the_next() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        watch(&mut session, &player, THIRD, 3, None);
        let effects = advance(&mut session, &player.playing(FOURTH));

        assert_eq!(
            effects,
            [close(SERIES, Some(3)), resume(&player, SERIES, Some(4))]
        );
    }

    #[test]
    fn another_release_of_the_episode_is_another_sitting_and_goes_on_from_the_total() {
        // The total is kept for the episode, whichever file it was watched
        // in. The second release closes the sitting of the first, asks about
        // the same episode, and counts on from what the first one kept.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Resumed {
            watched: Duration::from_secs(61),
            registered: false,
        };

        watch(&mut session, &player, THIRD, 3, None);
        let moved = answering(&mut session, &player.playing(THIRD_BY_OTHERS), Some(kept));
        let counted = advance(&mut session, &player.playing(THIRD_BY_OTHERS));

        assert_eq!(
            moved,
            [close(SERIES, Some(3)), resume(&player, SERIES, Some(3))]
        );
        assert_eq!(counted, [keep(SERIES, Some(3), THIRD_BY_OTHERS, 62)]);
    }

    #[test]
    fn a_player_that_is_gone_closes_what_it_had_open_once() {
        // The second call finds nothing open, and the file opened again is a
        // sitting of its own that asks again.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let gone = std::slice::from_ref(&player.id);

        watch(&mut session, &player, THIRD, 3, None);
        let closed = session.gone(gone);
        let closed_again = session.gone(gone);
        let opened_again = advance(&mut session, &player.playing(THIRD));

        assert_eq!(closed, [close(SERIES, Some(3))]);
        assert_eq!(closed_again, NOTHING);
        assert_eq!(opened_again, [resume(&player, SERIES, Some(3))]);
    }

    #[test]
    fn two_players_are_two_sittings() {
        // One of them leaving closes its own episode and leaves the count of
        // the other where it was.
        let mut session = Session::new(half());
        let mpv = Player::named("mpv");
        let vlc = Player::named("vlc");

        watch(&mut session, &mpv, THIRD, 2, None);
        watch(&mut session, &vlc, FOURTH, 2, None);
        let closed = session.gone(std::slice::from_ref(&mpv.id));
        let the_other = advance(&mut session, &vlc.playing(FOURTH));

        assert_eq!(closed, [close(SERIES, Some(3))]);
        assert_eq!(the_other, [keep(SERIES, Some(4), FOURTH, 2)]);
    }

    #[test]
    fn every_player_that_is_gone_closes_what_it_had_open() {
        // Two of three leave in one round. Each closes its own episode, in
        // the order they were named, and the third counts on.
        let mut session = Session::new(half());
        let mpv = Player::named("mpv");
        let vlc = Player::named("vlc");
        let celluloid = Player::named("celluloid");

        watch(&mut session, &mpv, THIRD, 2, None);
        watch(&mut session, &vlc, FOURTH, 2, None);
        watch(&mut session, &celluloid, THE_FILM, 2, None);
        let closed = session.gone(&[vlc.id.clone(), mpv.id.clone()]);
        let the_third = advance(&mut session, &celluloid.playing(THE_FILM));

        assert_eq!(closed, [close(SERIES, Some(4)), close(SERIES, Some(3))]);
        assert_eq!(the_third, [keep(FILM, None, THE_FILM, 2)]);
    }

    #[test]
    fn a_film_is_recorded_with_no_episode() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        assert_eq!(
            decided(&player.playing(THE_FILM)),
            Some(recognised(FILM, Episode::Absent))
        );

        let answers = watch(&mut session, &player, THE_FILM, 150, None);

        assert_eq!(answers[0], [resume(&player, FILM, None)]);
        assert_eq!(recorded_at(&answers), [ASKED_AT]);
        assert_eq!(
            answers[ASKED_AT],
            [
                keep(FILM, None, THE_FILM, 100),
                record(FILM, None, THE_FILM)
            ]
        );
    }

    #[test]
    fn a_file_of_several_episodes_is_not_recorded() {
        // The title is recognised, which the first assertion holds, so what
        // is refused is the episode: a file of twelve is neither the first
        // nor the twelfth, and with no episode it would arrive as a film.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        assert_eq!(
            decided(&player.playing(A_BATCH)),
            Some(recognised(SERIES, Episode::Several))
        );

        let answers = watch(&mut session, &player, A_BATCH, 150, None);

        assert_eq!(answers.concat(), NOTHING);
    }

    #[test]
    fn a_half_episode_is_not_recorded() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        assert_eq!(
            decided(&player.playing(A_HALF)),
            Some(recognised(SERIES, Episode::NotWhole))
        );

        let answers = watch(&mut session, &player, A_HALF, 150, None);

        assert_eq!(answers.concat(), NOTHING);
    }

    #[test]
    fn a_name_the_list_does_not_hold_is_not_recorded() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        assert!(matches!(
            decided(&player.playing(NOT_LISTED)),
            Some(Recognition::Unrecognised(_))
        ));

        let answers = watch(&mut session, &player, NOT_LISTED, 150, None);

        assert_eq!(answers.concat(), NOTHING);
    }

    #[test]
    fn a_name_that_reaches_several_entries_is_not_recorded() {
        // Neither candidate is taken, the first included.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let either = Recognition::Ambiguous(Ambiguity {
            parsed: SERIES.to_owned(),
            candidates: vec![SERIES.to_owned(), "Show Title (2019)".to_owned()],
        });

        let effects: Vec<Effect> = (0..150)
            .flat_map(|_| session.advance(&player.playing(THIRD), Some(&either)))
            .collect();

        assert_eq!(effects, NOTHING);
    }

    #[test]
    fn a_reading_nothing_was_decided_about_is_not_recorded() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let effects: Vec<Effect> = (0..150)
            .flat_map(|_| session.advance(&player.playing(THIRD), None))
            .collect();

        assert_eq!(effects, NOTHING);
    }

    #[test]
    fn a_reading_that_names_no_file_is_not_recorded_whatever_was_decided() {
        // What is kept and what is recorded both carry the file. An address
        // is not one, and neither is a title with nothing underneath it.
        let decision = recognised(SERIES, Episode::Only(3));

        for media in [
            MediaRef::Remote("https://example.invalid/show-title-03.m3u8".to_owned()),
            MediaRef::Title("Show Title - 03".to_owned()),
        ] {
            let mut session = Session::new(half());
            let player = Player::named("mpv");

            let effects: Vec<Effect> = (0..150)
                .flat_map(|_| {
                    session.advance(
                        &player.reading(media.clone(), PlayState::Playing),
                        Some(&decision),
                    )
                })
                .collect();

            assert_eq!(effects, NOTHING, "{media:?}");
        }
    }

    #[test]
    fn a_file_decided_otherwise_closes_one_sitting_and_begins_another() {
        // The file stays open and what it is changes under it, as it does
        // when a name is assigned by hand while the file plays. The episode
        // alone changes first, then the title alone, then the file is
        // nothing the list holds, and then it is what it was at the start.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let the_third = recognised(SERIES, Episode::Only(3));
        let the_fourth = recognised(SERIES, Episode::Only(4));
        let of_the_second = recognised(SECOND_SERIES, Episode::Only(4));
        let nothing = Recognition::Unrecognised(Refusal {
            parsed: SERIES.to_owned(),
            best: None,
        });

        let answers: Vec<Vec<Effect>> = [
            &the_third,
            &the_fourth,
            &of_the_second,
            &nothing,
            &the_third,
        ]
        .into_iter()
        .map(|decision| session.advance(&player.playing(THIRD), Some(decision)))
        .collect();

        assert_eq!(answers[0], [resume(&player, SERIES, Some(3))]);
        assert_eq!(
            answers[1],
            [close(SERIES, Some(3)), resume(&player, SERIES, Some(4))]
        );
        assert_eq!(
            answers[2],
            [
                close(SERIES, Some(4)),
                resume(&player, SECOND_SERIES, Some(4))
            ]
        );
        assert_eq!(answers[3], [close(SECOND_SERIES, Some(4))]);
        assert_eq!(answers[4], [resume(&player, SERIES, Some(3))]);
    }
}
