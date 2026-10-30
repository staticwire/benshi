//! Two watchers, read as one.

use futures_util::future::join;

use crate::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

/// Two watchers, read as one: every round reads both together.
///
/// A round carries the sources, readings and failures of both, its sources and
/// readings in the order of their identities. The two must give their sources
/// identities of their own, since a round cannot tell apart two readings of
/// one identity.
///
/// Where either cannot read its platform at all, the round fails, and with it
/// what the other found: [`PlayerWatcher::poll`] has no way to fail half of a
/// round.
pub struct Both<A, B> {
    first: A,
    second: B,
}

impl<A, B> Both<A, B> {
    /// Read `first` and `second` as one.
    #[must_use]
    pub const fn new(first: A, second: B) -> Self {
        Self { first, second }
    }
}

impl<A, B> PlayerWatcher for Both<A, B>
where
    A: PlayerWatcher + Send,
    B: PlayerWatcher + Send,
{
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        let (first, second) = join(self.first.sources(), self.second.sources()).await;
        let mut sources = first?;
        sources.extend(second?);
        sources.sort_by(|left, right| left.player.cmp(&right.player));

        Ok(sources)
    }

    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        let (first, second) = join(self.first.poll(), self.second.poll()).await;
        let (mut round, other) = (first?, second?);
        round.sources.extend(other.sources);
        round.snapshots.extend(other.snapshots);
        round.failures.extend(other.failures);
        round
            .sources
            .sort_by(|left, right| left.player.cmp(&right.player));
        round
            .snapshots
            .sort_by(|left, right| left.player.cmp(&right.player));

        Ok(round)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use benshi_core::clock::Timestamp;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };

    use super::Both;
    use crate::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

    /// A watcher that finds what it is told to, in the time it is told to.
    #[derive(Default)]
    struct Scripted {
        sources: Vec<&'static str>,
        readings: Vec<&'static str>,
        failing: Vec<&'static str>,
        takes: Duration,
        unusable: bool,
    }

    impl Scripted {
        async fn platform(&self) -> Result<(), WatchError> {
            tokio::time::sleep(self.takes).await;
            if self.unusable {
                return Err(WatchError::Transport("the platform went away".into()));
            }
            Ok(())
        }
    }

    impl PlayerWatcher for Scripted {
        async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
            self.platform().await?;

            Ok(self
                .sources
                .iter()
                .map(|identity| a_source(identity))
                .collect())
        }

        async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
            self.platform().await?;

            Ok(PollOutcome {
                sources: self
                    .sources
                    .iter()
                    .map(|identity| a_source(identity))
                    .collect(),
                snapshots: self
                    .readings
                    .iter()
                    .map(|identity| a_reading(identity))
                    .collect(),
                failures: self
                    .failing
                    .iter()
                    .map(|identity| {
                        let player = PlayerId((*identity).to_owned());
                        (player.clone(), WatchError::Unavailable(player))
                    })
                    .collect(),
            })
        }
    }

    fn a_source(identity: &str) -> SourceInfo {
        SourceInfo {
            player: PlayerId(identity.to_owned()),
            app: AppName("mpv".to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn a_reading(identity: &str) -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId(identity.to_owned()),
            media: MediaRef::Title("Episode 3".to_owned()),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(3)),
            duration: Known::Value(Duration::from_mins(24)),
            observed_at: Timestamp::epoch(),
        }
    }

    fn identities<'a>(players: impl Iterator<Item = &'a PlayerId>) -> Vec<&'a str> {
        players.map(|player| player.0.as_str()).collect()
    }

    #[tokio::test]
    async fn a_round_carries_what_both_found_in_the_order_of_identities() {
        let mut both = Both::new(
            Scripted {
                sources: vec!["mpv.instance2", "vlc"],
                readings: vec!["mpv.instance2"],
                failing: vec!["haruna"],
                ..Scripted::default()
            },
            Scripted {
                sources: vec!["127.0.0.1:9090", "mpv.instance1"],
                readings: vec!["127.0.0.1:9090", "mpv.instance1"],
                failing: vec!["smplayer"],
                ..Scripted::default()
            },
        );

        let round = both.poll().await.expect("a round");

        assert_eq!(
            identities(round.sources.iter().map(|source| &source.player)),
            ["127.0.0.1:9090", "mpv.instance1", "mpv.instance2", "vlc"]
        );
        assert_eq!(
            identities(round.snapshots.iter().map(|reading| &reading.player)),
            ["127.0.0.1:9090", "mpv.instance1", "mpv.instance2"]
        );
        let failed: BTreeSet<&str> = identities(round.failures.iter().map(|(player, _)| player))
            .into_iter()
            .collect();
        assert_eq!(failed, BTreeSet::from(["haruna", "smplayer"]));
    }

    #[tokio::test]
    async fn a_listing_carries_both_in_the_order_of_identities() {
        let mut both = Both::new(
            Scripted {
                sources: vec!["vlc", "mpv"],
                ..Scripted::default()
            },
            Scripted {
                sources: vec!["127.0.0.1:9090"],
                ..Scripted::default()
            },
        );

        let sources = both.sources().await.expect("a listing");

        assert_eq!(
            identities(sources.iter().map(|source| &source.player)),
            ["127.0.0.1:9090", "mpv", "vlc"]
        );
    }

    #[tokio::test]
    async fn a_watcher_that_cannot_read_its_platform_fails_the_round() {
        for unusable in [(true, false), (false, true)] {
            let mut both = Both::new(
                Scripted {
                    sources: vec!["vlc"],
                    readings: vec!["vlc"],
                    unusable: unusable.0,
                    ..Scripted::default()
                },
                Scripted {
                    sources: vec!["127.0.0.1:9090"],
                    readings: vec!["127.0.0.1:9090"],
                    unusable: unusable.1,
                    ..Scripted::default()
                },
            );

            assert!(both.poll().await.is_err(), "{unusable:?}");
            assert!(both.sources().await.is_err(), "{unusable:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn both_are_read_together() {
        // One after the other would take twice as long.
        let takes = Duration::from_millis(400);
        let mut both = Both::new(
            Scripted {
                takes,
                ..Scripted::default()
            },
            Scripted {
                takes,
                ..Scripted::default()
            },
        );

        let started = tokio::time::Instant::now();
        both.poll().await.expect("a round");
        let polled = started.elapsed();
        let started = tokio::time::Instant::now();
        both.sources().await.expect("a listing");
        let listed = started.elapsed();

        assert_eq!((polled, listed), (takes, takes));
    }
}
