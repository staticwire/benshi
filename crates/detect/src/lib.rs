//! Platform adapters that observe media sources.
//!
//! An adapter translates what a platform reports into a [`PlayerSnapshot`] and
//! does nothing else. It takes no decisions: matching, seek detection and every
//! policy question belong to `benshi-core`, where they can be tested without a
//! platform.
//!
//! Two rules hold for every implementation. Capabilities are declared, never
//! inferred by the consumer. Every platform call carries a deadline, because a
//! source that accepts a message and never replies must not hang the poll.

#[cfg(target_os = "linux")]
pub mod mpris;

pub mod replay;

use std::future::Future;
use std::time::Duration;

use benshi_core::{BoxError, PlayerId, PlayerSnapshot};

// A source is described in `core` and observed here. Re-exported so that an
// adapter and its consumers name it in the crate whose trait they implement or
// call, rather than importing the trait from one crate and its argument from
// another.
pub use benshi_core::SourceInfo;

/// Why a reading could not be taken.
///
/// The three error classes map onto these. [`WatchError::Timeout`] and
/// [`WatchError::Transport`] are transient and are retried with backoff;
/// [`WatchError::Unavailable`] is permanent for that source and stops polling
/// it until it appears again. The third class, a bug, panics and never becomes
/// a variant here.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The platform accepted the call and did not answer within its deadline.
    #[error("{player:?} did not answer within {deadline:?}")]
    Timeout {
        /// Which source failed to answer.
        player: PlayerId,
        /// The deadline it exceeded.
        deadline: Duration,
    },

    /// The transport itself failed: the bus is gone, the connection dropped.
    #[error("transport failure: {0}")]
    Transport(#[source] BoxError),

    /// The source has disappeared since it was listed.
    #[error("{0:?} is no longer present")]
    Unavailable(PlayerId),
}

/// The result of one polling round.
///
/// A round describes what it read as well as reading it, and both halves come
/// out of one observation of each source. That is what lets a consumer tell
/// whose reading it is holding: a reading names an identity, policy is keyed on
/// the application, and the rule relating the two is platform knowledge that
/// lives only in the adapter. Asking a second time would answer about a second
/// moment.
#[derive(Debug)]
pub struct PollOutcome {
    /// Every source that answered this round, whether or not it had something
    /// open.
    ///
    /// Together with [`PollOutcome::failures`] this accounts for every source
    /// the round found: one answered or it did not.
    pub sources: Vec<SourceInfo>,
    /// One reading per source that had something open and answered in time.
    pub snapshots: Vec<PlayerSnapshot>,
    /// Sources that failed this round, with the reason.
    pub failures: Vec<(PlayerId, WatchError)>,
}

/// A platform's view of every media source on it.
///
/// An implementation translates and emits. It applies no policy, filters
/// nothing and takes no decision: a denied source is still listed here, because
/// `benshi sources` must be able to show it, and the decision to ignore its
/// readings is taken by the daemon using `benshi_core::policy`.
// Desugared rather than written `async fn`, because an `async fn` in a trait
// leaves the auto traits of the future it returns undeclared, and the daemon is
// generic over this trait: `Supervisor::supervise` needs a `Send` future, and
// without the bound here no watcher can be supervised at all. An implementation
// still writes `async fn` in its `impl`, and the compiler checks that the future
// it returns satisfies the bound.
//
// The bound is a constraint on an adapter rather than a formality: a watcher,
// and everything it holds across an await, has to be able to cross threads. An
// adapter whose platform handle cannot has to keep that handle on a thread of
// its own and answer over a channel. Which platforms that applies to is not
// established here, and is the first question for whoever writes the next
// adapter; the bound is what makes it a question asked before that adapter is
// written rather than after.
pub trait PlayerWatcher {
    /// Every source the platform currently knows about.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] when the platform cannot be enumerated
    /// at all. A single unresponsive source is omitted from the listing rather
    /// than failing it.
    fn sources(&mut self) -> impl Future<Output = Result<Vec<SourceInfo>, WatchError>> + Send;

    /// Take one reading from every source, concurrently, and describe each
    /// source from the same observation.
    ///
    /// Each source carries its own deadline, so one that accepts a call and
    /// never answers delays no other source. A source with nothing open
    /// contributes no snapshot, rather than a snapshot with an empty media
    /// reference, but it is still described in [`PollOutcome::sources`].
    ///
    /// [`PlayerWatcher::sources`] answers what is there without taking
    /// readings; this answers it for every source that answered.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] only when the platform itself is
    /// unusable. A per-source failure is reported in [`PollOutcome::failures`]
    /// and does not fail the poll: one unresponsive player must not blind the
    /// daemon to the rest.
    fn poll(&mut self) -> impl Future<Output = Result<PollOutcome, WatchError>> + Send;
}
