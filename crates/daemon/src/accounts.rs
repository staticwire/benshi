//! Logging in to AniList, and out of it.
//!
//! A login asks AniList whose a token is and keeps the token in the secret
//! store, and a logout removes it. A token AniList does not take is not kept,
//! so a mistyped token never replaces a working one.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use benshi_core::token::Token;
use benshi_sync::anilist::AniList;
#[cfg(target_os = "linux")]
use benshi_sync::anilist::transport::Http;
use benshi_sync::anilist::transport::Transport;
use serde_json::Value;

use crate::secrets::{self, ANILIST, Secrets};
#[cfg(target_os = "linux")]
use crate::secrets::{Asking, SecretService};

/// Whose a token is, and when it runs out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedIn {
    /// The name of the account on AniList.
    pub name: String,
    /// When the token runs out, in seconds since the Unix epoch, and none
    /// where the token does not say.
    pub expires: Option<u64>,
}

/// Why a login or a logout was not carried out.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// AniList did not take the token, or could not be asked.
    #[error("AniList {0}")]
    Service(benshi_sync::Error),
    /// The secret store did not keep the token.
    #[error("token could not be kept: {0}")]
    NotKept(secrets::Error),
    /// The secret store did not remove the token.
    #[error("token could not be removed: {0}")]
    NotRemoved(secrets::Error),
}

/// What a login or a logout comes to, once it is done.
pub type Done<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Logins and logouts, as the socket asks for them.
pub trait Accounts: fmt::Debug + Send + Sync {
    /// Check `token` with AniList, and keep it.
    fn login(&self, token: Token) -> Done<'_, LoggedIn>;

    /// Remove the token kept.
    fn logout(&self) -> Done<'_, ()>;
}

/// Logins and logouts against AniList, with the token kept by the Secret
/// Service of the session.
///
/// Each call is made for a command a person runs, so the store may ask them
/// to unlock it, and their answer is waited for up to
/// [`PROMPT_WAIT`](secrets::PROMPT_WAIT).
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct OnAniList {
    address: String,
}

#[cfg(target_os = "linux")]
impl OnAniList {
    /// Logins checked with the service at `address`.
    #[must_use]
    pub fn new(address: &str) -> Self {
        Self {
            address: address.to_owned(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Accounts for OnAniList {
    fn login(&self, token: Token) -> Done<'_, LoggedIn> {
        Box::pin(async move {
            let secrets = asked().await.map_err(Error::NotKept)?;
            let http = Http::new(&self.address, token.secret()).map_err(|unbuilt| {
                Error::Service(benshi_sync::Error::Unreachable(unbuilt.into()))
            })?;

            login(&AniList::new(http), &secrets, &token).await
        })
    }

    fn logout(&self) -> Done<'_, ()> {
        Box::pin(async {
            let secrets = asked().await.map_err(Error::NotRemoved)?;

            logout(&secrets).await
        })
    }
}

/// The Secret Service of the session, for a call that may ask a person.
#[cfg(target_os = "linux")]
async fn asked() -> Result<SecretService, secrets::Error> {
    SecretService::connect(secrets::DEADLINE, Asking::Within(secrets::PROMPT_WAIT)).await
}

/// Ask `service` whose `token` is, and keep the token in `secrets`.
///
/// # Errors
///
/// [`Error::Service`] where AniList does not take the token or cannot be
/// asked, and nothing is kept; [`Error::NotKept`] where the store does not
/// keep it.
pub async fn login<T: Transport + Sync>(
    service: &AniList<T>,
    secrets: &impl Secrets,
    token: &Token,
) -> Result<LoggedIn, Error> {
    let account = service.account().await.map_err(Error::Service)?;
    secrets
        .write(ANILIST, token.secret())
        .await
        .map_err(Error::NotKept)?;

    Ok(LoggedIn {
        name: account.name,
        expires: expires(token),
    })
}

/// Remove the token kept in `secrets`.
///
/// # Errors
///
/// [`Error::NotRemoved`] where the store does not remove it.
pub async fn logout(secrets: &impl Secrets) -> Result<(), Error> {
    secrets.remove(ANILIST).await.map_err(Error::NotRemoved)
}

/// When `token` runs out: the `exp` of its claims, read as a JWT.
///
/// AniList's documentation gives its tokens as JWTs. A token of another
/// shape, or whose claims hold no `exp` of whole seconds since the Unix
/// epoch, says nothing.
fn expires(token: &Token) -> Option<u64> {
    let mut parts = token.secret().split('.');
    let (Some(_header), Some(claims), Some(_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()?;

    claims.get("exp")?.as_u64()
}

#[cfg(test)]
mod tests {
    use std::future::{self, Future};
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use benshi_core::token::Token;
    use benshi_sync::anilist::AniList;
    use benshi_sync::anilist::transport::{Answer, Request, Transport};
    use serde_json::{Value, json};

    use super::{Error, LoggedIn, expires, login, logout};
    use crate::secrets::{self, ANILIST, Key, Memory, Secrets};

    /// When the tokens of these tests run out.
    const EXP: u64 = 1_900_000_000;

    /// A token shaped as AniList's are: a JWT whose claims hold `claims`.
    fn a_token_claiming(claims: &Value) -> Token {
        Token::new(format!(
            "{}.{}.{}",
            URL_SAFE_NO_PAD.encode(r#"{"typ":"JWT","alg":"RS256"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string()),
            URL_SAFE_NO_PAD.encode("not a signature"),
        ))
    }

    fn a_token() -> Token {
        a_token_claiming(&json!({ "aud": "1", "sub": "2", "exp": EXP }))
    }

    /// A service that answers every request with one answer.
    struct Answering(Answer);

    impl Transport for Answering {
        fn send(
            &self,
            _request: &Request,
        ) -> impl Future<Output = Result<Answer, benshi_sync::Error>> + Send {
            future::ready(Ok(self.0.clone()))
        }
    }

    /// AniList, answering whose a token is.
    fn anilist_taking() -> AniList<Answering> {
        AniList::new(Answering(Answer {
            status: 200,
            retry_after: None,
            body: json!({ "data": { "Viewer": { "id": 2, "name": "someone" } } }).to_string(),
        }))
    }

    /// AniList, refusing a token as it refuses one that is not a token.
    fn anilist_refusing() -> AniList<Answering> {
        AniList::new(Answering(Answer {
            status: 400,
            retry_after: None,
            body:
                json!({ "data": null, "errors": [{ "message": "Invalid token", "status": 400 }] })
                    .to_string(),
        }))
    }

    /// A store that is locked and may not ask anyone.
    struct Locked;

    impl Secrets for Locked {
        fn read(
            &self,
            _key: Key,
        ) -> impl Future<Output = Result<Option<String>, secrets::Error>> + Send {
            future::ready(Err(secrets::Error::Locked))
        }

        fn write(
            &self,
            _key: Key,
            _token: &str,
        ) -> impl Future<Output = Result<(), secrets::Error>> + Send {
            future::ready(Err(secrets::Error::Locked))
        }

        fn remove(&self, _key: Key) -> impl Future<Output = Result<(), secrets::Error>> + Send {
            future::ready(Err(secrets::Error::Locked))
        }
    }

    async fn kept(store: &Memory) -> Option<String> {
        store.read(ANILIST).await.expect("memory answers")
    }

    #[tokio::test]
    async fn a_token_anilist_takes_is_kept_and_answered_with_its_account() {
        let store = Memory::default();
        let token = a_token();

        let logged = login(&anilist_taking(), &store, &token).await;

        assert_eq!(
            logged.expect("the login is done"),
            LoggedIn {
                name: "someone".to_owned(),
                expires: Some(EXP),
            }
        );
        assert_eq!(kept(&store).await.as_deref(), Some(token.secret()));
    }

    #[tokio::test]
    async fn a_token_anilist_refuses_is_not_kept_in_place_of_the_one_before() {
        let store = Memory::default();
        store
            .write(ANILIST, "the token before")
            .await
            .expect("memory keeps it");

        let logged = login(&anilist_refusing(), &store, &a_token()).await;

        assert!(
            matches!(
                &logged,
                Err(Error::Service(benshi_sync::Error::Refused(said))) if said == "Invalid token"
            ),
            "{logged:?}"
        );
        assert_eq!(kept(&store).await.as_deref(), Some("the token before"));
    }

    #[tokio::test]
    async fn a_token_the_store_does_not_keep_is_not_a_login() {
        let logged = login(&anilist_taking(), &Locked, &a_token()).await;

        assert!(
            matches!(logged, Err(Error::NotKept(secrets::Error::Locked))),
            "{logged:?}"
        );
    }

    #[tokio::test]
    async fn a_logout_removes_the_token_kept() {
        let store = Memory::default();
        store
            .write(ANILIST, "a token")
            .await
            .expect("memory keeps it");

        logout(&store).await.expect("the logout is done");

        assert_eq!(kept(&store).await, None);
    }

    #[tokio::test]
    async fn a_logout_with_no_token_kept_is_done() {
        logout(&Memory::default())
            .await
            .expect("the logout is done");
    }

    #[tokio::test]
    async fn a_logout_the_store_does_not_carry_out_is_said() {
        let logged_out = logout(&Locked).await;

        assert!(
            matches!(logged_out, Err(Error::NotRemoved(secrets::Error::Locked))),
            "{logged_out:?}"
        );
    }

    #[test]
    fn a_token_says_when_it_runs_out_in_its_claims() {
        assert_eq!(expires(&a_token()), Some(EXP));
    }

    #[test]
    fn a_token_whose_claims_cannot_be_read_does_not_say_when_it_runs_out() {
        let claims = URL_SAFE_NO_PAD.encode(json!({ "exp": EXP }).to_string());
        let unread = [
            Token::new("not a jwt"),
            Token::new(format!("header.{claims}")),
            Token::new(format!("header.{claims}.signature.more")),
            Token::new("header.!!!.signature"),
            Token::new(format!(
                "header.{}.signature",
                URL_SAFE_NO_PAD.encode("not json")
            )),
            a_token_claiming(&json!({ "sub": "2" })),
            a_token_claiming(&json!({ "exp": "soon" })),
            a_token_claiming(&json!({ "exp": 1.5 })),
            a_token_claiming(&json!({ "exp": -1 })),
        ];

        for token in unread {
            assert_eq!(expires(&token), None, "{}", token.secret());
        }
    }

    #[test]
    fn each_failure_is_said_as_decided() {
        let said = [
            (
                Error::Service(benshi_sync::Error::Refused("Invalid token".to_owned())),
                "AniList refused the call: Invalid token",
            ),
            (
                Error::Service(benshi_sync::Error::Timeout {
                    deadline: Duration::from_secs(15),
                }),
                "AniList did not answer within 15s",
            ),
            (
                Error::NotKept(secrets::Error::Locked),
                "token could not be kept: secret store is locked",
            ),
            (
                Error::NotKept(secrets::Error::Dismissed),
                "token could not be kept: unlocking the secret store was dismissed",
            ),
            (
                Error::NotRemoved(secrets::Error::Locked),
                "token could not be removed: secret store is locked",
            ),
        ];

        for (failure, line) in said {
            assert_eq!(failure.to_string(), line);
        }
    }
}
