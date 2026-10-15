//! AniList: an account's list over its GraphQL API.

mod query;
pub mod transport;

use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use benshi_core::list::{Entry, EntryId, Format, Rating, Status};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{Error, within};
use query::{Envelope, List, ListData, ListEntry, ListStatus, Title, ViewerData};
use transport::{Answer, Request, Transport};

/// How long the service is given to answer one request.
pub const DEADLINE: Duration = Duration::from_secs(15);

/// How far apart two requests are sent.
pub const PACE: Duration = Duration::from_secs(2);

/// The wait a refusal for too many requests is read as where it does not
/// say: the one-minute timeout AniList's documentation gives for going past
/// its rate limit.
pub const WAIT: Duration = Duration::from_secs(60);

/// How far past the wait a refusal for too many requests asks for the next
/// request is sent, as the user's importer of lists waits.
const MARGIN: Duration = Duration::from_secs(1);

/// The account a token is of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// AniList's number for the user.
    pub id: i64,
    /// The user's name.
    pub name: String,
}

/// An account's list on AniList.
#[derive(Debug)]
pub struct AniList<T> {
    transport: T,
    next: Mutex<Option<Instant>>,
}

impl<T: Transport + Sync> AniList<T> {
    /// A backend that sends its requests through `transport`.
    #[must_use]
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next: Mutex::new(None),
        }
    }

    /// The account the token is of.
    ///
    /// # Errors
    ///
    /// An [`Error`] of the class the failure of the service is.
    pub async fn account(&self) -> Result<Account, Error> {
        let data: ViewerData = self.ask(query::VIEWER, json!({})).await?;

        Ok(Account {
            id: data.viewer.id,
            name: data.viewer.name,
        })
    }

    /// Every entry of the account's list of anime.
    ///
    /// The list is asked for the account the token is of, a chunk at a time.
    /// A work on several of the account's lists is read once, from the list
    /// of its status where it is on one, and else from a list of the
    /// account's own making.
    ///
    /// # Errors
    ///
    /// An [`Error`] of the class the failure of the service is, and
    /// [`Error::Unreadable`] for an entry with no title, a score past a
    /// hundred, or a chunk that holds no entry and says more follow.
    pub async fn list(&self) -> Result<Vec<Entry>, Error> {
        let account = self.account().await?;
        let mut entries = Vec::new();
        let mut read = HashSet::new();
        for chunk in 1_u32.. {
            let data: ListData = self
                .ask(query::LIST, json!({ "user": account.id, "chunk": chunk }))
                .await?;
            let held: usize = data
                .collection
                .lists
                .iter()
                .map(|list| list.entries.len())
                .sum();
            if held == 0 && data.collection.has_next_chunk {
                return Err(Error::Unreadable(
                    format!("chunk {chunk} holds no entry and says more follow").into(),
                ));
            }
            let (of_status, own): (Vec<List>, Vec<List>) = data
                .collection
                .lists
                .into_iter()
                .partition(|list| !list.is_custom_list);
            for list in of_status.into_iter().chain(own) {
                for entry in list.entries {
                    if read.insert(entry.media_id) {
                        entries.push(entry_of(entry)?);
                    }
                }
            }
            if !data.collection.has_next_chunk {
                break;
            }
        }

        Ok(entries)
    }

    /// Send one request in its turn, and read its answer.
    ///
    /// A refusal for too many requests is answered as a wait and not waited
    /// out here, and no request is sent again before the wait and a second
    /// past it have passed.
    async fn ask<D: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<D, Error> {
        tokio::time::sleep_until(self.turn()).await;
        let request = Request {
            query: query.to_owned(),
            variables,
        };
        let answer = within(DEADLINE, self.transport.send(&request)).await?;
        if answer.status == 429 {
            self.hold_off(answer.retry_after.unwrap_or(WAIT) + MARGIN);
        }

        read(&answer)
    }

    /// Send no request before this long has passed.
    fn hold_off(&self, wait: Duration) {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let until = Instant::now() + wait;
        *next = Some(next.map_or(until, |next| next.max(until)));
    }

    /// When the next request may be sent, with the turn after it kept for
    /// the request after it, so that requests made at once go out in turn.
    fn turn(&self) -> Instant {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        let turn = next.map_or(now, |next| next.max(now));
        *next = Some(turn + PACE);

        turn
    }
}

/// What an answer holds, or the failure it is.
///
/// A refusal for too many requests is a wait, and a status of 500 or above
/// is a failure on the side of the service. Any other answer is read: an
/// error it carries is a refusal in the service's words, and its data is what
/// was asked for.
fn read<D: DeserializeOwned>(answer: &Answer) -> Result<D, Error> {
    let status = answer.status;
    if status == 429 {
        return Err(Error::Wait {
            after: answer.retry_after.unwrap_or(WAIT),
        });
    }
    if status >= 500 {
        return Err(Error::Unreachable(format!("HTTP {status}").into()));
    }
    let unreadable =
        |cause: &dyn std::fmt::Display| Error::Unreadable(format!("HTTP {status}: {cause}").into());
    let envelope: Envelope =
        serde_json::from_str(&answer.body).map_err(|cause| unreadable(&cause))?;
    if let Some(problem) = envelope.errors.into_iter().next() {
        return Err(Error::Refused(problem.message));
    }
    let data = envelope
        .data
        .ok_or_else(|| unreadable(&"no data and no error"))?;

    serde_json::from_value(data).map_err(|cause| unreadable(&cause))
}

/// An entry as the list reads it.
///
/// The title is the one the account prefers, and else the first of the
/// romaji, the English and the native; the spellings are the others and the
/// synonyms, each once. A work is released unless the service says it is
/// not yet.
fn entry_of(entry: ListEntry) -> Result<Entry, Error> {
    let id = EntryId(entry.media_id);
    let unreadable = |what: String| Error::Unreadable(format!("work {} {what}", id.0).into());
    let media = entry.media;
    let Title {
        user_preferred,
        romaji,
        english,
        native,
    } = media.title;
    let title = user_preferred
        .or_else(|| romaji.clone())
        .or_else(|| english.clone())
        .or_else(|| native.clone())
        .ok_or_else(|| unreadable("has no title".to_owned()))?;
    let mut spellings: Vec<String> = Vec::new();
    for spelling in [romaji, english, native]
        .into_iter()
        .flatten()
        .chain(media.synonyms.unwrap_or_default())
    {
        if spelling != title && !spellings.contains(&spelling) {
            spellings.push(spelling);
        }
    }
    let score = match entry.score {
        None | Some(0) => None,
        Some(points) => Some(
            Rating::new(points)
                .ok_or_else(|| unreadable(format!("has a score of {points}, past a hundred")))?,
        ),
    };

    Ok(Entry {
        id,
        title,
        spellings,
        format: media.format.as_deref().and_then(format_of),
        episodes: media.episodes,
        episode_length: media
            .duration
            .map(|minutes| Duration::from_mins(u64::from(minutes))),
        year: media
            .season_year
            .or(media.start_date.and_then(|date| date.year)),
        released: media.status.as_deref() != Some("NOT_YET_RELEASED"),
        status: status_of(entry.status),
        progress: entry.progress.unwrap_or(0),
        rewatches: entry.repeat.unwrap_or(0),
        score,
    })
}

/// The format AniList names, and none for one this build does not know.
fn format_of(said: &str) -> Option<Format> {
    match said {
        "TV" => Some(Format::Tv),
        "TV_SHORT" => Some(Format::TvShort),
        "MOVIE" => Some(Format::Film),
        "SPECIAL" => Some(Format::Special),
        "OVA" => Some(Format::Ova),
        "ONA" => Some(Format::Ona),
        "MUSIC" => Some(Format::Music),
        _ => None,
    }
}

/// Where the account stands, as AniList's status says it.
fn status_of(said: ListStatus) -> Status {
    match said {
        ListStatus::Current => Status::Watching,
        ListStatus::Planning => Status::Planned,
        ListStatus::Completed => Status::Completed,
        ListStatus::Dropped => Status::Dropped,
        ListStatus::Paused => Status::OnHold,
        ListStatus::Repeating => Status::Rewatching,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;

    use benshi_core::list::{Entry, EntryId, Format, Rating, Status};
    use serde_json::{Value, json};
    use tokio::time::Instant;

    use super::query::ViewerData;
    use super::transport::{Answer, Request, Transport};
    use super::{Account, AniList, DEADLINE, PACE, WAIT, read};
    use crate::Error;

    /// A service that answers what it was given to answer, in order, and
    /// keeps what it was sent and when.
    #[derive(Default)]
    struct Script {
        /// Each answer in turn, and none for a request never answered.
        answers: Mutex<VecDeque<Option<Answer>>>,
        sent: Mutex<Vec<(Instant, Request)>>,
    }

    impl Script {
        fn answering(answers: impl IntoIterator<Item = Option<Answer>>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().collect()),
                sent: Mutex::default(),
            }
        }

        fn sent(&self) -> Vec<(Instant, Request)> {
            self.sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Transport for Script {
        async fn send(&self, request: &Request) -> Result<Answer, Error> {
            self.sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((Instant::now(), request.clone()));
            let answer = self
                .answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .expect("the script has an answer for every request");
            match answer {
                Some(answer) => Ok(answer),
                None => std::future::pending().await,
            }
        }
    }

    fn an_answer(status: u16, body: &Value) -> Answer {
        Answer {
            status,
            retry_after: None,
            body: body.to_string(),
        }
    }

    /// The test account, as `Viewer` answered for it.
    fn the_viewer() -> Answer {
        an_answer(
            200,
            &json!({ "data": { "Viewer": { "id": 8_344_475, "name": "bentest" } } }),
        )
    }

    /// A list of one chunk holding these entries.
    fn a_list(entries: &[Value]) -> Answer {
        a_chunk(entries, false)
    }

    fn a_chunk(entries: &[Value], more: bool) -> Answer {
        an_answer(
            200,
            &json!({ "data": { "MediaListCollection": {
                "hasNextChunk": more,
                "lists": [{ "isCustomList": false, "entries": entries }],
            } } }),
        )
    }

    /// An entry of a series of twelve, as AniList answers one.
    fn a_series_entry() -> Value {
        json!({
            "mediaId": 16_498,
            "status": "PAUSED",
            "progress": 3,
            "repeat": 1,
            "score": 80,
            "media": {
                "format": "TV",
                "episodes": 12,
                "duration": 24,
                "status": "FINISHED",
                "seasonYear": 2009,
                "startDate": { "year": 2009 },
                "title": {
                    "userPreferred": "Show Title",
                    "romaji": "Show Title",
                    "english": "The Show Title",
                    "native": "Show Native Title"
                },
                "synonyms": ["Show Title Spelt Otherwise", "The Show Title"]
            }
        })
    }

    /// The entry of the series, as the list reads it.
    fn the_series() -> Entry {
        Entry {
            id: EntryId(16_498),
            title: "Show Title".to_owned(),
            spellings: vec![
                "The Show Title".to_owned(),
                "Show Native Title".to_owned(),
                "Show Title Spelt Otherwise".to_owned(),
            ],
            format: Some(Format::Tv),
            episodes: Some(12),
            episode_length: Some(Duration::from_mins(24)),
            year: Some(2009),
            released: true,
            status: Status::OnHold,
            progress: 3,
            rewatches: 1,
            score: Rating::new(80),
        }
    }

    /// The series' entry with one field of its media replaced.
    fn with_media(field: &str, value: Value) -> Value {
        let mut entry = a_series_entry();
        entry["media"][field] = value;
        entry
    }

    async fn the_list(
        answers: impl IntoIterator<Item = Option<Answer>>,
    ) -> Result<Vec<Entry>, Error> {
        AniList::new(Script::answering(
            std::iter::once(Some(the_viewer())).chain(answers),
        ))
        .list()
        .await
    }

    /// The one entry a list of one answers.
    async fn the_one_entry(entry: Value) -> Entry {
        let list = the_list([Some(a_list(&[entry]))])
            .await
            .expect("the list is read");
        let [entry] = list.as_slice() else {
            panic!("one entry was answered, and {list:?} was read")
        };
        entry.clone()
    }

    #[test]
    fn a_refusal_for_too_many_requests_is_the_wait_the_service_asked_for() {
        let answer = Answer {
            retry_after: Some(Duration::from_secs(30)),
            ..an_answer(
                429,
                &json!({ "errors": [{ "message": "Too Many Requests." }] }),
            )
        };

        let read = read::<ViewerData>(&answer);

        assert!(
            matches!(read, Err(Error::Wait { after }) if after == Duration::from_secs(30)),
            "got {read:?}"
        );
    }

    #[test]
    fn a_refusal_for_too_many_requests_that_names_no_wait_is_a_minute() {
        let read = read::<ViewerData>(&an_answer(429, &json!({})));

        assert!(
            matches!(read, Err(Error::Wait { after }) if after == WAIT),
            "got {read:?}"
        );
        assert_eq!(WAIT, Duration::from_mins(1));
    }

    #[test]
    fn a_failure_on_the_side_of_the_service_is_out_of_reach() {
        for status in [500, 502, 503, 599] {
            let read = read::<ViewerData>(&an_answer(status, &json!({})));

            assert!(
                matches!(&read, Err(Error::Unreachable(cause)) if cause.to_string() == format!("HTTP {status}")),
                "{status} got {read:?}"
            );
        }
    }

    #[test]
    fn a_token_the_service_does_not_take_is_refused_in_its_words() {
        // The answer the service gave a token that is not one.
        let answer = an_answer(
            400,
            &json!({ "data": null, "errors": [{ "message": "Invalid token", "status": 400 }] }),
        );

        let read = read::<ViewerData>(&answer);

        assert!(
            matches!(&read, Err(Error::Refused(said)) if said == "Invalid token"),
            "got {read:?}"
        );
    }

    #[test]
    fn a_request_with_no_token_is_refused_in_its_words() {
        // The answer the service gave `Viewer` with no token.
        let answer = an_answer(
            401,
            &json!({
                "errors": [{ "message": "Unauthorized.", "status": 401,
                             "locations": [{ "line": 1, "column": 9 }] }],
                "data": { "Viewer": null }
            }),
        );

        let read = read::<ViewerData>(&answer);

        assert!(
            matches!(&read, Err(Error::Refused(said)) if said == "Unauthorized."),
            "got {read:?}"
        );
    }

    #[test]
    fn an_error_in_an_answer_that_succeeded_is_a_refusal() {
        // AniList's documentation: an error can arrive with HTTP 200.
        let answer = an_answer(
            200,
            &json!({ "data": null, "errors": [{ "message": "Something went wrong." }] }),
        );

        let read = read::<ViewerData>(&answer);

        assert!(
            matches!(&read, Err(Error::Refused(said)) if said == "Something went wrong."),
            "got {read:?}"
        );
    }

    #[test]
    fn an_answer_that_is_not_json_is_unreadable_and_says_its_status() {
        let answer = Answer {
            status: 404,
            retry_after: None,
            body: "<html>Not Found</html>".to_owned(),
        };

        let read = read::<ViewerData>(&answer);

        assert!(
            matches!(&read, Err(Error::Unreadable(cause)) if cause.to_string().starts_with("HTTP 404: ")),
            "got {read:?}"
        );
    }

    #[test]
    fn an_answer_with_neither_data_nor_errors_is_unreadable() {
        let read = read::<ViewerData>(&an_answer(200, &json!({ "data": null })));

        assert!(
            matches!(&read, Err(Error::Unreadable(cause)) if cause.to_string() == "HTTP 200: no data and no error"),
            "got {read:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_account_is_the_one_the_token_is_of() {
        let anilist = AniList::new(Script::answering([Some(the_viewer())]));

        let account = anilist.account().await;

        assert_eq!(
            account.expect("the account is read"),
            Account {
                id: 8_344_475,
                name: "bentest".to_owned()
            }
        );
        let sent = anilist.transport.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1.query, super::query::VIEWER);
    }

    #[tokio::test(start_paused = true)]
    async fn a_first_request_is_sent_at_once() {
        let started = Instant::now();
        let anilist = AniList::new(Script::answering([Some(the_viewer())]));

        anilist.account().await.expect("the account is read");

        assert_eq!(anilist.transport.sent()[0].0, started);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_one_after_another_are_sent_two_seconds_apart() {
        let anilist = AniList::new(Script::answering([Some(the_viewer()), Some(the_viewer())]));

        anilist.account().await.expect("the account is read");
        anilist.account().await.expect("the account is read again");

        let sent = anilist.transport.sent();
        assert_eq!(sent[1].0 - sent[0].0, PACE);
        assert_eq!(PACE, Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn requests_made_at_once_are_sent_two_seconds_apart() {
        let anilist = AniList::new(Script::answering([
            Some(the_viewer()),
            Some(the_viewer()),
            Some(the_viewer()),
        ]));

        let (first, second, third) =
            tokio::join!(anilist.account(), anilist.account(), anilist.account());

        for answer in [first, second, third] {
            answer.expect("the account is read");
        }
        let mut times: Vec<Instant> = anilist.transport.sent().iter().map(|(at, _)| *at).collect();
        times.sort_unstable();
        assert_eq!(times[1] - times[0], PACE);
        assert_eq!(times[2] - times[1], PACE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_after_a_quiet_spell_is_sent_at_once_and_the_pace_kept_after_it() {
        // The third request is what tells the pace kept from the pace
        // forgotten: a turn in the past is a turn at once either way.
        let anilist = AniList::new(Script::answering([
            Some(the_viewer()),
            Some(the_viewer()),
            Some(the_viewer()),
        ]));
        anilist.account().await.expect("the account is read");
        tokio::time::sleep(Duration::from_secs(10)).await;
        let asked = Instant::now();

        anilist.account().await.expect("the account is read again");
        anilist
            .account()
            .await
            .expect("the account is read a third time");

        let sent = anilist.transport.sent();
        assert_eq!(sent[1].0, asked);
        assert_eq!(sent[2].0 - sent[1].0, PACE);
    }

    /// A refusal for too many requests that asks for this long a wait.
    fn too_many(after: u64) -> Answer {
        Answer {
            retry_after: Some(Duration::from_secs(after)),
            ..an_answer(
                429,
                &json!({ "errors": [{ "message": "Too Many Requests." }] }),
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_for_too_many_requests_is_answered_and_not_waited_out() {
        // Whoever made the call decides when to make it again.
        let anilist = AniList::new(Script::answering([Some(too_many(30))]));
        let started = Instant::now();

        let account = anilist.account().await;

        assert!(
            matches!(account, Err(Error::Wait { after }) if after == Duration::from_secs(30)),
            "got {account:?}"
        );
        assert_eq!(anilist.transport.sent().len(), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn the_request_after_a_refusal_for_too_many_waits_a_second_past_the_wait() {
        let anilist = AniList::new(Script::answering([Some(too_many(30)), Some(the_viewer())]));
        anilist
            .account()
            .await
            .expect_err("the service asks for a wait");

        anilist.account().await.expect("the account is read");

        let sent = anilist.transport.sent();
        assert_eq!(sent[1].0 - sent[0].0, Duration::from_secs(31));
    }

    #[tokio::test(start_paused = true)]
    async fn a_service_that_never_answers_is_a_timeout_of_fifteen_seconds() {
        let anilist = AniList::new(Script::answering([None]));
        let started = Instant::now();

        // Bounded at twice the deadline, so that a request with none fails the
        // test rather than leaving a paused clock nothing to move to.
        let account = tokio::time::timeout(DEADLINE * 2, anilist.account())
            .await
            .expect("the request ends within twice its deadline");

        assert!(
            matches!(account, Err(Error::Timeout { deadline }) if deadline == DEADLINE),
            "got {account:?}"
        );
        assert_eq!(started.elapsed(), DEADLINE);
        assert_eq!(DEADLINE, Duration::from_secs(15));
    }

    #[tokio::test(start_paused = true)]
    async fn the_list_is_read_for_the_account_the_token_is_of() {
        let script = Script::answering([Some(the_viewer()), Some(a_list(&[a_series_entry()]))]);
        let anilist = AniList::new(script);

        let list = anilist.list().await;

        assert_eq!(list.expect("the list is read"), [the_series()]);
        let sent = anilist.transport.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1.query, super::query::VIEWER);
        assert_eq!(sent[1].1.query, super::query::LIST);
        assert_eq!(
            sent[1].1.variables,
            json!({ "user": 8_344_475, "chunk": 1 })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_list_in_chunks_is_read_whole() {
        let film = json!({
            "mediaId": 112_151, "status": "COMPLETED", "progress": 1, "repeat": 0, "score": 0,
            "media": { "format": "MOVIE", "episodes": 1, "duration": 95, "status": "FINISHED",
                       "seasonYear": 1988, "startDate": { "year": 1988 },
                       "title": { "userPreferred": "A Film", "romaji": "A Film",
                                  "english": null, "native": null },
                       "synonyms": [] }
        });
        let script = Script::answering([
            Some(the_viewer()),
            Some(a_chunk(&[a_series_entry()], true)),
            Some(a_chunk(&[film], false)),
        ]);
        let anilist = AniList::new(script);

        let list = anilist.list().await.expect("the list is read");

        let ids: Vec<EntryId> = list.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, [EntryId(16_498), EntryId(112_151)]);
        let sent = anilist.transport.sent();
        assert_eq!(
            sent[2].1.variables,
            json!({ "user": 8_344_475, "chunk": 2 })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_chunk_that_holds_nothing_and_says_more_follow_is_unreadable() {
        // Followed, such a chunk would be asked for again and again for as
        // long as the service said so, and the reading would never end.
        let script = Script::answering([
            Some(the_viewer()),
            Some(a_chunk(&[a_series_entry()], true)),
            Some(a_chunk(&[], true)),
        ]);
        let anilist = AniList::new(script);

        let list = anilist.list().await;

        assert!(
            matches!(&list, Err(Error::Unreadable(cause)) if cause.to_string() == "chunk 2 holds no entry and says more follow"),
            "got {list:?}"
        );
        assert_eq!(anilist.transport.sent().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_is_read_with_every_fact_the_service_gives() {
        assert_eq!(the_one_entry(a_series_entry()).await, the_series());
    }

    #[tokio::test(start_paused = true)]
    async fn what_the_service_does_not_give_is_none() {
        let entry = json!({
            "mediaId": 9, "status": "PLANNING", "progress": null, "repeat": null, "score": null,
            "media": { "format": null, "episodes": null, "duration": null,
                       "status": "NOT_YET_RELEASED", "seasonYear": null, "startDate": null,
                       "title": { "userPreferred": "A Work To Come", "romaji": null,
                                  "english": null, "native": null },
                       "synonyms": null }
        });

        assert_eq!(
            the_one_entry(entry).await,
            Entry {
                id: EntryId(9),
                title: "A Work To Come".to_owned(),
                spellings: Vec::new(),
                format: None,
                episodes: None,
                episode_length: None,
                year: None,
                released: false,
                status: Status::Planned,
                progress: 0,
                rewatches: 0,
                score: None,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_status_is_read_as_its_own() {
        let every = [
            ("CURRENT", Status::Watching),
            ("PLANNING", Status::Planned),
            ("COMPLETED", Status::Completed),
            ("DROPPED", Status::Dropped),
            ("PAUSED", Status::OnHold),
            ("REPEATING", Status::Rewatching),
        ];

        for (said, status) in every {
            let mut entry = a_series_entry();
            entry["status"] = json!(said);

            assert_eq!(the_one_entry(entry).await.status, status, "{said}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_this_build_does_not_know_is_unreadable() {
        let mut entry = a_series_entry();
        entry["status"] = json!("BINGING");

        let list = the_list([Some(a_list(&[entry]))]).await;

        assert!(matches!(list, Err(Error::Unreadable(_))), "got {list:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn each_format_is_read_as_its_own_and_one_this_build_does_not_know_as_none() {
        let every = [
            ("TV", Some(Format::Tv)),
            ("TV_SHORT", Some(Format::TvShort)),
            ("MOVIE", Some(Format::Film)),
            ("SPECIAL", Some(Format::Special)),
            ("OVA", Some(Format::Ova)),
            ("ONA", Some(Format::Ona)),
            ("MUSIC", Some(Format::Music)),
            ("MANGA", None),
        ];

        for (said, format) in every {
            let entry = with_media("format", json!(said));

            assert_eq!(the_one_entry(entry).await.format, format, "{said}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_work_on_a_list_of_the_accounts_own_making_is_listed_once() {
        let answer = an_answer(
            200,
            &json!({ "data": { "MediaListCollection": {
                "hasNextChunk": false,
                "lists": [
                    { "isCustomList": true, "entries": [a_series_entry()] },
                    { "isCustomList": false, "entries": [a_series_entry()] },
                ],
            } } }),
        );

        let list = the_list([Some(answer)]).await;

        assert_eq!(list.expect("the list is read"), [the_series()]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_work_on_a_list_of_the_accounts_own_making_alone_is_listed() {
        let answer = an_answer(
            200,
            &json!({ "data": { "MediaListCollection": {
                "hasNextChunk": false,
                "lists": [{ "isCustomList": true, "entries": [a_series_entry()] }],
            } } }),
        );

        let list = the_list([Some(answer)]).await;

        assert_eq!(list.expect("the list is read"), [the_series()]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_year_is_the_seasons_and_else_the_year_the_work_began() {
        let mut entry = with_media("seasonYear", Value::Null);
        entry["media"]["startDate"] = json!({ "year": 2008 });

        assert_eq!(the_one_entry(entry).await.year, Some(2008));
    }

    #[tokio::test(start_paused = true)]
    async fn the_title_is_the_one_the_account_prefers_and_else_the_romaji() {
        let mut entry = a_series_entry();
        entry["media"]["title"]["userPreferred"] = Value::Null;
        entry["media"]["title"]["romaji"] = json!("Show Title Romaji");

        let read = the_one_entry(entry).await;

        assert_eq!(read.title, "Show Title Romaji");
        assert!(
            !read.spellings.contains(&read.title),
            "the title is among its own spellings: {read:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_work_with_no_title_at_all_is_unreadable() {
        let entry = with_media(
            "title",
            json!({ "userPreferred": null, "romaji": null, "english": null, "native": null }),
        );

        let list = the_list([Some(a_list(&[entry]))]).await;

        assert!(
            matches!(&list, Err(Error::Unreadable(cause)) if cause.to_string() == "work 16498 has no title"),
            "got {list:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_score_of_nought_is_no_score() {
        let mut entry = a_series_entry();
        entry["score"] = json!(0);

        assert_eq!(the_one_entry(entry).await.score, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_score_past_a_hundred_is_unreadable() {
        let mut entry = a_series_entry();
        entry["score"] = json!(101);

        let list = the_list([Some(a_list(&[entry]))]).await;

        assert!(
            matches!(&list, Err(Error::Unreadable(cause)) if cause.to_string() == "work 16498 has a score of 101, past a hundred"),
            "got {list:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_work_is_released_unless_the_service_says_not_yet() {
        for status in ["FINISHED", "RELEASING", "HIATUS", "CANCELLED"] {
            let entry = with_media("status", json!(status));

            assert!(the_one_entry(entry).await.released, "{status}");
        }
    }
}
