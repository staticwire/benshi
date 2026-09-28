//! What is open in the players, and what a store is to do about it.
//!
//! A [`Session`] takes a reading together with what recognition decided
//! about it, and answers with [`Effect`]s: a question for a store, a total
//! to keep, an episode to record, a sitting to close. It carries out none of
//! them, so this crate stays free of I/O and a test reads the whole answer.
//! The caller carries each one out, in the order given, and brings the
//! answer to a question back through [`Session::resumed`].
//!
//! **A sitting is one episode open, in one player or in several.** It begins
//! when a player opens an episode no player has open, and it is over when
//! the last player that has the episode open leaves it. A player leaves at
//! its first reading that names another episode or none, and when the caller
//! says it is [gone](Session::gone). A reading that names the same episode in
//! another file leaves nothing: the player goes on in the sitting with the
//! file it has open now.
//!
//! **The total and the mark are the episode's.** A store keeps one total for
//! an episode, so a sitting asks one question, keeps one total and records
//! the episode once, however many players count into it.
//!
//! **Time that was played is counted once.** A sitting holds the latest
//! instant it has counted up to, and an interval a player played adds the
//! part of itself that lies after that instant. Two players playing one
//! episode at once add a second a second. One that sits paused beside one
//! that plays adds nothing and takes nothing. What is counted is never more
//! than what was played, and it is less where an interval of one player is
//! handed over after a later one of another. A player that was silent for
//! some rounds loses what it played before the other began, at one reading
//! no more than [`OBSERVED_LIMIT`](crate::timeline::OBSERVED_LIMIT), and a
//! player stamped earlier than another in every round loses the time
//! between the two stamps in every round. The instants of two players are
//! compared here, so **every reading a session is given has to be stamped
//! from one clock**.
//!
//! **Nothing is kept before the store has answered.** The player that begins
//! a sitting asks what was kept of the episode in the sittings before it,
//! and the count goes on from the answer. A total that is kept takes the
//! place of the one kept before it. Before the answer this sitting is all a
//! session knows of the episode, and a total of that would take the place of
//! everything the earlier sittings added up to.
//!
//! **An episode is recorded once in a sitting**, at the first reading that
//! adds to the total and leaves it where the [`WatchedPolicy`] counts it as
//! watched. The total is kept in the same answer ahead of it. The policy is
//! asked with the length of the file that reading is of, which is the last
//! length a reading of that file reported, so a reading that reports no
//! length is decided by the length its file reported before, and the
//! policy's fallback decides only where no reading of the file has reported
//! one. A reading that adds nothing records nothing. A sitting told by the
//! store that the episode registered already records nothing at all.
//!
//! **What names no episode begins no sitting.** That is a reading that names
//! no file, a reading recognition was not asked about, a name it refused or
//! found under several entries, a file of several episodes, and a half
//! episode. Nothing is kept or recorded for any of them, and the one effect
//! such a reading answers with is the [`Close`](Effect::Close) of a sitting
//! its player was the last to leave.

use std::mem;
use std::time::Duration;

use crate::clock::Timestamp;
use crate::path::RawPath;
use crate::recognise::Recognition;
use crate::recognise::parse::Episode;
use crate::timeline::{Progress, Timeline, WatchedPolicy};
use crate::{Known, MediaRef, PlayerId, PlayerSnapshot};

/// What a store is to do, in answer to a reading or to a player that is
/// gone.
///
/// A title is the title as the corpus spells it. An episode is in the
/// release's numbering, and a film has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Ask what was kept of an episode, and bring the answer to
    /// [`Session::resumed`] with the question.
    Resume {
        /// What the answer is handed back with.
        question: Question,
        /// The title of what was opened.
        title: String,
        /// The episode that was opened.
        episode: Option<u32>,
    },
    /// Keep the total watched of an episode, in place of the one kept
    /// before.
    Keep {
        /// The title of what is open.
        title: String,
        /// The episode that is open.
        episode: Option<u32>,
        /// The file the reading that added to the total is of.
        media: RawPath,
        /// The total the sitting resumed from and everything it has counted
        /// since.
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
        /// The file the reading that reached the policy is of.
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

/// One question a session asked, which the answer to it is handed back
/// with.
///
/// A question belongs to the sitting that asked it. A sitting of the same
/// episode that begins after that one closed asks a question of its own, and
/// the answer to the earlier one is about what a store held before the
/// close.
///
/// It tells the sittings of one session apart and no more. Every session
/// numbers its questions from one, so a question kept from one session and
/// handed to another is taken by whichever sitting there carries its number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Question(u64);

/// What a store had kept of an episode, as [`Session::resumed`] takes it.
///
/// The file the total was kept for is no part of it. A total is of the
/// episode, whichever file it was watched in, and a sitting counts on from
/// it in the files that are open.
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

/// The last reading of a player, held while the store has not answered.
#[derive(Debug)]
struct Held {
    /// The file the reading is of.
    media: RawPath,
    /// The reading the player's count will go on from.
    reading: PlayerSnapshot,
}

/// One player counting into a sitting.
#[derive(Debug)]
struct Counted {
    /// Whose readings these are.
    player: PlayerId,
    /// The file the player has open.
    media: RawPath,
    /// What the player's readings of this file say was played.
    timeline: Timeline,
    /// What the timeline had counted at the player's reading before.
    so_far: Duration,
    /// The instant of the player's reading before.
    at: Timestamp,
    /// The last length a reading of this file reported, and what the first
    /// one answered where none has.
    length: Known<Duration>,
}

impl Counted {
    /// A count that begins at this reading of this file.
    ///
    /// The reading is folded in so that the next one is measured against
    /// it. It adds nothing, being the first the timeline sees.
    fn from(media: RawPath, reading: &PlayerSnapshot) -> Self {
        let mut timeline = Timeline::new();
        let progress = timeline.advance(reading);

        Self {
            player: reading.player.clone(),
            media,
            timeline,
            so_far: progress.watched,
            at: reading.observed_at,
            length: progress.duration,
        }
    }
}

/// How far a sitting has got with its total.
#[derive(Debug)]
enum Count {
    /// The store was asked and has not answered.
    Asked {
        /// What the answer will be handed back with.
        question: Question,
        /// The last reading of every player that has the episode open.
        held: Vec<Held>,
    },
    /// The store answered, and readings are counted.
    Counting {
        /// The total the store answered with and everything counted since.
        watched: Duration,
        /// Whether the episode registered, in this sitting or by the answer.
        registered: bool,
        /// The latest instant played time has been counted up to, and the
        /// epoch where none has been.
        until: Timestamp,
        /// Every player that has the episode open.
        players: Vec<Counted>,
    },
}

/// One episode open, in one player or in several.
#[derive(Debug)]
struct Sitting {
    /// The title as the corpus spells it.
    title: String,
    /// The episode, and none for a film.
    episode: Option<u32>,
    /// How far the total has got, and who counts into it.
    count: Count,
}

impl Sitting {
    /// A sitting that begins at this reading and asks the store.
    fn asking(question: Question, named: Named<'_>, reading: &PlayerSnapshot) -> Self {
        Self {
            title: named.title.to_owned(),
            episode: named.episode,
            count: Count::Asked {
                question,
                held: vec![Held {
                    media: named.media.clone(),
                    reading: reading.clone(),
                }],
            },
        }
    }

    /// Whether a reading that names this is a reading of this episode.
    fn is_of(&self, named: Named<'_>) -> bool {
        self.title == named.title && self.episode == named.episode
    }

    /// Whether this player has the episode open.
    fn has(&self, player: &PlayerId) -> bool {
        match &self.count {
            Count::Asked { held, .. } => held.iter().any(|held| held.reading.player == *player),
            Count::Counting { players, .. } => {
                players.iter().any(|counted| counted.player == *player)
            }
        }
    }

    /// Take in a player that opens the episode at this reading.
    fn join(&mut self, media: &RawPath, reading: &PlayerSnapshot) {
        match &mut self.count {
            Count::Asked { held, .. } => held.push(Held {
                media: media.clone(),
                reading: reading.clone(),
            }),
            Count::Counting { players, .. } => players.push(Counted::from(media.clone(), reading)),
        }
    }

    /// Let a player go, and say whether it was the last.
    fn leave(&mut self, player: &PlayerId) -> bool {
        match &mut self.count {
            Count::Asked { held, .. } => {
                held.retain(|held| held.reading.player != *player);
                held.is_empty()
            }
            Count::Counting { players, .. } => {
                players.retain(|counted| counted.player != *player);
                players.is_empty()
            }
        }
    }

    /// Go on from what the store had, where the answer is to the question
    /// this sitting is waiting on: every player from the reading it holds.
    fn answered(&mut self, to: Question, kept: Option<Resumed>) {
        let Count::Asked { question, held } = &mut self.count else {
            return;
        };
        if *question != to {
            return;
        }
        let Resumed {
            watched,
            registered,
        } = kept.unwrap_or(Resumed {
            watched: Duration::ZERO,
            registered: false,
        });

        let players = mem::take(held)
            .into_iter()
            .map(|held| Counted::from(held.media, &held.reading))
            .collect();
        self.count = Count::Counting {
            watched,
            registered,
            until: Timestamp::epoch(),
            players,
        };
    }

    /// Take one more reading of a player that has the episode open, and
    /// answer with what it changes.
    fn read(
        &mut self,
        media: &RawPath,
        reading: &PlayerSnapshot,
        policy: &WatchedPolicy,
    ) -> Vec<Effect> {
        let (watched, registered, until, players) = match &mut self.count {
            Count::Asked { held, .. } => {
                if let Some(held) = held
                    .iter_mut()
                    .find(|held| held.reading.player == reading.player)
                {
                    held.media.clone_from(media);
                    held.reading.clone_from(reading);
                }
                return Vec::new();
            }
            Count::Counting {
                watched,
                registered,
                until,
                players,
            } => (watched, registered, until, players),
        };
        let Some(player) = players
            .iter_mut()
            .find(|counted| counted.player == reading.player)
        else {
            return Vec::new();
        };
        // Another file of the episode. The interval the change fell inside
        // is of neither file, and the length is the new file's to report.
        if player.media != *media {
            *player = Counted::from(media.clone(), reading);
            return Vec::new();
        }

        let progress = player.timeline.advance(reading);
        let before = mem::replace(&mut player.at, reading.observed_at);
        let grew = progress.watched > mem::replace(&mut player.so_far, progress.watched);
        if let Known::Value(_) = progress.duration {
            player.length = progress.duration;
        }
        // Nothing was played, so nothing is counted and the instant counted
        // up to stays where it is: moved by a paused player, it would take
        // from every interval of one that plays beside it.
        if !grew {
            return Vec::new();
        }

        let added = reading.observed_at.since(before.max(*until));
        *until = (*until).max(reading.observed_at);
        if added.is_zero() {
            return Vec::new();
        }
        *watched += added;

        let mut effects = vec![Effect::Keep {
            title: self.title.clone(),
            episode: self.episode,
            media: player.media.clone(),
            watched: *watched,
        }];
        let reached = Progress {
            watched: *watched,
            duration: player.length,
            ..progress
        };
        if !*registered && policy.counts_as_watched(&reached) {
            *registered = true;
            effects.push(Effect::Record {
                title: self.title.clone(),
                episode: self.episode,
                media: player.media.clone(),
            });
        }

        effects
    }

    /// What the sitting ends with.
    fn close(self) -> Effect {
        Effect::Close {
            title: self.title,
            episode: self.episode,
        }
    }
}

/// The sittings that are open, one for each episode a player has open.
#[derive(Debug)]
pub struct Session {
    /// The episodes that are open. A player is in one of them at most.
    sittings: Vec<Sitting>,
    /// How many questions have been asked.
    asked: u64,
    /// When an episode counts as watched.
    policy: WatchedPolicy,
}

impl Session {
    /// A session with nothing open, which decides by this policy.
    #[must_use]
    pub const fn new(policy: WatchedPolicy) -> Self {
        Self {
            sittings: Vec::new(),
            asked: 0,
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
    ///
    /// Every reading is stamped from the clock every other reading of this
    /// session is stamped from.
    #[must_use = "an effect that is dropped is one nobody carries out"]
    pub fn advance(
        &mut self,
        reading: &PlayerSnapshot,
        decision: Option<&Recognition>,
    ) -> Vec<Effect> {
        let named = Named::by(reading, decision);

        if let Some(named) = named
            && let Some(sitting) = self
                .sittings
                .iter_mut()
                .find(|sitting| sitting.is_of(named) && sitting.has(&reading.player))
        {
            return sitting.read(named.media, reading, &self.policy);
        }

        let mut effects: Vec<Effect> = self.leave(&reading.player).into_iter().collect();
        if let Some(named) = named {
            effects.extend(self.open(named, reading));
        }

        effects
    }

    /// Take the answer to an [`Effect::Resume`].
    ///
    /// `question` is the one the effect carried, and `kept` is what the
    /// store had of the episode, with nothing where it had none. Every
    /// player that has the episode open counts on from the total in the
    /// answer and from its own last reading before it. What a player played
    /// before that reading is not counted. The interval from it is counted
    /// as [`Timeline`] counts any interval: as the state that reading
    /// reported, and not at all where it is longer than
    /// [`OBSERVED_LIMIT`](crate::timeline::OBSERVED_LIMIT).
    ///
    /// An answer is dropped where the sitting that asked is over, and where
    /// it has its answer already. A sitting of the same episode that began
    /// since has a question of its own and does not take it.
    pub fn resumed(&mut self, question: Question, kept: Option<Resumed>) {
        for sitting in &mut self.sittings {
            sitting.answered(question, kept);
        }
    }

    /// Let these players leave what they had open, and close what the last
    /// of them leaves.
    ///
    /// For a player that left and for one that has nothing open any more,
    /// neither of which sends a reading to say so. A player with nothing
    /// open is passed over.
    ///
    /// Called after the readings of its round have been taken. An episode
    /// that one player closes and another opens in one round then stays
    /// open, where called before them it would be closed and asked about
    /// again.
    #[must_use = "an effect that is dropped is one nobody carries out"]
    pub fn gone(&mut self, players: &[PlayerId]) -> Vec<Effect> {
        players
            .iter()
            .filter_map(|player| self.leave(player))
            .collect()
    }

    /// Take a player out of the sitting it is in, and close the sitting
    /// where it was the last.
    fn leave(&mut self, player: &PlayerId) -> Option<Effect> {
        let at = self
            .sittings
            .iter()
            .position(|sitting| sitting.has(player))?;
        let last = self.sittings[at].leave(player);

        last.then(|| self.sittings.remove(at).close())
    }

    /// Put a player into the sitting of what it opened, and begin one with
    /// a question where there is none.
    fn open(&mut self, named: Named<'_>, reading: &PlayerSnapshot) -> Option<Effect> {
        if let Some(sitting) = self
            .sittings
            .iter_mut()
            .find(|sitting| sitting.is_of(named))
        {
            sitting.join(named.media, reading);
            return None;
        }

        self.asked += 1;
        let question = Question(self.asked);
        self.sittings
            .push(Sitting::asking(question, named, reading));

        Some(Effect::Resume {
            question,
            title: named.title.to_owned(),
            episode: named.episode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Effect, Question, Resumed, Session};
    use crate::clock::{Clock, TestClock, Timestamp};
    use crate::path::RawPath;
    use crate::recognise::altname::Altnames;
    use crate::recognise::corpus::Corpus;
    use crate::recognise::index::Index;
    use crate::recognise::parse::{Episode, parse};
    use crate::recognise::{Ambiguity, Match, Recognition, Refusal, Stage, decide};
    use crate::timeline::{WATCHED_FALLBACK, WatchedPolicy};
    use crate::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::cell::Cell;
    use std::time::Duration;

    /// The interval between two readings of one player.
    const SECOND: Duration = Duration::from_secs(1);

    /// The length a reading here reports unless its test gives it another.
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

    fn a_player(name: &str) -> PlayerId {
        PlayerId(name.to_owned())
    }

    /// One player, read a second apart on a clock of its own.
    ///
    /// For a test of one player, or of players with an episode each. Where
    /// two have one episode open the session compares their instants, and
    /// the readings come from [`Stamps`].
    struct Player {
        id: PlayerId,
        clock: TestClock,
    }

    impl Player {
        fn named(id: &str) -> Self {
            Self {
                id: a_player(id),
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

    /// One clock for every player of a test, moved to the instants the test
    /// names.
    ///
    /// An adapter stamps each reading of a round when its own source
    /// answered, so two players of one round are stamped apart, and the one
    /// handed over first may carry the later instant.
    struct Stamps {
        clock: TestClock,
        reached: Cell<Duration>,
    }

    impl Stamps {
        fn new() -> Self {
            Self {
                clock: TestClock::new(),
                reached: Cell::new(Duration::ZERO),
            }
        }

        /// The instant this many milliseconds from the epoch. A clock goes
        /// one way, so instants are asked for in the order they fall in,
        /// whatever order their readings are handed over in.
        fn at(&self, millis: u64) -> Timestamp {
            let instant = Duration::from_millis(millis);
            let ahead = instant
                .checked_sub(self.reached.get())
                .expect("the instants of a test are asked for in order");
            self.clock.advance(ahead);
            self.reached.set(instant);
            self.clock.now()
        }

        fn reading(
            &self,
            player: &str,
            name: &str,
            state: PlayState,
            millis: u64,
        ) -> PlayerSnapshot {
            PlayerSnapshot {
                player: a_player(player),
                media: MediaRef::LocalFile(a_path(name)),
                state,
                position: Known::NotReported,
                duration: Known::Value(LENGTH),
                observed_at: self.at(millis),
            }
        }

        fn playing(&self, player: &str, name: &str, millis: u64) -> PlayerSnapshot {
            self.reading(player, name, PlayState::Playing, millis)
        }

        fn paused(&self, player: &str, name: &str, millis: u64) -> PlayerSnapshot {
            self.reading(player, name, PlayState::Paused, millis)
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
            if let Effect::Resume { question, .. } = effect {
                session.resumed(*question, kept);
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

    /// The totals a run kept, in the order it kept them.
    fn kept_in(answers: &[Vec<Effect>]) -> Vec<Duration> {
        answers
            .iter()
            .flatten()
            .filter_map(|effect| match effect {
                Effect::Keep { watched, .. } => Some(*watched),
                Effect::Resume { .. } | Effect::Record { .. } | Effect::Close { .. } => None,
            })
            .collect()
    }

    /// What the store had, unregistered.
    fn unregistered(seconds: u64) -> Resumed {
        Resumed {
            watched: Duration::from_secs(seconds),
            registered: false,
        }
    }

    /// The question of this number, a session's first being the first.
    fn resume(number: u64, title: &str, episode: Option<u32>) -> Effect {
        Effect::Resume {
            question: Question(number),
            title: title.to_owned(),
            episode,
        }
    }

    fn keep(title: &str, episode: Option<u32>, name: &str, seconds: u64) -> Effect {
        keep_ms(title, episode, name, seconds * 1000)
    }

    fn keep_ms(title: &str, episode: Option<u32>, name: &str, millis: u64) -> Effect {
        Effect::Keep {
            title: title.to_owned(),
            episode,
            media: a_path(name),
            watched: Duration::from_millis(millis),
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

        assert_eq!(effects, [resume(1, SERIES, Some(3))]);
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

            assert_eq!(effects, [resume(1, SERIES, Some(3))], "{state:?}");
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

        assert_eq!(first, [resume(1, SERIES, Some(3))]);
        assert_eq!(after, NOTHING);
    }

    #[test]
    fn the_count_goes_on_from_the_answer_and_from_the_reading_that_asked() {
        // The second between the reading that asked and the one after it was
        // played, so it is counted on top of what the store had.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        answering(&mut session, &player.playing(THIRD), Some(unregistered(61)));
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
        session.resumed(Question(1), None);
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 1)]);
    }

    #[test]
    fn an_answer_given_twice_is_taken_once() {
        // The second answer would start the count again from what it says.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        watch(&mut session, &player, THIRD, 4, None);
        session.resumed(Question(1), Some(unregistered(50)));
        let effects = advance(&mut session, &player.playing(THIRD));

        assert_eq!(effects, [keep(SERIES, Some(3), THIRD, 4)]);
    }

    #[test]
    fn an_answer_about_an_episode_no_longer_open_is_dropped() {
        // The player moved on to the fourth episode before the answer about
        // the third arrived. Taken, it would give the fourth what was watched
        // of the third, and register it.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        advance(&mut session, &player.playing(THIRD));
        advance(&mut session, &player.playing(FOURTH));
        session.resumed(Question(1), Some(unregistered(99)));
        let still_asking = advance(&mut session, &player.playing(FOURTH));
        session.resumed(Question(2), None);
        let counted = advance(&mut session, &player.playing(FOURTH));

        assert_eq!(still_asking, NOTHING);
        assert_eq!(counted, [keep(SERIES, Some(4), FOURTH, 1)]);
    }

    #[test]
    fn an_answer_to_a_question_asked_before_a_close_is_dropped() {
        // The same episode, closed and opened again, so that title and
        // episode tell the two questions apart no longer. A store drops a
        // total at a close where the episode registered, so the answer to
        // the first question says what the store no longer holds: taken, it
        // would mark this sitting registered, and its viewing would not be
        // recorded.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let before_the_close = Some(Resumed {
            watched: Duration::from_secs(120),
            registered: true,
        });

        let first = advance(&mut session, &player.playing(THIRD));
        let closed = session.gone(std::slice::from_ref(&player.id));
        let second = advance(&mut session, &player.playing(THIRD));
        session.resumed(Question(1), before_the_close);
        let still_asking = advance(&mut session, &player.playing(THIRD));
        session.resumed(Question(2), None);
        let counted = advance(&mut session, &player.playing(THIRD));

        assert_eq!(first, [resume(1, SERIES, Some(3))]);
        assert_eq!(closed, [close(SERIES, Some(3))]);
        assert_eq!(second, [resume(2, SERIES, Some(3))]);
        assert_eq!(still_asking, NOTHING);
        assert_eq!(counted, [keep(SERIES, Some(3), THIRD, 1)]);
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

        answering(&mut session, &player.paused(THIRD), Some(unregistered(61)));
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
        // policy, and the second is given what the first kept. What is kept
        // and what the policy is asked about are both the episode's total:
        // the second sitting's own timeline has counted forty seconds at the
        // reading that records.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let first = watch(&mut session, &player, THIRD, 61, None);
        let closed = session.gone(std::slice::from_ref(&player.id));
        let second = watch(&mut session, &player, THIRD, 61, Some(unregistered(60)));

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
    fn a_total_past_the_policy_at_the_answer_is_recorded_by_the_first_reading_that_adds_to_it() {
        // The total the store answers with can be past the policy with the
        // episode not registered, where the policy in force when it was kept
        // asked for more. A reading that adds nothing records nothing, and
        // the interval out of the pause began paused.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        answering(&mut session, &player.paused(THIRD), Some(unregistered(150)));
        let paused = advance(&mut session, &player.paused(THIRD));
        let out_of_the_pause = advance(&mut session, &player.playing(THIRD));
        let playing = advance(&mut session, &player.playing(THIRD));
        let after = advance(&mut session, &player.playing(THIRD));

        assert_eq!(paused, NOTHING);
        assert_eq!(out_of_the_pause, NOTHING);
        assert_eq!(
            playing,
            [
                keep(SERIES, Some(3), THIRD, 151),
                record(SERIES, Some(3), THIRD)
            ]
        );
        assert_eq!(after, [keep(SERIES, Some(3), THIRD, 152)]);
    }

    #[test]
    fn an_episode_that_registered_before_is_not_recorded_again() {
        // What a process that was killed leaves: the episode registered and
        // the sitting never closed. The sitting that begins after it goes on
        // from that total, which the second assertion shows, and has nothing
        // to record.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let kept = Some(Resumed {
            watched: Duration::from_secs(120),
            registered: true,
        });

        let answers = watch(&mut session, &player, THIRD, 30, kept);

        assert_eq!(recorded_at(&answers), AT_NO_READING);
        assert_eq!(answers[1], [keep(SERIES, Some(3), THIRD, 121)]);
    }

    #[test]
    fn a_reading_without_a_length_is_decided_by_the_length_its_file_reported() {
        // Two thousand seconds ask a thousand, and four hundred are watched.
        // The fallback asks three hundred, so a reading decided by it would
        // record the episode at a fifth of its length. One reading reports
        // no length, and one reports a length its own position contradicts,
        // which a timeline answers as none.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let long = Known::Value(Duration::from_secs(2000));
        let of_this_length = |length, position| PlayerSnapshot {
            duration: length,
            position,
            ..player.playing(THIRD)
        };

        let answers = [
            answering(
                &mut session,
                &of_this_length(long, Known::NotReported),
                Some(unregistered(400)),
            ),
            advance(&mut session, &of_this_length(long, Known::NotReported)),
            advance(
                &mut session,
                &of_this_length(Known::NotReported, Known::NotReported),
            ),
            advance(
                &mut session,
                &of_this_length(
                    Known::Value(Duration::from_secs(5)),
                    Known::Value(Duration::from_secs(600)),
                ),
            ),
        ];

        assert_eq!(answers[1], [keep(SERIES, Some(3), THIRD, 401)]);
        assert_eq!(answers[2], [keep(SERIES, Some(3), THIRD, 402)]);
        assert_eq!(answers[3], [keep(SERIES, Some(3), THIRD, 403)]);
    }

    #[test]
    fn a_length_reported_after_the_first_reading_is_the_one_in_force() {
        // A player is free to report the length of a file a moment after it
        // reports the file. Two hundred seconds ask a hundred, where the
        // fallback, deciding for the reading without a length, asks three
        // hundred.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let opening = PlayerSnapshot {
            duration: Known::NotReported,
            ..player.playing(THIRD)
        };

        answering(&mut session, &opening, Some(unregistered(98)));
        let short = advance(&mut session, &player.playing(THIRD));
        let reached = advance(&mut session, &player.playing(THIRD));

        assert_eq!(short, [keep(SERIES, Some(3), THIRD, 99)]);
        assert_eq!(
            reached,
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn a_length_the_first_reading_alone_reports_is_the_one_in_force() {
        // The reading a player holds while the question is out is a reading
        // of the file like any other, and its length is remembered from it.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let without_a_length = || PlayerSnapshot {
            duration: Known::NotReported,
            ..player.playing(THIRD)
        };

        answering(&mut session, &player.playing(THIRD), Some(unregistered(98)));
        let short = advance(&mut session, &without_a_length());
        let reached = advance(&mut session, &without_a_length());

        assert_eq!(short, [keep(SERIES, Some(3), THIRD, 99)]);
        assert_eq!(
            reached,
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn a_length_is_taken_from_a_reading_that_adds_nothing() {
        // The one reading that reports the length is paused and begins
        // paused, so it adds nothing to the total. It is a reading of the
        // file all the same.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let without_a_length = |reading| PlayerSnapshot {
            duration: Known::NotReported,
            ..reading
        };

        answering(
            &mut session,
            &without_a_length(player.paused(THIRD)),
            Some(unregistered(98)),
        );
        let reported = advance(&mut session, &player.paused(THIRD));
        let out_of_the_pause = advance(&mut session, &without_a_length(player.playing(THIRD)));
        let short = advance(&mut session, &without_a_length(player.playing(THIRD)));
        let reached = advance(&mut session, &without_a_length(player.playing(THIRD)));

        assert_eq!(reported, NOTHING);
        assert_eq!(out_of_the_pause, NOTHING);
        assert_eq!(short, [keep(SERIES, Some(3), THIRD, 99)]);
        assert_eq!(
            reached,
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn a_file_that_reports_no_length_is_decided_by_the_fallback() {
        // The other direction: with no reading of the file reporting a
        // length there is none to remember, and five minutes decide.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let without_a_length = || PlayerSnapshot {
            duration: Known::NotReported,
            ..player.playing(THIRD)
        };

        answering(&mut session, &without_a_length(), Some(unregistered(298)));
        let short = advance(&mut session, &without_a_length());
        let reached = advance(&mut session, &without_a_length());

        assert_eq!(short, [keep(SERIES, Some(3), THIRD, 299)]);
        assert_eq!(
            reached,
            [
                keep(SERIES, Some(3), THIRD, 300),
                record(SERIES, Some(3), THIRD)
            ]
        );
    }

    #[test]
    fn another_file_closes_the_episode_and_asks_about_the_next() {
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        watch(&mut session, &player, THIRD, 3, None);
        let effects = advance(&mut session, &player.playing(FOURTH));

        assert_eq!(
            effects,
            [close(SERIES, Some(3)), resume(2, SERIES, Some(4))]
        );
    }

    #[test]
    fn another_file_of_the_episode_keeps_it_open_with_its_total_and_its_mark() {
        // A better release opened in the middle of an episode is the same
        // viewing. Closed and asked about again, the store would drop the
        // total of an episode that registered, and the rest of the viewing
        // would count towards recording it a second time.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let first = watch(&mut session, &player, THIRD, 102, None);
        let moved = advance(&mut session, &player.playing(THIRD_BY_OTHERS));
        let second = watch(&mut session, &player, THIRD_BY_OTHERS, 150, None);

        assert_eq!(recorded_at(&first), [ASKED_AT]);
        assert_eq!(first[101], [keep(SERIES, Some(3), THIRD, 101)]);
        assert_eq!(moved, NOTHING);
        assert_eq!(second[0], [keep(SERIES, Some(3), THIRD_BY_OTHERS, 102)]);
        assert_eq!(recorded_at(&second), AT_NO_READING);
    }

    #[test]
    fn another_file_of_the_episode_opened_before_the_answer_is_the_one_counted_from() {
        // The reading a player holds is of the file it has open. Held over
        // from the file before, it would start a timeline of that file, and
        // the first reading of this one would start it again with nothing.
        let mut session = Session::new(half());
        let player = Player::named("mpv");

        let asked = advance(&mut session, &player.playing(THIRD));
        let moved = advance(&mut session, &player.playing(THIRD_BY_OTHERS));
        session.resumed(Question(1), Some(unregistered(61)));
        let counted = advance(&mut session, &player.playing(THIRD_BY_OTHERS));

        assert_eq!(asked, [resume(1, SERIES, Some(3))]);
        assert_eq!(moved, NOTHING);
        assert_eq!(counted, [keep(SERIES, Some(3), THIRD_BY_OTHERS, 62)]);
    }

    #[test]
    fn the_length_of_one_file_does_not_decide_for_another() {
        // The first file reports two hundred seconds, which ask a hundred,
        // and the second reports none, so the fallback asks three hundred.
        // With the first file's length remembered across the move, the
        // episode would be recorded at a hundred.
        let mut session = Session::new(half());
        let player = Player::named("mpv");
        let without_a_length = || PlayerSnapshot {
            duration: Known::NotReported,
            ..player.playing(THIRD_BY_OTHERS)
        };

        watch(&mut session, &player, THIRD, 51, None);
        let second: Vec<Vec<Effect>> = (0..60)
            .map(|_| advance(&mut session, &without_a_length()))
            .collect();

        assert_eq!(second[1], [keep(SERIES, Some(3), THIRD_BY_OTHERS, 51)]);
        assert_eq!(second[59], [keep(SERIES, Some(3), THIRD_BY_OTHERS, 109)]);
        assert_eq!(recorded_at(&second), AT_NO_READING);
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
        assert_eq!(opened_again, [resume(2, SERIES, Some(3))]);
    }

    #[test]
    fn two_players_with_an_episode_each_are_two_sittings() {
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
    fn two_players_with_one_episode_open_ask_once_and_both_start_at_the_answer() {
        // Each holds its own last reading while the question is out, and
        // counts from it. The two seconds each played before the answer are
        // not counted. The second player adds the two tenths of its second
        // that lie past what the first has counted.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let before: Vec<Vec<Effect>> = [1000, 2000, 3000]
            .into_iter()
            .flat_map(|round| {
                [
                    stamps.playing("mpv", THIRD, round),
                    stamps.playing("vlc", THIRD_BY_OTHERS, round + 200),
                ]
            })
            .map(|reading| advance(&mut session, &reading))
            .collect();
        session.resumed(Question(1), Some(unregistered(61)));
        let mpv = advance(&mut session, &stamps.playing("mpv", THIRD, 4000));
        let vlc = advance(&mut session, &stamps.playing("vlc", THIRD_BY_OTHERS, 4200));

        assert_eq!(before[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(before[1..].concat(), NOTHING);
        assert_eq!(mpv, [keep(SERIES, Some(3), THIRD, 62)]);
        assert_eq!(vlc, [keep_ms(SERIES, Some(3), THIRD_BY_OTHERS, 62_200)]);
    }

    #[test]
    fn a_player_that_joins_an_episode_asks_nothing_and_no_total_goes_down() {
        // One player has played forty seconds when a second opens the
        // episode. The second asks nothing, counts from its first reading,
        // and what either keeps is more than what was kept before it.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let mut answers: Vec<Vec<Effect>> = (1..=41)
            .map(|second| {
                answering(
                    &mut session,
                    &stamps.playing("mpv", THIRD, second * 1000),
                    None,
                )
            })
            .collect();
        for round in [42_000, 43_000] {
            let joined = stamps.playing("vlc", THIRD_BY_OTHERS, round - 800);
            let playing = stamps.playing("mpv", THIRD, round);
            answers.push(advance(&mut session, &joined));
            answers.push(advance(&mut session, &playing));
        }

        let asked = answers
            .concat()
            .iter()
            .filter(|effect| matches!(effect, Effect::Resume { .. }))
            .count();
        let kept = kept_in(&answers);
        assert_eq!(asked, 1);
        assert_eq!(answers[41], NOTHING);
        assert_eq!(answers[42], [keep(SERIES, Some(3), THIRD, 41)]);
        assert_eq!(
            answers[43],
            [keep_ms(SERIES, Some(3), THIRD_BY_OTHERS, 41_200)]
        );
        assert_eq!(answers[44], [keep(SERIES, Some(3), THIRD, 42)]);
        assert!(
            kept.windows(2).all(|pair| pair[0] < pair[1]),
            "a total was kept that is no more than the one before it: {kept:?}"
        );
    }

    #[test]
    fn two_players_playing_at_once_add_a_second_a_second_and_record_once() {
        // Ninety-eight seconds kept, and a hundred asked. From the first
        // reading to the last, 2.2 s pass and the total grows by 2.2 s. The
        // reading that reaches a hundred records the episode, and the
        // reading of the other player after it, past the policy as well,
        // records nothing.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let answers: Vec<Vec<Effect>> = [1000, 2000, 3000]
            .into_iter()
            .flat_map(|round| {
                [
                    stamps.playing("mpv", THIRD, round),
                    stamps.playing("vlc", THIRD_BY_OTHERS, round + 200),
                ]
            })
            .map(|reading| answering(&mut session, &reading, Some(unregistered(98))))
            .collect();

        assert_eq!(answers[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(answers[1], NOTHING);
        assert_eq!(answers[2], [keep(SERIES, Some(3), THIRD, 99)]);
        assert_eq!(
            answers[3],
            [keep_ms(SERIES, Some(3), THIRD_BY_OTHERS, 99_200)]
        );
        assert_eq!(
            answers[4],
            [
                keep(SERIES, Some(3), THIRD, 100),
                record(SERIES, Some(3), THIRD)
            ]
        );
        assert_eq!(
            answers[5],
            [keep_ms(SERIES, Some(3), THIRD_BY_OTHERS, 100_200)]
        );
    }

    #[test]
    fn a_paused_player_takes_nothing_from_the_one_that_plays() {
        // The paused one is stamped three tenths after the playing one in
        // every round. Its readings add nothing and move nothing, so the
        // playing one adds its whole second. With both stamped at one
        // instant this could not fail.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let answers: Vec<Vec<Effect>> = [0, 1000, 2000, 3000, 4000]
            .into_iter()
            .flat_map(|round| {
                [
                    stamps.playing("mpv", THIRD, round + 2),
                    stamps.paused("vlc", THIRD, round + 300),
                ]
            })
            .map(|reading| answering(&mut session, &reading, None))
            .collect();

        assert_eq!(answers[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(answers[1], NOTHING);
        assert_eq!(
            kept_in(&answers),
            [1, 2, 3, 4].map(Duration::from_secs),
            "{answers:?}"
        );
        assert_eq!(answers[3], NOTHING);
        assert_eq!(answers[9], NOTHING);
    }

    #[test]
    fn players_stamped_earlier_and_handed_over_later_add_nothing_twice() {
        // Three play. The one handed over first carries the latest instant
        // of every round, as it does where its source is the slowest to
        // answer. What the other two played lies before what the first has
        // counted, so three rounds add three seconds. The third is what
        // shows the instant counted up to never goes back: moved back to
        // the second's, it would let the third add the tenth between them.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let answers: Vec<Vec<Effect>> = [0, 1000, 2000, 3000]
            .into_iter()
            .flat_map(|round| {
                let earliest = stamps.playing("vlc", THIRD, round + 2);
                let earlier = stamps.playing("celluloid", THIRD, round + 100);
                let latest = stamps.playing("mpv", THIRD, round + 300);
                [latest, earliest, earlier]
            })
            .map(|reading| answering(&mut session, &reading, None))
            .collect();

        assert_eq!(answers[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(
            kept_in(&answers),
            [1, 2, 3].map(Duration::from_secs),
            "{answers:?}"
        );
        assert_eq!(answers[4..6].concat(), NOTHING);
        assert_eq!(answers[10..12].concat(), NOTHING);
    }

    #[test]
    fn a_player_that_opens_another_file_takes_nothing_from_the_one_that_plays() {
        // The paused player is stamped three tenths after the playing one
        // and moves to another file of the episode in the third round. Its
        // reading of the new file adds nothing and moves nothing, so the
        // playing one adds its whole second in the round after.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let answers: Vec<Vec<Effect>> = [
            (1000, THIRD),
            (2000, THIRD),
            (3000, THIRD_BY_OTHERS),
            (4000, THIRD_BY_OTHERS),
        ]
        .into_iter()
        .flat_map(|(round, beside)| {
            [
                stamps.playing("mpv", THIRD, round),
                stamps.paused("vlc", beside, round + 300),
            ]
        })
        .map(|reading| answering(&mut session, &reading, None))
        .collect();

        assert_eq!(
            kept_in(&answers),
            [1, 2, 3].map(Duration::from_secs),
            "{answers:?}"
        );
        assert_eq!(answers[5], NOTHING);
    }

    #[test]
    fn a_player_that_leaves_takes_nothing_back_from_what_was_counted() {
        // Two play and a third sits paused. The one stamped later has
        // counted up to 2.3 s when the paused one leaves. The one stamped
        // earlier is handed over first in the round after, and adds what
        // lies past 2.3 s of its second, seven tenths, and the other adds
        // the three tenths that are left.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        for round in [1000, 2000] {
            let earliest = stamps.playing("vlc", THIRD, round + 2);
            let paused = stamps.paused("celluloid", THIRD, round + 100);
            let latest = stamps.playing("mpv", THIRD, round + 300);
            for reading in [latest, earliest, paused] {
                answering(&mut session, &reading, None);
            }
        }
        let left = session.gone(&[a_player("celluloid")]);
        let earlier = advance(&mut session, &stamps.playing("vlc", THIRD, 3002));
        let later = advance(&mut session, &stamps.playing("mpv", THIRD, 3300));

        assert_eq!(left, NOTHING);
        assert_eq!(earlier, [keep_ms(SERIES, Some(3), THIRD, 1_702)]);
        assert_eq!(later, [keep(SERIES, Some(3), THIRD, 2)]);
    }

    #[test]
    fn a_long_interval_handed_over_late_adds_what_lies_past_what_was_counted() {
        // What counting time once costs. One player is silent for four
        // rounds and its next reading ends an interval of five seconds,
        // which a timeline counts. The other began to play inside those
        // five seconds and has counted up to 104.2 s. Of the five seconds,
        // the eight tenths after that instant are added, and the 3.2 s
        // before the other began to play are lost.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let readings = [
            stamps.playing("mpv", THIRD, 99_000),
            stamps.paused("vlc", THIRD_BY_OTHERS, 99_200),
            stamps.playing("mpv", THIRD, 100_000),
            stamps.paused("vlc", THIRD_BY_OTHERS, 100_200),
            stamps.paused("vlc", THIRD_BY_OTHERS, 101_200),
            stamps.paused("vlc", THIRD_BY_OTHERS, 102_200),
            stamps.playing("vlc", THIRD_BY_OTHERS, 103_200),
            stamps.playing("vlc", THIRD_BY_OTHERS, 104_200),
            stamps.playing("mpv", THIRD, 105_000),
            stamps.playing("vlc", THIRD_BY_OTHERS, 105_200),
        ];
        let answers: Vec<Vec<Effect>> = readings
            .iter()
            .map(|reading| answering(&mut session, reading, None))
            .collect();

        assert_eq!(answers[2], [keep(SERIES, Some(3), THIRD, 1)]);
        assert_eq!(answers[3..7].concat(), NOTHING);
        assert_eq!(answers[7], [keep(SERIES, Some(3), THIRD_BY_OTHERS, 2)]);
        assert_eq!(answers[8], [keep_ms(SERIES, Some(3), THIRD, 2_800)]);
        assert_eq!(answers[9], [keep(SERIES, Some(3), THIRD_BY_OTHERS, 3)]);
    }

    #[test]
    fn the_first_player_to_leave_an_episode_answers_nothing_and_the_last_closes_it() {
        // Closed by the first to leave, the store would drop the total of
        // an episode that registered under the player still counting.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        for round in [1000, 2000, 3000] {
            for reading in [
                stamps.playing("mpv", THIRD, round),
                stamps.paused("vlc", THIRD, round + 200),
            ] {
                answering(&mut session, &reading, None);
            }
        }
        let first = session.gone(&[a_player("mpv")]);
        let into_playing = advance(&mut session, &stamps.playing("vlc", THIRD, 4200));
        let playing = advance(&mut session, &stamps.playing("vlc", THIRD, 5200));
        let last = session.gone(&[a_player("vlc")]);
        let opened_again = advance(&mut session, &stamps.playing("mpv", THIRD, 6000));

        assert_eq!(first, NOTHING);
        assert_eq!(into_playing, NOTHING);
        assert_eq!(playing, [keep(SERIES, Some(3), THIRD, 3)]);
        assert_eq!(last, [close(SERIES, Some(3))]);
        assert_eq!(opened_again, [resume(2, SERIES, Some(3))]);
    }

    #[test]
    fn two_players_of_one_episode_that_leave_together_close_it_once() {
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        answering(&mut session, &stamps.playing("mpv", THIRD, 1000), None);
        answering(&mut session, &stamps.playing("vlc", THIRD, 1200), None);
        let closed = session.gone(&[a_player("vlc"), a_player("mpv")]);

        assert_eq!(closed, [close(SERIES, Some(3))]);
    }

    #[test]
    fn a_player_that_moves_on_leaves_the_episode_open_for_the_other() {
        // It asks about the episode it moves to and closes nothing. Coming
        // back, it closes the one it left and asks nothing, the episode
        // having been open all along.
        let mut session = Session::new(half());
        let stamps = Stamps::new();
        let mut round = |mpv: &str, at: u64| {
            let first = answering(&mut session, &stamps.playing("mpv", mpv, at), None);
            let second = answering(&mut session, &stamps.paused("vlc", THIRD, at + 200), None);
            [first, second]
        };

        let opened = round(THIRD, 1000);
        let played = round(THIRD, 2000);
        let moved = round(FOURTH, 3000);
        let elsewhere = round(FOURTH, 4000);
        let back = round(THIRD, 5000);
        let again = round(THIRD, 6000);

        assert_eq!(opened, [vec![resume(1, SERIES, Some(3))], vec![]]);
        assert_eq!(played, [vec![keep(SERIES, Some(3), THIRD, 1)], vec![]]);
        assert_eq!(moved, [vec![resume(2, SERIES, Some(4))], vec![]]);
        assert_eq!(elsewhere, [vec![keep(SERIES, Some(4), FOURTH, 1)], vec![]]);
        assert_eq!(back, [vec![close(SERIES, Some(4))], vec![]]);
        assert_eq!(again, [vec![keep(SERIES, Some(3), THIRD, 2)], vec![]]);
    }

    #[test]
    fn an_answer_is_taken_by_the_player_that_still_has_the_episode_open() {
        // The player that asked moved on before the answer. The question
        // was about the episode, and the player that joined while it was
        // out counts from the last reading it holds.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let asked = advance(&mut session, &stamps.playing("mpv", THIRD, 1000));
        let joined = advance(&mut session, &stamps.playing("vlc", THIRD, 1200));
        let moved = advance(&mut session, &stamps.playing("mpv", FOURTH, 2000));
        let waiting = advance(&mut session, &stamps.playing("vlc", THIRD, 2200));
        session.resumed(Question(1), Some(unregistered(61)));
        let counted = advance(&mut session, &stamps.playing("vlc", THIRD, 3200));

        assert_eq!(asked, [resume(1, SERIES, Some(3))]);
        assert_eq!(joined, NOTHING);
        assert_eq!(moved, [resume(2, SERIES, Some(4))]);
        assert_eq!(waiting, NOTHING);
        assert_eq!(counted, [keep(SERIES, Some(3), THIRD, 62)]);
    }

    #[test]
    fn a_player_that_played_nothing_does_not_register_the_episode() {
        // Two releases of one episode. The one that plays reports four
        // hundred seconds, which ask two hundred, and the one that sits
        // paused reports two hundred, which ask a hundred. A hundred and
        // fifty were kept. Asked at the paused player's readings, the
        // policy would count the total as watched by the length of a file
        // nobody is playing.
        let mut session = Session::new(half());
        let stamps = Stamps::new();
        let longer = |at| PlayerSnapshot {
            duration: Known::Value(Duration::from_secs(400)),
            ..stamps.playing("mpv", THIRD, at)
        };

        let answers: Vec<Vec<Effect>> = [1000, 2000, 3000, 4000]
            .into_iter()
            .flat_map(|round| {
                [
                    longer(round),
                    stamps.paused("vlc", THIRD_BY_OTHERS, round + 200),
                ]
            })
            .map(|reading| answering(&mut session, &reading, Some(unregistered(150))))
            .collect();

        assert_eq!(
            kept_in(&answers),
            [151, 152, 153].map(Duration::from_secs),
            "{answers:?}"
        );
        assert_eq!(recorded_at(&answers), AT_NO_READING);
    }

    #[test]
    fn an_episode_handed_from_one_player_to_another_in_one_round_stays_open() {
        // The readings of a round are taken before the players that are
        // gone are named. The episode registered in the first player, the
        // second opens it in the round the first leaves in, and it is one
        // sitting: nothing is closed, the total goes on, and nothing is
        // recorded again.
        let mut session = Session::new(half());
        let stamps = Stamps::new();

        let first: Vec<Vec<Effect>> = (1..=102)
            .map(|second| {
                answering(
                    &mut session,
                    &stamps.playing("mpv", THIRD, second * 1000),
                    None,
                )
            })
            .collect();
        let opened = advance(&mut session, &stamps.playing("vlc", THIRD, 103_000));
        let left = session.gone(&[a_player("mpv")]);
        let second = advance(&mut session, &stamps.playing("vlc", THIRD, 104_000));
        let closed = session.gone(&[a_player("vlc")]);

        assert_eq!(recorded_at(&first), [ASKED_AT]);
        assert_eq!(opened, NOTHING);
        assert_eq!(left, NOTHING);
        assert_eq!(second, [keep(SERIES, Some(3), THIRD, 102)]);
        assert_eq!(closed, [close(SERIES, Some(3))]);
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

        assert_eq!(answers[0], [resume(1, FILM, None)]);
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
        // The file stays open and what it is changes under it, a decision
        // being given with every reading. The episode alone changes first,
        // then the title alone, then the file is nothing the list holds,
        // and then it is what it was at the start.
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

        assert_eq!(answers[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(
            answers[1],
            [close(SERIES, Some(3)), resume(2, SERIES, Some(4))]
        );
        assert_eq!(
            answers[2],
            [close(SERIES, Some(4)), resume(3, SECOND_SERIES, Some(4))]
        );
        assert_eq!(answers[3], [close(SECOND_SERIES, Some(4))]);
        assert_eq!(answers[4], [resume(4, SERIES, Some(3))]);
    }

    #[test]
    fn a_file_two_players_have_open_decided_otherwise_moves_them_one_by_one() {
        // The first to be read leaves the episode to the second and asks
        // about the new one. The second, the last to leave, closes the old
        // one and joins the new one, which has its question out already.
        let mut session = Session::new(half());
        let stamps = Stamps::new();
        let the_third = recognised(SERIES, Episode::Only(3));
        let the_fourth = recognised(SERIES, Episode::Only(4));

        let answers: Vec<Vec<Effect>> = [(1000, &the_third), (2000, &the_fourth)]
            .into_iter()
            .flat_map(|(round, decision)| {
                [
                    (stamps.playing("mpv", THIRD, round), decision),
                    (stamps.playing("vlc", THIRD, round + 200), decision),
                ]
            })
            .map(|(reading, decision)| session.advance(&reading, Some(decision)))
            .collect();

        assert_eq!(answers[0], [resume(1, SERIES, Some(3))]);
        assert_eq!(answers[1], NOTHING);
        assert_eq!(answers[2], [resume(2, SERIES, Some(4))]);
        assert_eq!(answers[3], [close(SERIES, Some(3))]);
    }
}
