//! The Kodi adapter: Kodi, read over the JSON-RPC it answers on TCP.
//!
//! Kodi publishes no MPRIS name. With its default settings it answers JSON-RPC
//! at [`ADDRESS`]. A round asks it `Player.GetActivePlayers`, which names the
//! video player, and where it names one, `Player.GetItem` for the file that
//! player plays and `Player.GetProperties` for its speed, time and total time.
//! Kodi 21.3 logs a line as it takes each connection and another as the
//! connection closes, so the calls go over one connection, held from one round
//! to the next while Kodi keeps it. A round that holds none, or finds the one
//! it held closed, connects, which finds a Kodi started after the daemon. A
//! round that fails drops its connection. Kodi also writes notifications, such
//! as `Player.OnPause`, down an open connection, and a reading passes over
//! those.
//!
//! The source is Kodi's video player. Kodi plays music and pictures through
//! players of their own, and those are not read: policy is keyed on the
//! application, and Kodi is one application whatever it plays, so a track read
//! here would be admitted beside every episode.
//!
//! A source is identified by the address it is read at, such as
//! `127.0.0.1:9090`. A well-known bus name cannot hold a colon, so no MPRIS
//! source shares that identity.
//!
//! Kodi 21.3, at JSON-RPC 13.5, writes each answer and each notification as one
//! JSON value with nothing between two of them, and reads a request the same
//! way.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use benshi_core::clock::{Clock, Timestamp};
use benshi_core::path::RawPath;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

/// Where Kodi answers JSON-RPC over TCP with its default settings.
pub const ADDRESS: SocketAddr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9090));

/// The application every source read here belongs to.
const APP: &str = "kodi";

/// The type `Player.GetActivePlayers` gives the player that plays video.
const VIDEO: &str = "video";

/// What Kodi's video player is able to report.
///
/// Kodi gives the file, the time, the total time and the speed of whatever the
/// player has open, so all four hold whatever it plays.
const CAPABILITIES: Capabilities = Capabilities {
    position: true,
    duration: true,
    paused: true,
    location: true,
};

/// How large one value Kodi sends may grow before the reading is given up.
///
/// Each answer and notification Kodi 21.3 sends here runs to a few hundred
/// bytes beside the name of the file. A peer that sends one value without end
/// is given up on once the value passes this size, before the deadline.
const VALUE_LIMIT: usize = 64 * 1024;

/// The call that names the players that have something open.
const ACTIVE_PLAYERS: &str = "Player.GetActivePlayers";

/// The call that names what one player has open.
const ITEM: &str = "Player.GetItem";

/// The call that gives one player's speed, time and total time.
const PROPERTIES: &str = "Player.GetProperties";

/// Why a reading of Kodi stopped short of its answers.
///
/// Each message completes "failed: transport failure: ".
#[derive(Debug, thiserror::Error)]
enum Refused {
    /// The connection itself could not be made, for a reason other than no
    /// Kodi at the address.
    #[error("cannot connect: {0}")]
    Connect(io::Error),
    /// Writing a request or reading its answer failed.
    #[error("{method}: {failed}")]
    Io {
        method: &'static str,
        failed: io::Error,
    },
    /// The connection closed before the answer arrived.
    #[error("closed before answering {method}")]
    Closed { method: &'static str },
    /// What arrived is no JSON.
    #[error("answered {method} with no JSON: {garbled}")]
    NotJson {
        method: &'static str,
        garbled: serde_json::Error,
    },
    /// One value grew past [`VALUE_LIMIT`].
    #[error("sent over {VALUE_LIMIT} bytes of one value before answering {method}")]
    TooLong { method: &'static str },
    /// Kodi answered the call with an error of its own.
    #[error("refused {method}: {message} ({code})")]
    Fault {
        method: &'static str,
        code: i64,
        message: String,
    },
    /// The answer is JSON of another shape than the call gives.
    #[error("answered {method} out of shape: {unexpected}")]
    Unexpected {
        method: &'static str,
        unexpected: serde_json::Error,
    },
}

/// One player `Player.GetActivePlayers` names.
#[derive(Debug, Deserialize)]
struct ActivePlayer {
    playerid: i64,
    #[serde(rename = "type")]
    kind: String,
}

/// What `Player.GetItem` answers.
#[derive(Debug, Deserialize)]
struct Playing {
    item: Item,
}

/// What a player has open.
#[derive(Debug, Deserialize)]
struct Item {
    /// The path or URL of the file, empty when nothing is open.
    file: String,
}

/// What `Player.GetProperties` answers for the three properties asked.
#[derive(Debug, Deserialize)]
struct Properties {
    speed: i64,
    time: Time,
    totaltime: Time,
}

/// A time as Kodi gives it.
#[derive(Debug, Deserialize)]
struct Time {
    hours: u32,
    minutes: u32,
    seconds: u32,
    milliseconds: u32,
}

impl Time {
    /// The time as a duration.
    fn duration(&self) -> Duration {
        let seconds =
            u64::from(self.hours) * 3600 + u64::from(self.minutes) * 60 + u64::from(self.seconds);

        Duration::from_secs(seconds) + Duration::from_millis(u64::from(self.milliseconds))
    }
}

/// An error Kodi answers a call with.
#[derive(Debug, Deserialize)]
struct Fault {
    code: i64,
    message: String,
}

/// What a reading finds: the video player, and what it has open.
#[derive(Debug)]
struct Observation {
    source: SourceInfo,
    /// The reading, absent when the video player has nothing open.
    reading: Option<PlayerSnapshot>,
}

/// Describe the video player and read it, from what its item and its
/// properties answered at `at`.
///
/// An item with no file is nothing open.
fn observe(
    identity: &PlayerId,
    file: String,
    properties: &Properties,
    at: Timestamp,
) -> Observation {
    let Some(media) = media_from(file) else {
        return nothing_open(identity);
    };
    let state = play_state(properties.speed);
    let position = properties.time.duration();
    let reading = PlayerSnapshot {
        player: identity.clone(),
        media,
        state,
        position: Known::Value(position),
        duration: duration_of(properties.totaltime.duration(), position),
        observed_at: at,
    };

    Observation {
        source: describe(identity, state),
        reading: Some(reading),
    }
}

/// The video player of a Kodi that answered, with nothing open.
fn nothing_open(identity: &PlayerId) -> Observation {
    Observation {
        source: describe(identity, PlayState::Stopped),
        reading: None,
    }
}

/// The video player, in the state it is in.
fn describe(identity: &PlayerId, state: PlayState) -> SourceInfo {
    SourceInfo {
        player: identity.clone(),
        app: AppName(APP.to_owned()),
        capabilities: CAPABILITIES,
        state,
    }
}

/// What a file Kodi plays is.
///
/// Kodi names a local file by its absolute path, and anything else by a URL:
/// `smb://`, `nfs://`, `http://`, `plugin://`. An empty name is nothing open.
fn media_from(file: String) -> Option<MediaRef> {
    if file.is_empty() {
        None
    } else if file.starts_with('/') {
        Some(MediaRef::LocalFile(RawPath::from_bytes(file.into_bytes())))
    } else {
        Some(MediaRef::Remote(file))
    }
}

/// The playback state of a speed: zero is paused, and any other speed plays.
fn play_state(speed: i64) -> PlayState {
    if speed == 0 {
        PlayState::Paused
    } else {
        PlayState::Playing
    }
}

/// How long the item is, from its total time.
///
/// Advisory, and guarded twice: zero is no length, and a length shorter than
/// the time it arrives with is one no file can have.
fn duration_of(total: Duration, position: Duration) -> Known<Duration> {
    if total.is_zero() || total < position {
        Known::NotReported
    } else {
        Known::Value(total)
    }
}

/// Box a refusal as a transport error.
fn transport(refused: Refused) -> WatchError {
    WatchError::Transport(Box::new(refused))
}

/// One connection to Kodi, open for one round.
struct Connection {
    stream: TcpStream,
    /// What has arrived and is not taken yet: the start of a value whose end
    /// has not arrived.
    pending: Vec<u8>,
    /// The id of the request last sent, which its answer carries.
    sent: u64,
}

impl Connection {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            sent: 0,
        }
    }

    /// Ask `method`, and read its result as `T`.
    async fn ask<T: DeserializeOwned>(
        &mut self,
        method: &'static str,
        params: Option<Value>,
    ) -> Result<T, Refused> {
        self.sent += 1;
        let mut request = json!({ "jsonrpc": "2.0", "id": self.sent, "method": method });
        if let Some(params) = params {
            request["params"] = params;
        }
        self.stream
            .write_all(request.to_string().as_bytes())
            .await
            .map_err(|failed| Refused::Io { method, failed })?;

        let answer = self.answer(method).await?;
        let unexpected = |unexpected| Refused::Unexpected { method, unexpected };
        if let Some(fault) = answer.get("error") {
            let Fault { code, message } = Fault::deserialize(fault).map_err(unexpected)?;
            return Err(Refused::Fault {
                method,
                code,
                message,
            });
        }

        T::deserialize(answer.get("result").unwrap_or(&Value::Null)).map_err(unexpected)
    }

    /// The answer to the request last sent, past every notification that
    /// arrives before it.
    async fn answer(&mut self, method: &'static str) -> Result<Value, Refused> {
        let mut chunk = [0; 4096];

        loop {
            while let Some(value) = self.next_value(method)? {
                if value.get("id").and_then(Value::as_u64) == Some(self.sent) {
                    return Ok(value);
                }
            }
            if self.pending.len() > VALUE_LIMIT {
                return Err(Refused::TooLong { method });
            }
            let read = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|failed| Refused::Io { method, failed })?;
            if read == 0 {
                return Err(Refused::Closed { method });
            }
            self.pending.extend_from_slice(&chunk[..read]);
        }
    }

    /// The next whole value that has arrived, taken off what is pending.
    fn next_value(&mut self, method: &'static str) -> Result<Option<Value>, Refused> {
        let mut values = serde_json::Deserializer::from_slice(&self.pending).into_iter::<Value>();

        match values.next() {
            Some(Ok(value)) => {
                let used = values.byte_offset();
                self.pending.drain(..used);
                Ok(Some(value))
            }
            Some(Err(garbled)) if !garbled.is_eof() => Err(Refused::NotJson { method, garbled }),
            Some(Err(_)) | None => Ok(None),
        }
    }
}

/// Kodi, read over JSON-RPC at one address.
pub struct KodiWatcher<C> {
    address: SocketAddr,
    /// The address, as the identity of the one source read here.
    identity: PlayerId,
    clock: C,
    deadline: Duration,
    /// The connection held from one round to the next, while Kodi keeps it.
    held: Option<Connection>,
}

impl<C: Clock> KodiWatcher<C> {
    /// A watcher of the Kodi that answers at `address`.
    ///
    /// Nothing is connected here: the first round connects.
    ///
    /// The clock is taken rather than read, so a test can move twenty seconds
    /// of playback in no time at all.
    ///
    /// The deadline bounds one reading as a whole: every call it makes, and
    /// a connection where it opens one.
    #[must_use]
    pub fn new(address: SocketAddr, clock: C, deadline: Duration) -> Self {
        Self {
            address,
            identity: PlayerId(address.to_string()),
            clock,
            deadline,
            held: None,
        }
    }

    /// Read Kodi once, by the deadline.
    ///
    /// No Kodi at the address, which refuses the connection, is no source.
    /// Anything else that stops the reading fails the source.
    async fn read(&mut self) -> Result<Option<Observation>, WatchError> {
        let deadline = self.deadline;

        timeout(deadline, self.reached())
            .await
            .unwrap_or_else(|_elapsed| {
                Err(WatchError::Timeout {
                    player: self.identity.clone(),
                    deadline,
                })
            })
    }

    /// Read Kodi over the connection held, or over a new one where none is
    /// held or Kodi has closed it.
    ///
    /// The connection is taken out for the reading and held again only where
    /// the reading succeeds, so a reading that fails, or runs out of time,
    /// leaves the next round to connect.
    async fn reached(&mut self) -> Result<Option<Observation>, WatchError> {
        if let Some(mut kodi) = self.held.take() {
            match self.asked(&mut kodi).await {
                Ok(observed) => {
                    self.held = Some(kodi);
                    return Ok(Some(observed));
                }
                // Closed since the last round: by a Kodi that quit, which
                // refuses the new connection, or one started again.
                Err(Refused::Closed { .. } | Refused::Io { .. }) => {}
                Err(refused) => return Err(transport(refused)),
            }
        }

        let stream = match TcpStream::connect(self.address).await {
            Ok(stream) => stream,
            Err(refused) if refused.kind() == io::ErrorKind::ConnectionRefused => return Ok(None),
            Err(failed) => return Err(transport(Refused::Connect(failed))),
        };
        let mut kodi = Connection::new(stream);
        let observed = self.asked(&mut kodi).await.map_err(transport)?;
        self.held = Some(kodi);

        Ok(Some(observed))
    }

    /// The calls of one reading, in turn, over one connection.
    async fn asked(&self, kodi: &mut Connection) -> Result<Observation, Refused> {
        let players: Vec<ActivePlayer> = kodi.ask(ACTIVE_PLAYERS, None).await?;
        let Some(video) = players.into_iter().find(|player| player.kind == VIDEO) else {
            return Ok(nothing_open(&self.identity));
        };
        let playing: Playing = kodi
            .ask(
                ITEM,
                Some(json!({ "playerid": video.playerid, "properties": ["file"] })),
            )
            .await?;
        let properties: Properties = kodi
            .ask(
                PROPERTIES,
                Some(json!({
                    "playerid": video.playerid,
                    "properties": ["speed", "time", "totaltime"],
                })),
            )
            .await?;
        // The moment of the answer, which is the moment its time was the
        // player's.
        let at = self.clock.now();

        Ok(observe(&self.identity, playing.item.file, &properties, at))
    }
}

impl<C: Clock> PlayerWatcher for KodiWatcher<C> {
    /// Kodi's video player, where Kodi answers. A Kodi that fails to answer
    /// is left out: a listing has no channel for a per-source failure.
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        let found = self.read().await.ok().flatten();

        Ok(found.map(|observed| observed.source).into_iter().collect())
    }

    /// One reading, under one deadline. No Kodi at the address is a round
    /// with nothing in it, and a Kodi that fails is reported in
    /// [`PollOutcome::failures`].
    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        let mut outcome = PollOutcome {
            sources: Vec::new(),
            snapshots: Vec::new(),
            failures: Vec::new(),
        };

        match self.read().await {
            Ok(Some(Observation { source, reading })) => {
                outcome.sources.push(source);
                outcome.snapshots.extend(reading);
            }
            Ok(None) => {}
            Err(failed) => outcome.failures.push((self.identity.clone(), failed)),
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use benshi_core::clock::{Clock, TestClock};
    use benshi_core::path::RawPath;
    use benshi_core::{Known, MediaRef, PlayState, PlayerId};

    use super::{Properties, Time, media_from, observe, play_state};

    const FILE: &str = "/media/anime/[Group] Show - 03 (1080p).mkv";

    fn a_time(hours: u32, minutes: u32, seconds: u32, milliseconds: u32) -> Time {
        Time {
            hours,
            minutes,
            seconds,
            milliseconds,
        }
    }

    fn properties(speed: i64, time: Time, totaltime: Time) -> Properties {
        Properties {
            speed,
            time,
            totaltime,
        }
    }

    fn kodi() -> PlayerId {
        PlayerId("127.0.0.1:9090".to_owned())
    }

    #[test]
    fn a_time_counts_its_hours_minutes_seconds_and_milliseconds() {
        assert_eq!(
            a_time(1, 2, 3, 4).duration(),
            Duration::from_millis(3_723_004)
        );
    }

    #[test]
    fn a_speed_of_zero_is_paused_and_any_other_speed_plays() {
        assert_eq!(play_state(0), PlayState::Paused);
        for speed in [1, 2, -2] {
            assert_eq!(play_state(speed), PlayState::Playing, "{speed}");
        }
    }

    #[test]
    fn an_absolute_path_is_a_local_file() {
        assert_eq!(
            media_from(FILE.to_owned()),
            Some(MediaRef::LocalFile(RawPath::from_bytes(
                FILE.as_bytes().to_vec()
            )))
        );
    }

    #[test]
    fn any_other_name_is_an_address() {
        for address in [
            "smb://nas/anime/[Group] Show - 03 (1080p).mkv",
            "plugin://plugin.video.example/?episode=3",
            "https://example.invalid/episode-3.m3u8",
        ] {
            assert_eq!(
                media_from(address.to_owned()),
                Some(MediaRef::Remote(address.to_owned())),
                "{address}"
            );
        }
    }

    #[test]
    fn an_item_with_no_file_is_nothing_open() {
        // What Kodi 21.3 answers once its player stops: an item whose file is
        // empty, and every time zero.
        let stopped = properties(0, a_time(0, 0, 0, 0), a_time(0, 0, 0, 0));

        let observed = observe(&kodi(), String::new(), &stopped, TestClock::new().now());

        assert_eq!(observed.source.state, PlayState::Stopped);
        assert_eq!(observed.reading, None);
    }

    #[test]
    fn a_reading_carries_the_time_and_the_total_time() {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(42));
        let playing = properties(1, a_time(0, 0, 3, 528), a_time(0, 10, 0, 2));

        let observed = observe(&kodi(), FILE.to_owned(), &playing, clock.now());
        let reading = observed.reading.expect("a reading of the file");

        assert_eq!(observed.source.state, PlayState::Playing);
        assert_eq!(reading.state, PlayState::Playing);
        assert_eq!(reading.position, Known::Value(Duration::from_millis(3_528)));
        assert_eq!(
            reading.duration,
            Known::Value(Duration::from_millis(600_002))
        );
        assert_eq!(reading.observed_at, clock.now());
    }

    #[test]
    fn a_total_time_of_zero_is_no_length() {
        // At its start, where a length of zero is no shorter than the time,
        // so only the rule for zero leaves it out.
        let streaming = properties(1, a_time(0, 0, 0, 0), a_time(0, 0, 0, 0));

        let observed = observe(&kodi(), FILE.to_owned(), &streaming, TestClock::new().now());

        assert_eq!(
            observed.reading.expect("a reading").duration,
            Known::NotReported
        );
    }

    #[test]
    fn a_total_time_short_of_the_time_is_no_length() {
        let lying = properties(1, a_time(0, 5, 0, 0), a_time(0, 4, 59, 999));

        let observed = observe(&kodi(), FILE.to_owned(), &lying, TestClock::new().now());

        assert_eq!(
            observed.reading.expect("a reading").duration,
            Known::NotReported
        );
    }

    #[test]
    fn a_total_time_equal_to_the_time_is_a_length() {
        // The last reading of a file that played to its end.
        let ended = properties(1, a_time(0, 10, 0, 2), a_time(0, 10, 0, 2));

        let observed = observe(&kodi(), FILE.to_owned(), &ended, TestClock::new().now());

        assert_eq!(
            observed.reading.expect("a reading").duration,
            Known::Value(Duration::from_millis(600_002))
        );
    }
}
