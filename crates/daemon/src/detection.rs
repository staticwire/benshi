//! The detection task: what the platform reports, filtered by policy, on the
//! bus.
//!
//! This is where the adapter, the policy table and the event bus meet, and it
//! is the only place any of them do. The adapter decides nothing and does not
//! know policy exists; the policy table is pure logic in `benshi-core` and
//! knows nothing about a bus; the filter between them is here.
//!
//! Policy is applied to **readings** and never to discovery. A denied source is
//! published in the membership exactly as an admitted one is, because a source
//! nobody can see is a source nobody can diagnose, and "why is my player being
//! ignored" has to have somewhere to look.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use benshi_core::PlayerId;
use benshi_core::policy::PolicyTable;
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use tokio::time::MissedTickBehavior;

use crate::bus::{BusEvent, EventBus};
use crate::supervisor::TaskError;

/// What is said about a reading whose source the same round did not describe.
///
/// The adapter contract forbids this, and the daemon does not rely on that. A
/// reading it cannot attribute to an application is a reading it cannot apply
/// policy to, and publishing it would walk past a deny the user set.
const UNDESCRIBED: &str = "read but not described in the same round, so no policy applies to it";

/// What is said when the policy table cannot be trusted.
const POISONED: &str = "the policy table was poisoned by a panic in another task";

/// Which class a failed round belongs to.
///
/// Every failure is transient, and the match is exhaustive so that a fourth
/// variant has to state its own answer here instead of inheriting one. Nothing
/// is permanent because no platform failure is improved by stopping: a daemon
/// that gave up watching looks exactly like one where nothing is playing, and
/// the user finds out hours later that nothing was recorded.
///
/// This is the class of a whole round, which is not the class of one source. A
/// source that has gone is permanent for that source and is reported per
/// source; the same fact reaching this function says the platform could not be
/// read at all, and that is worth another attempt.
fn classify(error: WatchError) -> TaskError {
    match error {
        WatchError::Timeout { .. } | WatchError::Transport(_) | WatchError::Unavailable(_) => {
            TaskError::Transient(error.into())
        }
    }
}

/// Readings from a platform, filtered by policy, published to the bus.
#[derive(Debug)]
pub struct Detection<W> {
    watcher: W,
    bus: Arc<EventBus>,
    policy: Arc<RwLock<PolicyTable>>,
    period: Duration,
    listed: BTreeSet<PlayerId>,
}

impl<W: PlayerWatcher> Detection<W> {
    /// Detection over one platform, reading one policy table.
    ///
    /// The table is shared rather than copied, so that setting a policy over
    /// IPC changes what a running loop publishes without restarting it.
    #[must_use]
    pub fn new(
        watcher: W,
        bus: Arc<EventBus>,
        policy: Arc<RwLock<PolicyTable>>,
        period: Duration,
    ) -> Self {
        Self {
            watcher,
            bus,
            policy,
            period,
            listed: BTreeSet::new(),
        }
    }

    /// A round every `period`, for ever.
    ///
    /// The first round happens at once rather than after a wait, so a daemon
    /// that has just started publishes what is playing instead of nothing.
    ///
    /// A round that overruns delays the next one rather than being followed
    /// immediately by another: rounds that ran back to back to catch up would
    /// spend a struggling platform's remaining capacity on the backlog.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError::Transient`] when a round cannot reach the
    /// platform. The supervisor retries with backoff, which for a session bus
    /// that went away is the right answer and the only one.
    pub async fn run(&mut self) -> Result<(), TaskError> {
        let mut ticker = tokio::time::interval(self.period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            self.round().await?;
        }
    }

    /// One round: read the platform, apply policy, publish.
    async fn round(&mut self) -> Result<(), TaskError> {
        let PollOutcome {
            sources,
            snapshots,
            failures,
        } = self.watcher.poll().await.map_err(classify)?;

        let described: BTreeSet<&PlayerId> = sources.iter().map(|source| &source.player).collect();

        // A source that did not answer is still on the platform: it failed to
        // speak, and it did not close. Leaving it out of the membership would
        // report a player that timed out as one that had gone, and report it
        // back the round after.
        let listed: BTreeSet<PlayerId> = described
            .iter()
            .copied()
            .chain(failures.iter().map(|(player, _reason)| player))
            .cloned()
            .collect();
        self.publish_membership(&listed);

        for (player, reason) in failures {
            self.bus.publish(BusEvent::SourceFailed {
                player,
                reason: reason.to_string(),
            });
        }

        // Three answers to a reading: publish it when policy admits its source,
        // drop it without a word when policy denies it, and report it when this
        // round described no source it could have come from.
        let admitted = self.admitted(&sources);

        for snapshot in snapshots {
            if admitted.contains(&snapshot.player) {
                self.bus.publish(BusEvent::Snapshot(snapshot));
            } else if !described.contains(&snapshot.player) {
                self.bus.publish(BusEvent::SourceFailed {
                    player: snapshot.player,
                    reason: UNDESCRIBED.to_owned(),
                });
            }
        }

        Ok(())
    }

    /// Publish this round's membership when it differs from the last round's.
    ///
    /// Only when it differs. The bus is the log a person reads, and a line
    /// every second saying what the last one said is how a log stops being
    /// read.
    fn publish_membership(&mut self, listed: &BTreeSet<PlayerId>) {
        if *listed != self.listed {
            self.listed.clone_from(listed);
            self.bus.publish(BusEvent::SourcesChanged {
                sources: listed.iter().cloned().collect(),
            });
        }
    }

    /// Which of this round's sources policy admits readings from.
    ///
    /// The table is read once for the whole round rather than once per source,
    /// and the guard does not outlive the call, so nothing is published and
    /// nothing is awaited while the table is locked.
    ///
    /// # Panics
    ///
    /// If the table was poisoned by a panic elsewhere. What a poisoned table
    /// says about policy cannot be trusted, and carrying on would risk
    /// publishing a source the user denied. The supervisor records the panic
    /// and restarts the task.
    fn admitted<'round>(&self, sources: &'round [SourceInfo]) -> BTreeSet<&'round PlayerId> {
        let table = self.policy.read().expect(POISONED);

        sources
            .iter()
            .filter(|source| table.policy_for(&source.app).admits())
            .map(|source| &source.player)
            .collect()
    }
}

#[cfg(test)]
// The trait's methods are async, so an implementation cannot drop the keyword
// even when its body has nothing to await. A fake answers from memory, which is
// the whole point of a fake.
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use super::Detection;
    use crate::bus::{BusEvent, EventBus};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::{Policy, PolicyTable};
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
    use std::collections::VecDeque;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;
    use tokio::sync::broadcast::Receiver;

    /// The period a test drives the loop at.
    ///
    /// Every test that lets the loop tick pauses the clock, so this is virtual
    /// time and no test waits for it.
    const INTERVAL: Duration = Duration::from_millis(100);

    fn a_source(identity: &str, app: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn a_reading(source: &SourceInfo) -> PlayerSnapshot {
        PlayerSnapshot {
            player: source.player.clone(),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
            state: source.state,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        }
    }

    /// A round in which every source answered and had something open.
    fn a_round(sources: Vec<SourceInfo>) -> PollOutcome {
        let snapshots = sources.iter().map(a_reading).collect();

        PollOutcome {
            sources,
            snapshots,
            failures: Vec::new(),
        }
    }

    /// A watcher answering from memory: the scripted rounds in order, and then
    /// a round of `then` for ever.
    ///
    /// `PollOutcome` cannot derive `Clone`, because a `WatchError` cannot, so
    /// the repeating answer is rebuilt from its sources rather than copied.
    struct ScriptedWatcher {
        scripted: VecDeque<Result<PollOutcome, WatchError>>,
        then: Vec<SourceInfo>,
    }

    impl ScriptedWatcher {
        /// Answers these rounds in order, then empty rounds for ever.
        fn of(rounds: Vec<Result<PollOutcome, WatchError>>) -> Self {
            Self {
                scripted: rounds.into(),
                then: Vec::new(),
            }
        }

        /// Answers a round describing and reading these sources, every time.
        fn repeating(sources: Vec<SourceInfo>) -> Self {
            Self {
                scripted: VecDeque::new(),
                then: sources,
            }
        }
    }

    impl PlayerWatcher for ScriptedWatcher {
        async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
            Ok(self.then.clone())
        }

        async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
            self.scripted
                .pop_front()
                .unwrap_or_else(|| Ok(a_round(self.then.clone())))
        }
    }

    /// Everything on the bus, drained without waiting.
    ///
    /// `try_recv` rather than `recv`, so a regression that stops an event being
    /// published fails the test in no time instead of hanging the run.
    fn drain(events: &mut Receiver<BusEvent>) -> Vec<BusEvent> {
        let mut seen = Vec::new();

        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }

        seen
    }

    /// The source of every reading among these events, in the order published.
    fn snapshots_in(published: &[BusEvent]) -> Vec<PlayerId> {
        published
            .iter()
            .filter_map(|event| match event {
                BusEvent::Snapshot(snapshot) => Some(snapshot.player.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every membership among these events, in the order published.
    fn listings_in(published: &[BusEvent]) -> Vec<Vec<PlayerId>> {
        published
            .iter()
            .filter_map(|event| match event {
                BusEvent::SourcesChanged { sources } => Some(sources.clone()),
                _ => None,
            })
            .collect()
    }

    fn id(identity: &str) -> PlayerId {
        PlayerId(identity.to_owned())
    }

    fn app(name: &str) -> AppName {
        AppName(name.to_owned())
    }

    #[tokio::test]
    async fn readings_from_admitted_sources_reach_the_bus() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![
            a_source("mpv.instance1701", "mpv"),
            a_source("vlc", "vlc"),
        ]))]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("a round");

        assert_eq!(
            snapshots_in(&drain(&mut events)),
            vec![id("mpv.instance1701"), id("vlc")]
        );
    }

    #[tokio::test]
    async fn a_denied_source_is_silenced_and_still_listed() {
        // Policy suppresses readings and never discovery. A source nobody can
        // see is a source nobody can diagnose, so the denied player stays in
        // the membership the bus publishes.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![
            a_source("firefox.instance30062", "firefox"),
            a_source("mpv", "mpv"),
        ]))]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert_eq!(
            snapshots_in(&published),
            vec![id("mpv")],
            "the denied source produced a reading"
        );
        assert!(
            !published
                .iter()
                .any(|event| matches!(event, BusEvent::SourceFailed { .. })),
            "the denied source was reported as a failure rather than silenced: {published:?}"
        );
        assert_eq!(
            listings_in(&published),
            vec![vec![id("firefox.instance30062"), id("mpv")]],
            "the denied source is missing from the membership"
        );
    }

    #[tokio::test]
    async fn policy_is_keyed_on_the_application_and_not_on_the_identity() {
        // A browser qualifies its bus name with a process id, so the identity
        // is not what the denylist can match. The daemon reads the application
        // the adapter declared beside the reading.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![Ok(a_round(vec![a_source(
            "chromium.instance16481",
            "chromium",
        )]))]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("a round");

        assert!(snapshots_in(&drain(&mut events)).is_empty());
    }

    #[tokio::test]
    async fn a_reading_the_round_did_not_describe_is_reported_and_not_published() {
        // The contract forbids this, and the daemon must not depend on that:
        // a reading it cannot attribute to an application is a reading it
        // cannot apply policy to, and publishing it would walk past a deny.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let stranger = a_source("firefox", "firefox");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: Vec::new(),
            snapshots: vec![a_reading(&stranger)],
            failures: Vec::new(),
        })]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert!(
            snapshots_in(&published).is_empty(),
            "an unattributable reading was published: {published:?}"
        );
        assert!(
            published
                .iter()
                .any(|event| matches!(event, BusEvent::SourceFailed { player, .. } if player == &id("firefox"))),
            "an unattributable reading was dropped without a word: {published:?}"
        );
    }

    #[tokio::test]
    async fn a_failing_source_is_reported_and_the_others_continue() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let working = a_source("mpv", "mpv");
        let watcher = ScriptedWatcher::of(vec![Ok(PollOutcome {
            sources: vec![working.clone()],
            snapshots: vec![a_reading(&working)],
            failures: vec![(
                id("vlc"),
                WatchError::Timeout {
                    player: id("vlc"),
                    deadline: Duration::from_millis(500),
                },
            )],
        })]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("a round");
        let published = drain(&mut events);

        assert_eq!(
            snapshots_in(&published),
            vec![id("mpv")],
            "one source failing blinded the daemon to the rest"
        );
        let reported = published
            .iter()
            .find_map(|event| match event {
                BusEvent::SourceFailed { player, reason } if player == &id("vlc") => Some(reason),
                _ => None,
            })
            .expect("the failure reached the bus");
        assert!(
            reported.contains("500ms"),
            "the reason does not say what happened: {reported:?}"
        );
    }

    #[tokio::test]
    async fn a_source_that_did_not_answer_stays_in_the_membership() {
        // Failing to answer is not closing. A player left out of the membership
        // for one round and put back the next reads as one that closed and
        // reopened, and a player that times out every other round would write
        // that pair of lines for as long as it ran.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let answering = a_source("mpv", "mpv");
        let silent = || {
            (
                id("vlc"),
                WatchError::Timeout {
                    player: id("vlc"),
                    deadline: Duration::from_millis(500),
                },
            )
        };
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![answering.clone(), a_source("vlc", "vlc")])),
            Ok(PollOutcome {
                sources: vec![answering.clone()],
                snapshots: vec![a_reading(&answering)],
                failures: vec![silent()],
            }),
        ]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("the first round");
        detection.round().await.expect("the round it went quiet in");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv"), id("vlc")]],
            "a source that did not answer was reported as gone"
        );
    }

    #[tokio::test]
    async fn a_source_that_stopped_being_listed_leaves_the_membership() {
        // The other half of the same rule. A source that the platform no longer
        // reports at all has closed, and holding it in the membership would
        // mean nothing ever leaves.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![
                a_source("mpv", "mpv"),
                a_source("vlc", "vlc"),
            ])),
            Ok(a_round(vec![a_source("mpv", "mpv")])),
        ]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("the first round");
        detection.round().await.expect("the round after vlc closed");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv"), id("vlc")], vec![id("mpv")]]
        );
    }

    #[tokio::test]
    async fn a_swap_is_visible_because_membership_is_published() {
        // One player closing while another opens leaves the count unchanged,
        // so a count cannot see it. The membership can.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![
            Ok(a_round(vec![a_source("mpv", "mpv")])),
            Ok(a_round(vec![a_source("vlc", "vlc")])),
        ]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("the first round");
        detection.round().await.expect("the second round");

        assert_eq!(
            listings_in(&drain(&mut events)),
            vec![vec![id("mpv")], vec![id("vlc")]]
        );
    }

    #[tokio::test]
    async fn membership_that_did_not_change_is_not_republished() {
        // The bus is the log a person reads. A line every second saying the
        // same thing is how a log stops being read.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        detection.round().await.expect("the first round");
        detection.round().await.expect("the second round");
        detection.round().await.expect("the third round");

        assert_eq!(
            listings_in(&drain(&mut events)).len(),
            1,
            "membership was republished without changing"
        );
    }

    #[tokio::test]
    async fn a_platform_that_cannot_be_reached_is_transient() {
        // There is no platform failure that stopping detection would improve.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::of(vec![Err(WatchError::Transport(Box::new(
            std::io::Error::other("the session bus is gone"),
        )))]);
        let mut detection = Detection::new(watcher, bus, policy, INTERVAL);

        let failure = detection.round().await.expect_err("the round fails");

        assert!(
            matches!(failure, crate::supervisor::TaskError::Transient(_)),
            "a lost bus stopped detection instead of retrying it: {failure:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_deny_takes_effect_within_one_interval_and_without_a_restart() {
        // The policy is read each round rather than once at startup, so a
        // table that changes under a running loop changes what that loop
        // publishes, and the daemon is not restarted to make it take effect.
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let mut detection =
            Detection::new(watcher, Arc::clone(&bus), Arc::clone(&policy), INTERVAL);

        let running = tokio::spawn(async move { detection.run().await });
        tokio::time::sleep(INTERVAL / 2).await;
        assert_eq!(
            snapshots_in(&drain(&mut events)),
            vec![id("mpv")],
            "the loop published nothing before the policy changed"
        );

        policy
            .write()
            .expect("the policy table")
            .set(&app("mpv"), Policy::Deny);
        tokio::time::sleep(INTERVAL).await;

        assert!(
            snapshots_in(&drain(&mut events)).is_empty(),
            "the deny did not take effect within one interval"
        );
        assert!(
            !running.is_finished(),
            "the loop stopped instead of going on"
        );
        running.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_round_happens_once_an_interval() {
        let bus = Arc::new(EventBus::new());
        let mut events = bus.subscribe();
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let mut detection = Detection::new(watcher, Arc::clone(&bus), policy, INTERVAL);

        let running = tokio::spawn(async move { detection.run().await });
        tokio::time::sleep(INTERVAL * 3 + INTERVAL / 2).await;
        running.abort();

        assert_eq!(
            snapshots_in(&drain(&mut events)).len(),
            4,
            "a round runs at the start and once an interval after"
        );
    }

    #[tokio::test]
    async fn a_lock_a_panic_left_poisoned_is_not_read() {
        // A poisoned table means another task panicked holding it, so what it
        // says about policy cannot be trusted. Reading it anyway could publish
        // a source the user denied, which is the one mistake policy exists to
        // prevent.
        let bus = Arc::new(EventBus::new());
        let policy = Arc::new(RwLock::new(PolicyTable::with_default_denylist()));
        let poisoner = Arc::clone(&policy);
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.write().expect("the policy table");
            panic!("a bug in another task");
        })
        .join();
        assert!(panicked.is_err(), "the thread did not panic");

        let watcher = ScriptedWatcher::repeating(vec![a_source("mpv", "mpv")]);
        let mut detection = Detection::new(watcher, bus, policy, INTERVAL);

        let read = tokio::spawn(async move { detection.round().await });

        assert!(
            read.await.expect_err("the round panics").is_panic(),
            "a poisoned policy table was read as though it were trustworthy"
        );
    }

    #[test]
    fn every_watch_error_is_classified() {
        // Written as an exhaustive match in the module, so a fourth variant
        // has to state its own answer rather than inherit one.
        assert!(matches!(
            super::classify(WatchError::Unavailable(id("mpv"))),
            crate::supervisor::TaskError::Transient(_)
        ));
        assert!(matches!(
            super::classify(WatchError::Timeout {
                player: id("mpv"),
                deadline: Duration::from_millis(500),
            }),
            crate::supervisor::TaskError::Transient(_)
        ));
        assert!(matches!(
            super::classify(WatchError::Transport(Box::new(std::io::Error::other(
                "the session bus is gone"
            )))),
            crate::supervisor::TaskError::Transient(_)
        ));
    }
}
