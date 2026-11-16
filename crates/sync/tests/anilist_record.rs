//! Records the fixtures of the AniList backend from the test account.
//!
//! Ignored, because it speaks to the service. It runs by hand, with the
//! test account's token in the environment, one recording at a time:
//!
//! ```text
//! BENSHI_TOKEN_ANILIST=... cargo test -p benshi-sync --test anilist_record -- --ignored record_the_list
//! BENSHI_TOKEN_ANILIST=... cargo test -p benshi-sync --test anilist_record -- --ignored record_the_contract
//! ```
//!
//! `list.json` comes of three works filed on the account's list, the
//! backend reading the list, and the three taken away again. `contract.json`
//! comes of a series filed on the list, the checks of the contract that ask
//! the service, and the series and the film the checks add taken away again.
//! Both need the list empty first, and two seconds pass between every
//! request. The account is the one made for tests: no user's own list is in
//! a fixture.

mod contract;
mod recorded;
mod subject;

use std::sync::{Arc, Mutex, PoisonError};

use benshi_core::list::EntryId;
use benshi_sync::ListBackend;
use benshi_sync::anilist::transport::{ANILIST, Http, Request, Transport};
use benshi_sync::anilist::{AniList, PACE};
use serde_json::{Value, json};

use recorded::Recording;

/// The works filed for the recording: a series set aside after its third
/// episode, an OVA being watched, and a film seen and scored.
const WORKS: [(i64, &str, u32, Option<u32>); 3] = [
    (16_498, "PAUSED", 3, None),
    (227, "CURRENT", 1, None),
    (112_151, "COMPLETED", 1, Some(80)),
];

/// A request that files a work on the list.
fn save(work: i64, status: &str, progress: u32, score: Option<u32>) -> Request {
    match score {
        None => Request {
            query: "mutation($mediaId: Int, $status: MediaListStatus, $progress: Int) { \
                    SaveMediaListEntry(mediaId: $mediaId, status: $status, progress: $progress) \
                    { id } }"
                .to_owned(),
            variables: json!({ "mediaId": work, "status": status, "progress": progress }),
        },
        Some(points) => Request {
            query: "mutation($mediaId: Int, $status: MediaListStatus, $progress: Int, \
                    $scoreRaw: Int) { SaveMediaListEntry(mediaId: $mediaId, status: $status, \
                    progress: $progress, scoreRaw: $scoreRaw) { id } }"
                .to_owned(),
            variables: json!({
                "mediaId": work, "status": status, "progress": progress, "scoreRaw": points
            }),
        },
    }
}

/// A request that takes an entry off the list.
fn delete(entry: i64) -> Request {
    Request {
        query: "mutation($id: Int) { DeleteMediaListEntry(id: $id) { deleted } }".to_owned(),
        variables: json!({ "id": entry }),
    }
}

/// The request that files the series of the contract: set aside after its
/// third episode, scored, and begun in 2015.
fn file_the_series() -> Request {
    Request {
        query: "mutation ($mediaId: Int, $status: MediaListStatus, $progress: Int, \
                $scoreRaw: Int, $startedAt: FuzzyDateInput) { SaveMediaListEntry(mediaId: \
                $mediaId, status: $status, progress: $progress, scoreRaw: $scoreRaw, \
                startedAt: $startedAt) { id } }"
            .to_owned(),
        variables: json!({
            "mediaId": subject::LISTED.0, "status": "PAUSED", "progress": 3, "scoreRaw": 80,
            "startedAt": { "year": 2015 }
        }),
    }
}

/// The request for the number of the account's entry for one work.
fn entry_for(work: EntryId) -> Request {
    Request {
        query: "query ($id: Int) { Media(id: $id) { mediaListEntry { id } } }".to_owned(),
        variables: json!({ "id": work.0 }),
    }
}

/// Send one request out of the backend's hands, in its turn.
async fn send(http: &Http, request: &Request) -> Value {
    tokio::time::sleep(PACE).await;
    let answer = http.send(request).await.expect("the service answers");
    assert_eq!(answer.status, 200, "{}", answer.body);

    serde_json::from_str(&answer.body).expect("the answer is JSON")
}

/// The token of the test account, from the environment.
fn token() -> String {
    std::env::var("BENSHI_TOKEN_ANILIST")
        .expect("BENSHI_TOKEN_ANILIST holds the test account's token")
        .trim()
        .to_owned()
}

#[tokio::test]
#[ignore = "speaks to AniList with the test account's token, from BENSHI_TOKEN_ANILIST"]
async fn record_the_contract_on_the_test_account() {
    let token = token();
    let http = Arc::new(Http::new(ANILIST, &token).expect("a client"));
    let reading = || {
        AniList::new(Recording {
            http: Arc::clone(&http),
            kept: Arc::default(),
        })
    };
    let before = reading().list().await.expect("the list is read");
    assert!(
        before.is_empty(),
        "the test account's list holds {} entries, and the recording needs it empty",
        before.len()
    );

    send(&http, &file_the_series()).await;
    tokio::time::sleep(PACE).await;
    let kept = Arc::new(Mutex::new(Vec::new()));
    let answering = subject::subject(Recording {
        http: Arc::clone(&http),
        kept: Arc::clone(&kept),
    });
    // The checks run as a task of their own: where one fails, the works are
    // taken away again before the failure is told.
    let checked = tokio::task::LocalSet::new()
        .run_until(async move {
            tokio::task::spawn_local(async move {
                contract::verify_answers(&answering).await;
            })
            .await
        })
        .await;

    for work in [subject::LISTED, subject::UNLISTED] {
        let held = send(&http, &entry_for(work)).await;
        if let Some(entry) = held["data"]["Media"]["mediaListEntry"]["id"].as_i64() {
            send(&http, &delete(entry)).await;
        }
    }
    tokio::time::sleep(PACE).await;
    let after = reading().list().await.expect("the list is read again");
    assert!(
        after.is_empty(),
        "the recording left {} entries on the test account's list",
        after.len()
    );
    if let Err(failed) = checked {
        std::panic::resume_unwind(failed.into_panic());
    }

    let exchanges = kept.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let text = serde_json::to_string_pretty(&exchanges).expect("the exchanges are JSON") + "\n";
    assert!(!text.contains(&token), "the token is in the fixture");
    std::fs::write(recorded::fixture("contract.json"), text).expect("the fixture is written");
}

#[tokio::test]
#[ignore = "speaks to AniList with the test account's token, from BENSHI_TOKEN_ANILIST"]
async fn record_the_list_of_the_test_account() {
    let token = token();
    let http = Arc::new(Http::new(ANILIST, &token).expect("a client"));

    let mut filed = Vec::new();
    for (work, status, progress, score) in WORKS {
        let saved = send(&http, &save(work, status, progress, score)).await;
        filed.push(
            saved["data"]["SaveMediaListEntry"]["id"]
                .as_i64()
                .expect("the entry has an id"),
        );
    }

    tokio::time::sleep(PACE).await;
    let kept = Arc::new(Mutex::new(Vec::new()));
    let anilist = AniList::new(Recording {
        http: Arc::clone(&http),
        kept: Arc::clone(&kept),
    });
    let read = anilist.list().await;
    drop(anilist);

    for entry in filed {
        send(&http, &delete(entry)).await;
    }

    let list = read.expect("the list is read");
    assert_eq!(list.len(), WORKS.len(), "{list:?}");
    let exchanges = kept.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let text = serde_json::to_string_pretty(&exchanges).expect("the exchanges are JSON") + "\n";
    assert!(!text.contains(&token), "the token is in the fixture");
    std::fs::write(recorded::fixture("list.json"), text).expect("the fixture is written");
}
