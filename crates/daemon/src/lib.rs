//! Wiring, supervision, the event bus, the IPC server and configuration.
//!
//! **The event bus is the log.** `benshi watch` subscribes to exactly the
//! stream the sinks receive, so the ad-hoc debug log that a tracker always ends
//! up needing exists as a product feature and therefore cannot rot.
//!
//! Errors come in three classes and no fourth. *Transient* - a network fault, a
//! D-Bus timeout, a broken pipe - is retried with backoff. *Permanent* - an
//! expired token, an unresolvable mapping, bad configuration - stops that
//! component and says so. *Bugs* panic; a panic in one task does not take the
//! process down.
//!
//! The supervisor **owns each task's handle and observes its exit**. That is
//! what makes it impossible to bypass: a restart limiter that an exception can
//! escape past is not a limiter.

pub mod bus;

pub mod detection;

#[cfg(unix)]
pub mod ipc;

pub mod protocol;

pub mod supervisor;
