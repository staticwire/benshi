//! The event bus, which is also the log.
//!
//! Every subscriber receives the same sequence. `benshi watch` is one
//! subscriber and a sink is another, so the stream a user can read is the
//! stream the program acts on, rather than a separate debug channel that is
//! written once and then rots.
//!
//! A subscriber that stops reading is told how much it missed rather than
//! quietly skipped. Losing events on a diagnostic stream is acceptable; losing
//! them silently is not, because a gap nobody reports reads as nothing having
//! happened.

use benshi_core::{PlayerId, PlayerSnapshot, SessionState};
use serde::Serialize;
use tokio::sync::broadcast;

/// How many events the bus keeps for a subscriber that has fallen behind.
///
/// One round of polling emits an event per source that has something open and
/// is not denied, and the membership only when it changes, so a desktop with a
/// handful of players fills this in under a minute at the one-second default
/// interval. A client on a local socket that has not read for that long has
/// stopped reading rather than fallen behind, and dropping its backlog is then
/// the right answer. The cost of being generous is small: an event is a few
/// hundred bytes, so the whole buffer is tens of kilobytes.
pub const CAPACITY: usize = 256;

/// Something the daemon did or saw.
///
/// Serialisable so that `benshi watch` can carry these over the socket exactly
/// as they are published, rather than through a second representation free to
/// drift from this one. Nothing reads the other end yet, so only `Serialize` is
/// derived; the client adds `Deserialize` and the round trip that proves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum BusEvent {
    /// A reading was taken from a source.
    Snapshot(PlayerSnapshot),
    /// The set of sources the platform reports has changed.
    ///
    /// Carries the membership rather than its size, because a size cannot show
    /// one player closing while another opens: the number is the same and the
    /// world is not. A subscriber that joined late learns the whole set from
    /// the first change it sees, rather than a difference it holds no state to
    /// apply.
    SourcesChanged {
        /// Every source now listed, in identity order.
        sources: Vec<PlayerId>,
    },
    /// One source could not be read this round.
    SourceFailed {
        /// Which source failed.
        player: PlayerId,
        /// What went wrong, rendered for a person.
        reason: String,
    },
    /// The session state a sink would render, or its absence.
    State(Option<SessionState>),
}

/// A published sequence of events with many readers.
#[derive(Debug)]
pub struct EventBus {
    sender: broadcast::Sender<BusEvent>,
}

impl EventBus {
    /// A bus holding [`CAPACITY`] events for a subscriber that falls behind.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(CAPACITY)
    }

    /// A bus holding at least `capacity` events for a subscriber that falls
    /// behind.
    ///
    /// The channel underneath rounds a capacity that is not a power of two up
    /// to the next one and measures lag against the rounded figure, so asking
    /// for a hundred keeps a hundred and twenty-eight.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _first) = broadcast::channel(capacity);

        Self { sender }
    }

    /// Read every event published from now on.
    ///
    /// A subscriber sees what is published after it subscribes and nothing
    /// before, so `benshi watch` starts at the present rather than replaying a
    /// backlog it has no context for.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.sender.subscribe()
    }

    /// Publish one event to every subscriber.
    pub fn publish(&self, event: BusEvent) {
        // The only failure is that nothing is subscribed, which is the ordinary
        // case: the daemon publishes whether or not anyone is watching. The
        // lint against discarding a `Result` is answered by saying so here
        // rather than by silencing it.
        if let Err(no_subscribers) = self.sender.send(event) {
            drop(no_subscribers);
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{BusEvent, EventBus};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::{Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
    use std::time::Duration;
    use tokio::sync::broadcast::error::TryRecvError;

    fn a_snapshot() -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        }
    }

    /// A listing of one source, named so that two of them differ.
    fn a_listing(which: usize) -> BusEvent {
        BusEvent::SourcesChanged {
            sources: vec![PlayerId(format!("player{which}"))],
        }
    }

    #[test]
    fn two_subscribers_receive_the_same_sequence() {
        // What a user watching the stream sees is what the program acts on. A
        // separate diagnostic channel would be free to disagree with this one,
        // and would, the first time someone forgot to write to both.
        let bus = EventBus::new();
        let mut watching = bus.subscribe();
        let mut sink_side = bus.subscribe();

        bus.publish(a_listing(1));
        bus.publish(BusEvent::Snapshot(a_snapshot()));

        assert_eq!(
            watching.try_recv().expect("an event"),
            sink_side.try_recv().expect("an event")
        );
        assert_eq!(
            watching.try_recv().expect("an event"),
            sink_side.try_recv().expect("an event")
        );
    }

    #[test]
    fn publishing_with_no_subscribers_is_not_an_error() {
        // The daemon publishes whether or not anyone is watching, and nobody
        // watching is the ordinary case.
        let bus = EventBus::new();

        bus.publish(a_listing(0));
    }

    #[test]
    fn a_slow_subscriber_is_told_it_lagged_rather_than_silently_skipped() {
        let bus = EventBus::with_capacity(2);
        let mut slow = bus.subscribe();

        for which in 0..5 {
            bus.publish(a_listing(which));
        }

        assert!(matches!(slow.try_recv(), Err(TryRecvError::Lagged(_))));
    }

    #[test]
    fn a_subscriber_that_lagged_keeps_receiving() {
        // A gap must not end the stream. `benshi watch` reports how much it
        // missed and carries on, so a client that stalled once stays useful.
        let bus = EventBus::with_capacity(2);
        let mut slow = bus.subscribe();

        for which in 0..5 {
            bus.publish(a_listing(which));
        }

        let lag = slow.try_recv();
        assert!(matches!(lag, Err(TryRecvError::Lagged(_))));

        let next = slow.try_recv().expect("the stream continues after a lag");
        assert_eq!(next, a_listing(3));
    }

    #[test]
    fn an_event_serialises_as_published() {
        // `benshi watch` streams these over the socket unchanged, so the wire
        // form is this type and there is no second representation to drift.
        let event = BusEvent::Snapshot(a_snapshot());

        let text = serde_json::to_string(&event).expect("serialise");

        assert!(text.contains("Snapshot"), "got {text}");
        assert!(text.contains("/anime/ep 03.mkv"), "got {text}");
    }
}
