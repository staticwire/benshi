//! A login and a logout against AniList and the secret store of the session
//! the tests run in.
//!
//! Ignored: it asks AniList whose a token is and changes the store of
//! whoever runs it. Run it by hand, in a desktop session, with the token of
//! an account made for tests:
//!
//! ```sh
//! BENSHI_TOKEN_ANILIST=... cargo test -p benshi-daemon --test login_live -- --ignored
//! ```
//!
//! The token is kept where `benshi login` keeps it, under the label
//! `AniList token for benshi`, and removed at the end. The test does not run
//! where a token is kept there already, since it would replace that token.

#![cfg(target_os = "linux")]

use std::time::{SystemTime, UNIX_EPOCH};

use benshi_core::token::Token;
use benshi_daemon::accounts::{Accounts, OnAniList};
use benshi_daemon::secrets::{ANILIST, Asking, DEADLINE, SecretService, Secrets};
use benshi_sync::anilist::transport;

#[tokio::test]
#[ignore = "speaks to AniList and writes to the secret store of the session it runs in"]
async fn a_token_is_logged_in_with_and_out_of_on_anilist() {
    let token = std::env::var("BENSHI_TOKEN_ANILIST")
        .expect("BENSHI_TOKEN_ANILIST holds the token of an account made for tests");
    let token = token.trim();
    let store = SecretService::connect(DEADLINE, Asking::Never)
        .await
        .expect("a secret store on the session bus");
    assert!(
        store.read(ANILIST).await.expect("a read").is_none(),
        "a token is kept for AniList already, and this test would replace it"
    );
    let accounts = OnAniList::new(transport::ANILIST);

    let logged = accounts.login(Token::new(token)).await.expect("a login");

    // Compared and never printed: a failure here would show the token.
    let kept = store.read(ANILIST).await.expect("a read");
    assert!(
        kept.as_deref() == Some(token),
        "the token kept is not the one logged in with"
    );
    assert!(!logged.name.is_empty(), "{logged:?}");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock past the epoch")
        .as_secs();
    assert!(
        logged.expires.is_some_and(|expires| expires > now),
        "{logged:?}"
    );
    accounts.logout().await.expect("a logout");
    assert!(
        store.read(ANILIST).await.expect("a read").is_none(),
        "the token is kept after a logout"
    );
}
