//! The canonical local store.
//!
//! One SQLite database holds everything: `shows`, `episodes_seen`,
//! `sync_queue`, `backend_ids`, `altnames`, `library` and `meta`. Migrations are
//! numbered and each has a test.
//!
//! Recording an episode cannot fail. A network problem, a dead API or an
//! expired token is a delivery problem, not a recording problem - so marking an
//! episode locally and enqueueing its sync operation are **one transaction
//! across two tables**, and the queue holds idempotent operations rather than
//! state snapshots.
