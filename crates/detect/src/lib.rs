//! Platform adapters that observe media sources.
//!
//! An adapter translates what a platform reports into a
//! [`PlayerSnapshot`] and does nothing else. It
//! takes no decisions: matching, seek detection and every policy question
//! belong to `benshi-core`, where they can be tested without a platform.
//!
//! Two rules hold for every implementation. Capabilities are declared, never
//! inferred by the consumer. Every platform call carries a deadline, because a
//! source that accepts a message and never replies must not hang the poll.

use benshi_core::{BoxError, Capabilities, PlayerSnapshot};

/// A source of playback readings.
///
/// Provisional: the methods below are a placeholder and will change when the
/// first adapter is written.
pub trait PlayerWatcher {
    /// What this adapter is able to report.
    fn capabilities(&self) -> Capabilities;

    /// Take one reading, or report that nothing is currently open.
    ///
    /// # Errors
    ///
    /// Returns an error when the platform call fails or exceeds its deadline.
    fn poll(&mut self) -> Result<Option<PlayerSnapshot>, BoxError>;
}
