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

use std::time::Duration;

use benshi_core::{AppName, BoxError, Capabilities, PlayState, PlayerId, PlayerSnapshot};

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

/// One source as the platform currently describes it.
///
/// Capabilities belong to the source rather than to the adapter: mpv and a
/// browser are observed through the same MPRIS adapter and do not report the
/// same things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// The stable identity the platform provides, unique per instance.
    pub player: PlayerId,
    /// The application this source belongs to, which policy is keyed on.
    pub app: AppName,
    /// What this source is able to report.
    pub capabilities: Capabilities,
    /// What it is doing, as far as it will say.
    pub state: PlayState,
}

/// The result of one polling round.
#[derive(Debug)]
pub struct PollOutcome {
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
// An `async fn` in a public trait leaves the auto traits of the future it
// returns undeclared, so code generic over this trait cannot require `Send`.
// Accepted: a concrete adapter's future carries its own auto traits to the
// caller regardless, which is how the daemon will await one. Generic code that
// does need the bound must desugar the method to
// `fn poll(&mut self) -> impl Future<Output = ...> + Send`.
#[allow(async_fn_in_trait)]
pub trait PlayerWatcher {
    /// Every source the platform currently knows about.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] when the platform cannot be enumerated
    /// at all. A single unresponsive source is omitted from the listing rather
    /// than failing it.
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError>;

    /// Take one reading from every source, concurrently.
    ///
    /// Each source carries its own deadline, so one that accepts a call and
    /// never answers delays no other source. A source with nothing open
    /// contributes no snapshot, rather than a snapshot with an empty media
    /// reference.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] only when the platform itself is
    /// unusable. A per-source failure is reported in [`PollOutcome::failures`]
    /// and does not fail the poll: one unresponsive player must not blind the
    /// daemon to the rest.
    async fn poll(&mut self) -> Result<PollOutcome, WatchError>;
}
