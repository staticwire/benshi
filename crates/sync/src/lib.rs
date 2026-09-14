//! List backends: AniList, and later MAL, Shikimori and Kitsu.
//!
//! Recognition yields `(title, episode)` in the release's own numbering, which
//! is all a filename honestly carries. Each backend then resolves that against
//! **its own account's list**, because services model the same work
//! differently and no identifier mapping can bridge a one-to-many split.
//!
//! A backend that cannot resolve an entry says so and skips the write. Silently
//! writing progress to the wrong title is the worst failure a tracker has, and
//! guessing is how you get there.
//!
//! A new backend is finished when it passes the shared contract suite, not when
//! it seems to work.

use benshi_core::BoxError;

/// A remote list a local record can be synchronised to.
///
/// Provisional: the methods below are a placeholder and will change when the
/// first backend is written.
pub trait ListBackend {
    /// Name of this backend, as shown to the user.
    fn name(&self) -> &str;

    /// Apply one queued operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend is unreachable, rejects the request,
    /// or cannot resolve the entry the operation refers to.
    fn apply(&mut self) -> Result<(), BoxError>;
}
