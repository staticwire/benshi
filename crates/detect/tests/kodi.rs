//! The Kodi adapter, against a Kodi that answers from a script.
//!
//! The answers are those Kodi 21.3 gave at JSON-RPC 13.5 while it played a
//! generated file, with the path of the file replaced.

// The adapter under test exists only on Linux, so on any other target this
// binary has nothing to compile rather than nothing to run.
#![cfg(target_os = "linux")]

mod contract;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use benshi_core::clock::{Clock, SystemClock, TestClock};
use benshi_core::path::RawPath;
use benshi_core::policy::{Policy, PolicyTable};
use benshi_core::{Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use benshi_detect::kodi::KodiWatcher;
use benshi_detect::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

/// The deadline a watcher here is given, where a test needs no other.
const DEADLINE: Duration = Duration::from_millis(500);

/// The file the scripted Kodi plays.
const FILE: &str = "/media/anime/[Group] Show - 03 (1080p).mkv";

/// What Kodi 21.3 answered `Player.GetActivePlayers` while it played a file.
const ACTIVE_VIDEO: &str = r#"[{"playerid":1,"playertype":"internal","type":"video"}]"#;

/// What it answered `Player.GetItem` for that player.
const ITEM: &str = r#"{"item":{"file":"/media/anime/[Group] Show - 03 (1080p).mkv","label":"[Group] Show - 03 (1080p).mkv","type":"unknown"}}"#;

/// What it answered `Player.GetProperties` for that player while it played.
const PLAYING: &str = r#"{"speed":1,"time":{"hours":0,"milliseconds":528,"minutes":0,"seconds":3},"totaltime":{"hours":0,"milliseconds":2,"minutes":10,"seconds":0}}"#;

/// And while it was paused.
const PAUSED: &str = r#"{"speed":0,"time":{"hours":0,"milliseconds":603,"minutes":0,"seconds":6},"totaltime":{"hours":0,"milliseconds":2,"minutes":10,"seconds":0}}"#;

/// What it sent down an open connection as the file paused.
const NOTIFICATION: &str = r#"{"jsonrpc":"2.0","method":"Player.OnPause","params":{"data":{"item":{"title":"[Group] Show - 03 (1080p).mkv","type":"movie"},"player":{"playerid":1,"speed":0}},"sender":"xbmc"}}"#;

/// What a scripted Kodi does with one request: the pieces it sends back, one
/// after another and a moment apart, or `None` to close the connection.
type Script = dyn Fn(&Value) -> Option<Vec<String>> + Send + Sync;

/// Kodi's answer to `request`, carrying `result`.
fn answer(request: &Value, result: &str) -> String {
    format!(
        r#"{{"id":{},"jsonrpc":"2.0","result":{result}}}"#,
        request["id"]
    )
}

/// Kodi's answer to a call whose parameters it refuses, as Kodi 21.3 refused
/// a player id given as a string.
fn a_fault(request: &Value) -> String {
    format!(
        r#"{{"error":{{"code":-32602,"data":{{"method":{},"stack":{{"message":"Invalid type string received","name":"playerid","type":"integer"}}}},"message":"Invalid params."}},"id":{},"jsonrpc":"2.0"}}"#,
        request["method"], request["id"]
    )
}

/// The method a request calls.
fn method(request: &Value) -> &str {
    request["method"].as_str().unwrap_or_default()
}

/// A Kodi playing [`FILE`], answering each call as Kodi 21.3 did.
fn playing(request: &Value) -> Option<Vec<String>> {
    let result = match method(request) {
        "Player.GetActivePlayers" => ACTIVE_VIDEO,
        "Player.GetItem" => ITEM,
        "Player.GetProperties" => PLAYING,
        _ => return None,
    };

    Some(vec![answer(request, result)])
}

/// How many ports the tests of this binary have been handed.
///
/// Each port between 20000 and 40000 is handed out once, so a port one test
/// lets go of is never bound by another while the first still connects to it.
static HANDED_OUT: AtomicU32 = AtomicU32::new(0);

/// A port between 20000 and 40000 that no other test here was handed.
fn a_port() -> u16 {
    let offset = HANDED_OUT.fetch_add(1, Ordering::SeqCst);
    let port = 20_000 + (std::process::id() % 20_000 + offset) % 20_000;

    u16::try_from(port).expect("below 40000")
}

/// A listener on a free port between 20000 and 40000.
async fn a_listener() -> TcpListener {
    for _tried in 0..20_000 {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", a_port())).await {
            return listener;
        }
    }
    panic!("no port between 20000 and 40000 is free");
}

/// An address between 20000 and 40000 held by a socket that listens for
/// nothing, so that a connection to it is refused while the socket is kept.
fn a_closed_address() -> (TcpSocket, SocketAddr) {
    for _tried in 0..20_000 {
        let address = SocketAddr::from(([127, 0, 0, 1], a_port()));
        let socket = TcpSocket::new_v4().expect("a socket");
        if socket.bind(address).is_ok() {
            return (socket, address);
        }
    }
    panic!("no port between 20000 and 40000 is free");
}

/// A scripted Kodi, serving on its own task.
struct Kodi {
    address: SocketAddr,
    /// How many connections it has taken.
    taken: Arc<AtomicUsize>,
    /// The task that takes connections, and serves each on a task of its own.
    serving: JoinHandle<()>,
}

impl Kodi {
    /// How many connections it has taken.
    fn connections(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }

    /// Close the listener and every connection, as a Kodi that quits does.
    async fn quit(self) {
        self.serving.abort();
        // The task serves until it is aborted, so it ends with an error, and
        // once it has, the listener and every connection are closed.
        assert!(
            self.serving.await.is_err(),
            "the scripted Kodi ended by itself"
        );
    }
}

/// Serve `script` on `listener`.
fn serve_on(
    listener: TcpListener,
    script: impl Fn(&Value) -> Option<Vec<String>> + Send + Sync + 'static,
) -> Kodi {
    let address = listener
        .local_addr()
        .expect("a bound listener has an address");
    let script: Arc<Script> = Arc::new(script);
    let taken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&taken);

    let serving = tokio::spawn(async move {
        // Held by this task, so that aborting it aborts every connection.
        let mut connections = JoinSet::new();
        while let Ok((connection, _peer)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            connections.spawn(serve(connection, Arc::clone(&script)));
        }
    });

    Kodi {
        address,
        taken,
        serving,
    }
}

/// A Kodi on a free port that does with each request what `script` says.
async fn a_kodi(
    script: impl Fn(&Value) -> Option<Vec<String>> + Send + Sync + 'static,
) -> SocketAddr {
    serve_on(a_listener().await, script).address
}

/// Answer the requests of one connection until either side closes it.
async fn serve(mut connection: TcpStream, script: Arc<Script>) {
    let mut pending = Vec::new();
    let mut chunk = [0; 4096];

    loop {
        while let Some(request) = take_value(&mut pending) {
            let Some(pieces) = script(&request) else {
                return;
            };
            for piece in pieces {
                if connection.write_all(piece.as_bytes()).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        match connection.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => pending.extend_from_slice(&chunk[..read]),
        }
    }
}

/// The first whole JSON value in `pending`, taken off it.
fn take_value(pending: &mut Vec<u8>) -> Option<Value> {
    let mut values = serde_json::Deserializer::from_slice(pending).into_iter::<Value>();
    let value = values.next()?.ok()?;
    let used = values.byte_offset();
    pending.drain(..used);

    Some(value)
}

/// Wait for `call`, failing the test where it outlasts ten seconds.
async fn bounded<T>(call: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("the call ends")
}

/// One round of a watcher of the Kodi at `address`.
async fn a_round_of(address: SocketAddr) -> PollOutcome {
    let mut watcher = KodiWatcher::new(address, SystemClock::new(), DEADLINE);

    bounded(watcher.poll()).await.expect("a round")
}

/// The one source a round described.
fn the_source(outcome: &PollOutcome) -> &SourceInfo {
    let [source] = outcome.sources.as_slice() else {
        panic!("one source in {outcome:?}");
    };
    source
}

/// The one reading a round took.
fn the_reading(outcome: &PollOutcome) -> &PlayerSnapshot {
    let [reading] = outcome.snapshots.as_slice() else {
        panic!("one reading in {outcome:?}");
    };
    reading
}

/// The one failure of a round that found nothing else.
fn the_failure(outcome: PollOutcome) -> (PlayerId, WatchError) {
    assert!(
        outcome.sources.is_empty() && outcome.snapshots.is_empty(),
        "{outcome:?}"
    );
    let mut failures = outcome.failures;
    assert_eq!(failures.len(), 1, "{failures:?}");

    failures.remove(0)
}

#[tokio::test]
async fn a_kodi_playing_a_file_is_read_as_its_video_player() {
    let kodi = a_kodi(playing).await;

    let outcome = a_round_of(kodi).await;

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    let source = the_source(&outcome);
    assert_eq!(source.player, PlayerId(kodi.to_string()));
    assert_eq!(source.app.0, "kodi");
    assert_eq!(
        source.capabilities,
        Capabilities {
            position: true,
            duration: true,
            paused: true,
            location: true,
        }
    );
    assert_eq!(source.state, PlayState::Playing);
    let reading = the_reading(&outcome);
    assert_eq!(reading.player, source.player);
    assert_eq!(
        reading.media,
        MediaRef::LocalFile(RawPath::from_bytes(FILE.as_bytes().to_vec()))
    );
    assert_eq!(reading.state, PlayState::Playing);
    assert_eq!(reading.position, Known::Value(Duration::from_millis(3_528)));
    assert_eq!(
        reading.duration,
        Known::Value(Duration::from_millis(600_002))
    );
}

#[tokio::test]
async fn a_paused_file_is_read_as_paused() {
    let kodi = a_kodi(|request| {
        if method(request) == "Player.GetProperties" {
            return Some(vec![answer(request, PAUSED)]);
        }
        playing(request)
    })
    .await;

    let outcome = a_round_of(kodi).await;

    assert_eq!(the_source(&outcome).state, PlayState::Paused);
    let reading = the_reading(&outcome);
    assert_eq!(reading.state, PlayState::Paused);
    assert_eq!(reading.position, Known::Value(Duration::from_millis(6_603)));
}

#[tokio::test]
async fn a_kodi_with_no_video_player_is_described_with_nothing_open() {
    // Nothing open at all, then each of the other two players Kodi has.
    for active in [
        "[]",
        r#"[{"playerid":0,"playertype":"internal","type":"audio"}]"#,
        r#"[{"playerid":2,"playertype":"internal","type":"picture"}]"#,
    ] {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let kodi = a_kodi({
            let asked = Arc::clone(&asked);
            move |request| {
                asked
                    .lock()
                    .expect("a test thread held the lock")
                    .push(method(request).to_owned());
                if method(request) == "Player.GetActivePlayers" {
                    return Some(vec![answer(request, active)]);
                }
                playing(request)
            }
        })
        .await;

        let outcome = a_round_of(kodi).await;

        assert!(outcome.failures.is_empty(), "{active}: {outcome:?}");
        assert_eq!(the_source(&outcome).state, PlayState::Stopped, "{active}");
        assert!(outcome.snapshots.is_empty(), "{active}: {outcome:?}");
        assert_eq!(
            *asked.lock().expect("a test thread held the lock"),
            ["Player.GetActivePlayers"],
            "{active}"
        );
    }
}

#[tokio::test]
async fn the_item_and_its_properties_are_asked_of_the_player_kodi_named() {
    // An id other than the 1 Kodi 21.3 gives its video player, so a reading
    // that asks of player 1 whatever Kodi named fails here.
    let kodi = a_kodi(|request| match method(request) {
        "Player.GetActivePlayers" => Some(vec![answer(
            request,
            r#"[{"playerid":7,"playertype":"internal","type":"video"}]"#,
        )]),
        _ if request["params"]["playerid"] == 7 => playing(request),
        _ => Some(vec![a_fault(request)]),
    })
    .await;

    let outcome = a_round_of(kodi).await;

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(
        the_reading(&outcome).media,
        MediaRef::LocalFile(RawPath::from_bytes(FILE.as_bytes().to_vec()))
    );
}

#[tokio::test]
async fn no_kodi_at_the_address_is_a_round_with_nothing_in_it() {
    let (_held, nowhere) = a_closed_address();

    let outcome = a_round_of(nowhere).await;

    assert!(
        outcome.sources.is_empty() && outcome.snapshots.is_empty() && outcome.failures.is_empty(),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_kodi_started_after_the_watcher_is_found_by_the_next_round() {
    let (held, address) = a_closed_address();
    let mut watcher = KodiWatcher::new(address, SystemClock::new(), DEADLINE);

    let before = bounded(watcher.poll()).await.expect("a round");
    drop(held);
    let listener = TcpListener::bind(address)
        .await
        .expect("the port just let go of");
    serve_on(listener, playing);
    let after = bounded(watcher.poll()).await.expect("a round");

    assert!(
        before.sources.is_empty() && before.failures.is_empty(),
        "{before:?}"
    );
    assert_eq!(the_source(&after).state, PlayState::Playing);
}

#[tokio::test]
async fn a_kodi_that_quits_leaves_the_next_round_with_nothing_in_it() {
    let kodi = serve_on(a_listener().await, playing);
    let mut watcher = KodiWatcher::new(kodi.address, SystemClock::new(), DEADLINE);

    let before = bounded(watcher.poll()).await.expect("a round");
    kodi.quit().await;
    let after = bounded(watcher.poll()).await.expect("a round");

    assert_eq!(the_source(&before).state, PlayState::Playing);
    assert!(
        after.sources.is_empty() && after.failures.is_empty(),
        "{after:?}"
    );
}

#[tokio::test]
async fn one_connection_serves_every_round_while_kodi_keeps_it() {
    let kodi = serve_on(a_listener().await, playing);
    let mut watcher = KodiWatcher::new(kodi.address, SystemClock::new(), DEADLINE);

    for _round in 0..3 {
        let outcome = bounded(watcher.poll()).await.expect("a round");
        assert_eq!(the_source(&outcome).state, PlayState::Playing);
    }

    assert_eq!(kodi.connections(), 1);
}

#[tokio::test]
async fn a_connection_kodi_closed_is_opened_again_by_the_next_round() {
    // Kodi closes the connection at the first call of the second round.
    let asked = Arc::new(AtomicUsize::new(0));
    let kodi = serve_on(a_listener().await, {
        let asked = Arc::clone(&asked);
        move |request| {
            if asked.fetch_add(1, Ordering::SeqCst) == 3 {
                return None;
            }
            playing(request)
        }
    });
    let mut watcher = KodiWatcher::new(kodi.address, SystemClock::new(), DEADLINE);

    let first = bounded(watcher.poll()).await.expect("a round");
    let second = bounded(watcher.poll()).await.expect("a round");

    assert_eq!(the_source(&first).state, PlayState::Playing);
    assert!(second.failures.is_empty(), "{second:?}");
    assert_eq!(
        the_reading(&second).position,
        Known::Value(Duration::from_millis(3_528))
    );
    assert_eq!(kodi.connections(), 2);
}

/// Answer `answered` requests of `connection` as [`playing`] does, and reset
/// the connection at the next.
async fn reset_after(mut connection: TcpStream, answered: usize) {
    let mut pending = Vec::new();
    let mut chunk = [0; 4096];
    let mut asked = 0;

    loop {
        while let Some(request) = take_value(&mut pending) {
            if asked == answered {
                connection
                    .set_zero_linger()
                    .expect("a socket takes a linger of zero");
                return;
            }
            asked += 1;
            let answer = playing(&request).expect("a call Kodi answers").concat();
            connection
                .write_all(answer.as_bytes())
                .await
                .expect("the watcher reads");
        }
        let read = connection
            .read(&mut chunk)
            .await
            .expect("the watcher writes");
        if read == 0 {
            return;
        }
        pending.extend_from_slice(&chunk[..read]);
    }
}

#[tokio::test]
async fn a_connection_kodi_reset_is_opened_again_by_the_next_round() {
    // Kodi resets the connection at the first call of the second round, so
    // the read of that answer fails with an I/O error.
    let listener = a_listener().await;
    let address = listener
        .local_addr()
        .expect("a bound listener has an address");
    let serving = tokio::spawn(async move {
        let (first, _peer) = listener.accept().await.expect("a first connection");
        reset_after(first, 3).await;
        let (second, _peer) = listener.accept().await.expect("a second connection");
        serve(second, Arc::new(playing)).await;
    });
    let mut watcher = KodiWatcher::new(address, SystemClock::new(), DEADLINE);

    let first = bounded(watcher.poll()).await.expect("a round");
    let second = bounded(watcher.poll()).await.expect("a round");
    serving.abort();

    assert_eq!(the_source(&first).state, PlayState::Playing);
    assert!(second.failures.is_empty(), "{second:?}");
    assert_eq!(the_source(&second).state, PlayState::Playing);
}

#[tokio::test]
async fn a_round_that_fails_drops_its_connection() {
    // The first call is answered with no JSON, and every call after it as
    // Kodi answers. A connection kept after that answer keeps it as well.
    let asked = Arc::new(AtomicUsize::new(0));
    let kodi = serve_on(a_listener().await, {
        let asked = Arc::clone(&asked);
        move |request| {
            if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                return Some(vec!["HTTP/1.1 400 Bad Request\r\n\r\n".to_owned()]);
            }
            playing(request)
        }
    });
    let mut watcher = KodiWatcher::new(kodi.address, SystemClock::new(), DEADLINE);

    let failed = bounded(watcher.poll()).await.expect("a round");
    let read = bounded(watcher.poll()).await.expect("a round");

    assert_eq!(failed.failures.len(), 1, "{failed:?}");
    assert_eq!(the_source(&read).state, PlayState::Playing);
    assert_eq!(kodi.connections(), 2);
}

#[tokio::test]
async fn a_kodi_that_never_answers_fails_by_the_deadline() {
    let deadline = Duration::from_millis(200);
    let kodi = a_kodi(|_request| Some(Vec::new())).await;
    let mut watcher = KodiWatcher::new(kodi, SystemClock::new(), deadline);

    let started = Instant::now();
    let outcome = bounded(watcher.poll()).await.expect("a round");
    let took = started.elapsed();

    let (player, failed) = the_failure(outcome);
    assert_eq!(player, PlayerId(kodi.to_string()));
    assert!(
        matches!(
            &failed,
            WatchError::Timeout { player, deadline: given }
                if *player == PlayerId(kodi.to_string()) && *given == deadline
        ),
        "{failed:?}"
    );
    assert!(took < deadline * 2, "the round took {took:?}");
}

#[tokio::test]
async fn notifications_before_an_answer_are_passed_over() {
    // Back to back, as Kodi 21.3 writes them.
    let kodi = a_kodi(|request| {
        let answered = playing(request)?.concat();
        Some(vec![format!("{NOTIFICATION}{NOTIFICATION}{answered}")])
    })
    .await;

    let outcome = a_round_of(kodi).await;

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(
        the_reading(&outcome).position,
        Known::Value(Duration::from_millis(3_528))
    );
}

#[tokio::test]
async fn an_answer_that_arrives_in_pieces_is_read_whole() {
    let kodi = a_kodi(|request| {
        let whole = playing(request)?.concat();
        let (head, tail) = whole.split_at(whole.len() / 2);
        Some(vec![head.to_owned(), tail.to_owned()])
    })
    .await;

    let outcome = a_round_of(kodi).await;

    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(
        the_reading(&outcome).duration,
        Known::Value(Duration::from_millis(600_002))
    );
}

#[tokio::test]
async fn an_answer_that_is_no_json_fails_the_source() {
    // What an HTTP server at the address sends back.
    let kodi = a_kodi(|_request| {
        Some(vec![
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_owned(),
        ])
    })
    .await;

    let (_player, failed) = the_failure(a_round_of(kodi).await);

    let said = failed.to_string();
    assert!(
        said.contains("Player.GetActivePlayers") && said.contains("no JSON"),
        "{said}"
    );
}

#[tokio::test]
async fn a_call_kodi_refuses_fails_the_source_in_kodis_words() {
    let kodi = a_kodi(|request| Some(vec![a_fault(request)])).await;

    let (_player, failed) = the_failure(a_round_of(kodi).await);

    let said = failed.to_string();
    assert!(
        said.contains("Player.GetActivePlayers") && said.contains("Invalid params."),
        "{said}"
    );
}

#[tokio::test]
async fn a_kodi_that_closes_before_its_answer_fails_the_source() {
    let kodi = a_kodi(|_request| None).await;

    let (_player, failed) = the_failure(a_round_of(kodi).await);

    let said = failed.to_string();
    assert!(
        said.contains("closed before answering Player.GetActivePlayers"),
        "{said}"
    );
}

#[tokio::test]
async fn a_value_past_the_limit_fails_the_source_before_the_deadline() {
    // One value that runs past the limit without ending.
    let deadline = Duration::from_secs(5);
    let endless = format!(
        r#"{{"jsonrpc":"2.0","method":"Other.OnChange","params":"{}"#,
        "a".repeat(70_000)
    );
    let kodi = a_kodi(move |_request| Some(vec![endless.clone()])).await;
    let mut watcher = KodiWatcher::new(kodi, SystemClock::new(), deadline);

    let started = Instant::now();
    let outcome = bounded(watcher.poll()).await.expect("a round");
    let took = started.elapsed();

    let (_player, failed) = the_failure(outcome);
    let said = failed.to_string();
    assert!(said.contains("65536"), "{said}");
    assert!(took < deadline, "the round took {took:?}");
}

#[tokio::test]
async fn an_answer_out_of_shape_fails_the_source() {
    let kodi = a_kodi(|request| Some(vec![answer(request, r#"{"players":[]}"#)])).await;

    let (_player, failed) = the_failure(a_round_of(kodi).await);

    let said = failed.to_string();
    assert!(
        said.contains("Player.GetActivePlayers") && said.contains("out of shape"),
        "{said}"
    );
}

#[tokio::test]
async fn a_reading_is_timed_by_the_clock_the_watcher_was_given() {
    let kodi = a_kodi(playing).await;
    let clock = TestClock::new();
    clock.advance(Duration::from_secs(42));
    let then = clock.now();
    let mut watcher = KodiWatcher::new(kodi, clock, DEADLINE);

    let outcome = bounded(watcher.poll()).await.expect("a round");

    assert_eq!(the_reading(&outcome).observed_at, then);
}

#[tokio::test]
async fn the_listing_names_a_kodi_that_answers() {
    let kodi = a_kodi(playing).await;
    let mut watcher = KodiWatcher::new(kodi, SystemClock::new(), DEADLINE);

    let sources = bounded(watcher.sources()).await.expect("a listing");

    let named: Vec<&PlayerId> = sources.iter().map(|source| &source.player).collect();
    assert_eq!(named, [&PlayerId(kodi.to_string())]);
}

#[tokio::test]
async fn the_listing_leaves_out_a_kodi_that_fails() {
    let kodi = a_kodi(|_request| None).await;
    let mut watcher = KodiWatcher::new(kodi, SystemClock::new(), DEADLINE);

    let sources = bounded(watcher.sources()).await.expect("a listing");

    assert!(sources.is_empty(), "{sources:?}");
}

#[tokio::test]
async fn the_default_table_allows_the_application_kodi_is_read_as() {
    let kodi = a_kodi(playing).await;

    let outcome = a_round_of(kodi).await;

    assert_eq!(
        PolicyTable::allowing_video_players().policy_for(&the_source(&outcome).app),
        Policy::Auto
    );
}

#[tokio::test]
async fn the_kodi_adapter_satisfies_the_contract() {
    let kodi = a_kodi(playing).await;

    contract::verify(contract::Subject {
        watcher: KodiWatcher::new(kodi, SystemClock::new(), DEADLINE),
        deadline: DEADLINE,
    })
    .await;
}
