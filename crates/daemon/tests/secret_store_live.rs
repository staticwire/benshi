//! The secret store of the session the tests run in, written to, read and
//! cleared.
//!
//! Ignored: it changes the store of whoever runs it. Run it by hand, in a
//! desktop session:
//!
//! ```sh
//! cargo test -p benshi-daemon --test secret_store_live -- --ignored
//! ```
//!
//! It keeps one item under a label of its own and removes it at the end. An
//! item a failed run leaves is removed at the start of the next. Where the
//! store is locked it opens the store's window to unlock it.

#![cfg(target_os = "linux")]

use benshi_daemon::secrets::{Asking, DEADLINE, Key, PROMPT_WAIT, SecretService, Secrets};

/// An item a person who finds it knows to be a test's.
const LIVE: Key = Key {
    backend: "live-test",
    label: "benshi live test, safe to delete",
};

#[tokio::test]
#[ignore = "writes to the secret store of the session it runs in"]
async fn a_token_goes_through_the_secret_store_of_this_session() {
    let store = SecretService::connect(DEADLINE, Asking::Within(PROMPT_WAIT))
        .await
        .expect("a secret store on the session bus");
    store
        .remove(LIVE)
        .await
        .expect("an item an earlier run left is removed");

    assert_eq!(store.read(LIVE).await.expect("a read"), None);
    store.write(LIVE, "not a token").await.expect("a write");
    assert_eq!(
        store.read(LIVE).await.expect("a read").as_deref(),
        Some("not a token")
    );
    store.write(LIVE, "written again").await.expect("a write");
    assert_eq!(
        store.read(LIVE).await.expect("a read").as_deref(),
        Some("written again")
    );
    store.remove(LIVE).await.expect("a removal");
    assert_eq!(store.read(LIVE).await.expect("a read"), None);
}
