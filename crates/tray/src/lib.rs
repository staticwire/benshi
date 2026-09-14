//! The optional tray icon.
//!
//! This crate is grouped with the daemon rather than treated as a user
//! interface, because its hard problem is thread ownership, not drawing. On
//! Windows and macOS the GUI event loop must own the main thread, so the main
//! thread runs the tray and the async runtime runs on a worker thread. Without
//! a tray the main thread simply hands itself to the runtime.
//!
//! The tray reports capabilities honestly: when the active detection method
//! cannot report position it says so, rather than showing an empty progress bar
//! and leaving the user to guess.
