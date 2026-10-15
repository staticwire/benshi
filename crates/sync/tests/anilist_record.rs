//! Records the fixtures of the AniList backend from the test account.
//!
//! Ignored, because it speaks to the service. It runs by hand, with the
//! test account's token in the environment:
//!
//! ```text
//! BENSHI_TOKEN_ANILIST=... cargo test -p benshi-sync --test anilist_record -- --ignored
//! ```
//!
//! It files three works on the account's list, records the backend reading
//! the list, and takes the three away again, two seconds between every
//! request. The account is the one made for tests: no user's own list is in
//! a fixture.

mod recorded;

use std::sync::{Arc, Mutex, PoisonError};

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

/// Send one request out of the backend's hands, in its turn.
async fn send(http: &Http, request: &Request) -> Value {
    tokio::time::sleep(PACE).await;
    let answer = http.send(request).await.expect("the service answers");
    assert_eq!(answer.status, 200, "{}", answer.body);

    serde_json::from_str(&answer.body).expect("the answer is JSON")
}

#[tokio::test]
#[ignore = "speaks to AniList with the test account's token, from BENSHI_TOKEN_ANILIST"]
async fn record_the_list_of_the_test_account() {
    let token = std::env::var("BENSHI_TOKEN_ANILIST")
        .expect("BENSHI_TOKEN_ANILIST holds the test account's token");
    let http = Http::new(ANILIST, token.trim()).expect("a client");

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
        http: &http,
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
    assert!(!text.contains(token.trim()), "the token is in the fixture");
    std::fs::write(recorded::fixture("list.json"), text).expect("the fixture is written");
}
