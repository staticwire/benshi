//! Where a token is kept: the store in memory, and the Secret Service API
//! against a service of the test's own over a socket pair.
//!
//! The clauses every store keeps are written once and run against each. What
//! is particular to the Secret Service follows them.

#[cfg(target_os = "linux")]
mod fake_secret_service;

use benshi_daemon::secrets::{ANILIST, Key, Memory, Secrets};

/// A key of another backend, for the clause that keeps the two apart.
const OTHER: Key = Key {
    backend: "other",
    label: "other token for benshi",
};

/// A token built to the shape of none in particular.
const TOKEN: &str = "eyJ0eXAiOiJKV1QifQ.c2lnbmVk.dG9rZW4";

/// The clauses every store keeps, each a test of its own for each store.
macro_rules! every_store_keeps {
    ($store:ident, $made:ident) => {
        mod $store {
            use super::*;

            #[tokio::test]
            async fn nothing_kept_is_read_as_nothing() {
                $made!(store);

                assert_eq!(store.read(ANILIST).await.expect("a read"), None);
            }

            #[tokio::test]
            async fn a_token_written_is_read_back() {
                $made!(store);

                store.write(ANILIST, TOKEN).await.expect("a write");

                assert_eq!(
                    store.read(ANILIST).await.expect("a read").as_deref(),
                    Some(TOKEN)
                );
            }

            #[tokio::test]
            async fn a_token_written_again_takes_the_place_of_the_first() {
                $made!(store);

                store.write(ANILIST, "the first").await.expect("a write");
                store.write(ANILIST, TOKEN).await.expect("a second write");

                assert_eq!(
                    store.read(ANILIST).await.expect("a read").as_deref(),
                    Some(TOKEN)
                );
            }

            #[tokio::test]
            async fn a_token_removed_is_read_as_nothing() {
                $made!(store);
                store.write(ANILIST, TOKEN).await.expect("a write");

                store.remove(ANILIST).await.expect("a removal");

                assert_eq!(store.read(ANILIST).await.expect("a read"), None);
            }

            #[tokio::test]
            async fn removing_what_is_not_kept_does_nothing() {
                $made!(store);
                store.write(OTHER, "of the other").await.expect("a write");

                store.remove(ANILIST).await.expect("a removal of nothing");

                assert_eq!(
                    store.read(OTHER).await.expect("a read").as_deref(),
                    Some("of the other")
                );
            }

            #[tokio::test]
            async fn the_tokens_of_two_backends_are_kept_apart() {
                $made!(store);

                store.write(ANILIST, TOKEN).await.expect("a write");
                store.write(OTHER, "of the other").await.expect("a write");

                assert_eq!(
                    store.read(ANILIST).await.expect("a read").as_deref(),
                    Some(TOKEN)
                );
                assert_eq!(
                    store.read(OTHER).await.expect("a read").as_deref(),
                    Some("of the other")
                );
            }
        }
    };
}

macro_rules! in_memory {
    ($store:ident) => {
        let $store = Memory::default();
    };
}

every_store_keeps!(memory, in_memory);

#[cfg(target_os = "linux")]
macro_rules! over_the_secret_service {
    ($store:ident) => {
        let fake = crate::fake_secret_service::Fake::default();
        let ($store, _serving) = fake
            .serve(
                benshi_daemon::secrets::DEADLINE,
                benshi_daemon::secrets::Asking::Never,
            )
            .await;
    };
}

#[cfg(target_os = "linux")]
every_store_keeps!(secret_service, over_the_secret_service);

#[cfg(target_os = "linux")]
mod the_secret_service {
    use std::collections::HashMap;
    use std::future::Future;
    use std::time::Duration;

    use benshi_daemon::secrets::{ANILIST, Asking, DEADLINE, Error, Secrets};

    use super::TOKEN;
    use crate::fake_secret_service::{Fake, Fault, Item, Person};

    /// A deadline, or a wait for a person, that a test which waits for it
    /// spends whole.
    const SHORT: Duration = Duration::from_millis(200);

    /// A bound on a test whose call gives up on its own, so that one which
    /// does not ends the test.
    const BOUND: Duration = Duration::from_secs(5);

    async fn bounded<T>(call: impl Future<Output = T>) -> T {
        tokio::time::timeout(BOUND, call)
            .await
            .expect("the call gives up on its own")
    }

    fn locked(value: &[u8]) -> Item {
        Item {
            locked: true,
            ..Item::of("anilist", value)
        }
    }

    fn asking() -> Asking {
        Asking::Within(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn a_token_is_written_to_the_default_collection_as_agreed() {
        let fake = Fake::default();
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        store.write(ANILIST, TOKEN).await.expect("a write");

        let state = fake.state();
        let items: Vec<&Item> = state.items.values().collect();
        assert_eq!(
            items,
            [&Item {
                label: "AniList token for benshi".to_owned(),
                attributes: HashMap::from([
                    ("application".to_owned(), "benshi".to_owned()),
                    ("backend".to_owned(), "anilist".to_owned()),
                ]),
                parameters: Vec::new(),
                value: TOKEN.as_bytes().to_vec(),
                content_type: "text/plain; charset=utf8".to_owned(),
                locked: false,
            }]
        );
        assert_eq!(state.replaced, [true]);
    }

    #[tokio::test]
    async fn a_session_is_opened_in_the_clear_and_closed_after_each_call() {
        let fake = Fake::default();
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        store.write(ANILIST, TOKEN).await.expect("a write");
        store.read(ANILIST).await.expect("a read");

        let state = fake.state();
        let opened: Vec<&String> = state
            .calls
            .iter()
            .filter(|call| call.starts_with("OpenSession"))
            .collect();
        assert_eq!(opened, ["OpenSession plain \"\"", "OpenSession plain \"\""]);
        assert_eq!(
            state
                .methods()
                .iter()
                .filter(|method| **method == "Close")
                .count(),
            2
        );
        assert!(state.sessions.is_empty(), "{:?} left open", state.sessions);
    }

    #[tokio::test]
    async fn a_removal_opens_no_session() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        store.remove(ANILIST).await.expect("a removal");

        assert_eq!(fake.state().methods(), ["SearchItems", "Delete"]);
    }

    #[tokio::test]
    async fn a_locked_token_is_not_read_where_nobody_may_be_asked() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::Locked)), "{read:?}");
        let methods = fake.state().methods().join(" ");
        assert!(!methods.contains("Unlock"), "{methods}");
        assert!(!methods.contains("GetSecret"), "{methods}");
    }

    #[tokio::test]
    async fn a_locked_token_is_read_once_the_person_asked_unlocks() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        let read = store.read(ANILIST).await.expect("a read");

        assert_eq!(read.as_deref(), Some(TOKEN));
        let state = fake.state();
        let prompted: Vec<&String> = state
            .calls
            .iter()
            .filter(|call| call.starts_with("Prompt"))
            .collect();
        assert_eq!(prompted, ["Prompt \"\""]);
    }

    #[tokio::test]
    async fn a_window_the_person_dismisses_is_said_as_dismissed() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        fake.state().person = Person::Dismisses;
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::Dismissed)), "{read:?}");
    }

    #[tokio::test]
    async fn a_window_nobody_answers_is_given_up_on_and_dismissed() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        fake.state().person = Person::Away;
        let (store, _serving) = fake.serve(DEADLINE, Asking::Within(SHORT)).await;

        let read = bounded(store.read(ANILIST)).await;

        assert!(
            matches!(read, Err(Error::Timeout { deadline }) if deadline == SHORT),
            "{read:?}"
        );
        assert_eq!(fake.state().methods().last(), Some(&"Close"));
        assert!(fake.state().methods().contains(&"Dismiss"));
    }

    #[tokio::test]
    async fn a_locked_collection_is_unlocked_before_a_token_is_written() {
        let fake = Fake::default();
        fake.state().collection_locked = true;
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        store.write(ANILIST, TOKEN).await.expect("a write");

        assert_eq!(fake.state().items.len(), 1);
        let methods = fake.state().methods().join(" ");
        assert!(methods.contains("Unlock Prompt CreateItem"), "{methods}");
    }

    #[tokio::test]
    async fn a_locked_collection_is_not_written_to_where_nobody_may_be_asked() {
        let fake = Fake::default();
        fake.state().collection_locked = true;
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let written = store.write(ANILIST, TOKEN).await;

        assert!(matches!(written, Err(Error::Locked)), "{written:?}");
        assert!(fake.state().items.is_empty());
        assert!(!fake.state().methods().contains(&"Unlock"));
    }

    #[tokio::test]
    async fn a_write_the_store_asks_about_is_done_once_the_person_agrees() {
        let fake = Fake::default();
        fake.state().asking.insert("CreateItem");
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        store.write(ANILIST, TOKEN).await.expect("a write");

        assert_eq!(fake.state().items.len(), 1);
    }

    #[tokio::test]
    async fn a_removal_the_store_asks_about_is_done_once_the_person_agrees() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        fake.state().asking.insert("Delete");
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        store.remove(ANILIST).await.expect("a removal");

        assert!(fake.state().items.is_empty());
    }

    #[tokio::test]
    async fn a_locked_token_is_unlocked_before_it_is_removed() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        store.remove(ANILIST).await.expect("a removal");

        assert!(fake.state().items.is_empty());
    }

    #[tokio::test]
    async fn a_store_with_no_default_collection_refuses_a_write() {
        let fake = Fake::default();
        fake.state().aliased = false;
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let written = store.write(ANILIST, TOKEN).await;

        assert_eq!(
            written.map_err(|refused| refused.to_string()),
            Err("secret store refused the call: no default collection".to_owned())
        );
    }

    #[tokio::test]
    async fn no_store_on_the_bus_is_unreachable() {
        let fake = Fake::default();
        fake.state()
            .faults
            .insert("OpenSession", Fault::ServiceUnknown);
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await;

        assert_eq!(
            read.map_err(|unreached| unreached.to_string()),
            Err(
                "secret store could not be reached: org.freedesktop.DBus.Error.ServiceUnknown: \
                 The name org.freedesktop.secrets was not provided by any .service files"
                    .to_owned()
            )
        );
    }

    #[tokio::test]
    async fn an_object_the_store_says_is_locked_is_locked() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        fake.state().faults.insert("GetSecret", Fault::IsLocked);
        let (store, _serving) = fake.serve(DEADLINE, asking()).await;

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::Locked)), "{read:?}");
    }

    #[tokio::test]
    async fn another_error_of_the_store_is_a_refusal_in_its_words() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        fake.state().faults.insert("GetSecret", Fault::NoSession);
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await;

        assert_eq!(
            read.map_err(|refused| refused.to_string()),
            Err(
                "secret store refused the call: org.freedesktop.Secret.Error.NoSession: \
                 no such session"
                    .to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_token_that_is_not_utf_8_is_not_text() {
        let fake = Fake::default();
        fake.state().items.insert(1, Item::of("anilist", b"\xff"));
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::NotText)), "{read:?}");
    }

    /// What a test asks the store to do.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Asked {
        Read,
        ReadLocked,
        Write,
        Remove,
    }

    #[tokio::test]
    async fn a_call_the_store_does_not_answer_is_given_up_on_at_the_deadline() {
        for (method, asked) in [
            ("OpenSession", Asked::Read),
            ("SearchItems", Asked::Read),
            ("GetSecret", Asked::Read),
            ("Unlock", Asked::ReadLocked),
            ("Prompt", Asked::ReadLocked),
            ("ReadAlias", Asked::Write),
            ("Locked", Asked::Write),
            ("CreateItem", Asked::Write),
            ("SearchItems", Asked::Remove),
            ("Delete", Asked::Remove),
        ] {
            let fake = Fake::default();
            let item = if asked == Asked::ReadLocked {
                locked(TOKEN.as_bytes())
            } else {
                Item::of("anilist", TOKEN.as_bytes())
            };
            fake.state().items.insert(1, item);
            fake.state().faults.insert(method, Fault::Silent);
            let (store, _serving) = fake.serve(SHORT, asking()).await;

            let outcome = bounded(async {
                match asked {
                    Asked::Read | Asked::ReadLocked => store.read(ANILIST).await.map(drop),
                    Asked::Write => store.write(ANILIST, TOKEN).await,
                    Asked::Remove => store.remove(ANILIST).await,
                }
            })
            .await;

            assert!(
                matches!(outcome, Err(Error::Timeout { deadline }) if deadline == SHORT),
                "{method}: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_dismissal_the_store_does_not_answer_still_ends_as_locked() {
        let fake = Fake::default();
        fake.state().asking.insert("CreateItem");
        fake.state().faults.insert("Dismiss", Fault::Silent);
        let (store, _serving) = fake.serve(SHORT, Asking::Never).await;

        let written = bounded(store.write(ANILIST, TOKEN)).await;

        assert!(matches!(written, Err(Error::Locked)), "{written:?}");
    }

    #[tokio::test]
    async fn a_write_the_store_asks_about_is_not_made_where_nobody_may_be_asked() {
        let fake = Fake::default();
        fake.state().asking.insert("CreateItem");
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let written = store.write(ANILIST, TOKEN).await;

        assert!(matches!(written, Err(Error::Locked)), "{written:?}");
        assert!(fake.state().items.is_empty());
        let methods = fake.state().methods().join(" ");
        assert!(methods.contains("Dismiss"), "{methods}");
        assert!(!methods.contains("Prompt"), "{methods}");
    }

    #[tokio::test]
    async fn a_removal_the_store_asks_about_is_not_made_where_nobody_may_be_asked() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        fake.state().asking.insert("Delete");
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let removed = store.remove(ANILIST).await;

        assert!(matches!(removed, Err(Error::Locked)), "{removed:?}");
        assert_eq!(fake.state().items.len(), 1);
        assert!(fake.state().methods().contains(&"Dismiss"));
    }

    #[tokio::test]
    async fn a_locked_token_is_not_removed_where_nobody_may_be_asked() {
        let fake = Fake::default();
        fake.state().items.insert(1, locked(TOKEN.as_bytes()));
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let removed = store.remove(ANILIST).await;

        assert!(matches!(removed, Err(Error::Locked)), "{removed:?}");
        assert_eq!(fake.state().items.len(), 1);
        assert!(!fake.state().methods().contains(&"Unlock"));
    }

    #[tokio::test]
    async fn a_copy_that_is_unlocked_is_read_before_one_that_is_locked() {
        let fake = Fake::default();
        fake.state().items.insert(1, locked(b"the locked copy"));
        fake.state()
            .items
            .insert(2, Item::of("anilist", TOKEN.as_bytes()));
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await.expect("a read");

        assert_eq!(read.as_deref(), Some(TOKEN));
    }

    #[tokio::test]
    async fn every_copy_of_a_token_is_removed() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", b"the first copy"));
        fake.state()
            .items
            .insert(2, Item::of("anilist", TOKEN.as_bytes()));
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        store.remove(ANILIST).await.expect("a removal");

        assert!(fake.state().items.is_empty());
    }

    #[tokio::test]
    async fn a_store_the_bus_cannot_start_is_unreachable() {
        let fake = Fake::default();
        fake.state()
            .faults
            .insert("OpenSession", Fault::SpawnFailed);
        let (store, _serving) = fake.serve(DEADLINE, Asking::Never).await;

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::Unreachable(_))), "{read:?}");
    }

    #[tokio::test]
    async fn a_session_the_store_does_not_close_keeps_the_token_read() {
        let fake = Fake::default();
        fake.state()
            .items
            .insert(1, Item::of("anilist", TOKEN.as_bytes()));
        fake.state().faults.insert("Close", Fault::Silent);
        let (store, _serving) = fake.serve(SHORT, Asking::Never).await;

        let read = bounded(store.read(ANILIST)).await.expect("a read");

        assert_eq!(read.as_deref(), Some(TOKEN));
    }

    #[tokio::test]
    async fn a_store_that_is_gone_is_unreachable() {
        let fake = Fake::default();
        let (store, serving) = fake.serve(DEADLINE, Asking::Never).await;
        drop(serving);

        let read = store.read(ANILIST).await;

        assert!(matches!(read, Err(Error::Unreachable(_))), "{read:?}");
    }

    #[tokio::test]
    async fn a_store_that_goes_while_the_person_is_asked_is_unreachable() {
        let fake = Fake::default();
        fake.state().items.insert(
            1,
            Item {
                locked: true,
                ..Item::of("anilist", TOKEN.as_bytes())
            },
        );
        fake.state().person = Person::Away;
        let (store, serving) = fake.serve(DEADLINE, asking()).await;
        let asked = async {
            while !fake.state().methods().contains(&"Prompt") {
                tokio::task::yield_now().await;
            }
        };
        // Bounded, so that a read that never asks ends the test.
        let going = async {
            let asked = tokio::time::timeout(Duration::from_secs(5), asked).await;
            drop(serving);

            asked.is_ok()
        };

        let (read, asked) = tokio::join!(store.read(ANILIST), going);

        assert!(asked, "the person was never asked: {read:?}");
        assert!(matches!(read, Err(Error::Unreachable(_))), "{read:?}");
    }
}
