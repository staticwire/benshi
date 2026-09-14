//! Status sinks: Discord, Matrix, webhooks.
//!
//! A sink receives a **snapshot of current state**, never a stream of changes.
//! The channel to each sink holds one item and overwrites it, so a sink that
//! falls behind skips intermediate states and sends the latest. That is not
//! data loss; it is the only correct semantics for a presence indicator, and it
//! dissolves by construction the family of bugs where a stale payload sits in a
//! queue behind a fresh one.
//!
//! Rate limiting belongs to the sink, because only the sink knows its own
//! service's limits - and deciding to wait is safe precisely because the sink
//! always holds the latest state.

use benshi_core::{BoxError, SessionState};

/// A destination that renders the current session state.
// Async functions in a public trait make the trait not object-safe and pin its
// future's auto-traits to the implementation. Both are acceptable here: sinks
// are held in a supervised task each, not behind `dyn`.
#[allow(async_fn_in_trait)]
pub trait StatusSink {
    /// Name of this sink, as shown to the user.
    fn name(&self) -> &str;

    /// Render `state`, or clear the display when it is `None`.
    ///
    /// `None` is an explicit clear, not a placeholder: a sink that has nothing
    /// to show must show nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination is unreachable or rejects the
    /// payload.
    async fn apply(&mut self, state: Option<&SessionState>) -> Result<(), BoxError>;
}
