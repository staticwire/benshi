//! The MPRIS adapter: every media player that publishes itself on the session
//! bus.
//!
//! Confirmed against zbus 5.19.0 before this module was written, because the
//! surface moves between major versions: the attribute macro is `zbus::proxy`,
//! a session connection comes from `Connection::session()`, and the bus names
//! come from `fdo::DBusProxy::list_names()`. The two proxies this module needs,
//! `fdo::DBusProxy` and `fdo::PropertiesProxy`, are supplied by zbus itself and
//! are used rather than redeclared.
//!
//! Every property this adapter reads arrives in a single `GetAll` per source,
//! so a listing costs one round trip per player and the whole translation below
//! is pure and testable without a bus.

use std::collections::HashMap;
use std::time::Duration;

use benshi_core::clock::{Clock, Timestamp};
use benshi_core::path::RawPath;
use benshi_core::{AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot};
use futures_util::future::join_all;
use tokio::time::timeout;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

use crate::{PlayerWatcher, PollOutcome, SourceInfo, WatchError};

/// The prefix every MPRIS bus name carries, including its trailing dot.
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// The prefix a bus name component carries when it distinguishes one instance
/// of a player from another.
const INSTANCE_QUALIFIER: &str = "instance";

/// The object every MPRIS player exports, fixed by the specification.
const PLAYER_OBJECT: &str = "/org/mpris/MediaPlayer2";

/// The interface carrying everything about playback.
///
/// Built through the const constructor zbus offers for a literal, so that no
/// call pays to validate it. The checked constructor reports a misspelling as a
/// transport failure, which is the wrong class for a typo in a constant. The
/// test below holds the literal to the rule instead.
const PLAYER_INTERFACE: InterfaceName<'static> =
    InterfaceName::from_static_str_unchecked("org.mpris.MediaPlayer2.Player");

/// The scheme a `xesam:url` carries when it names a file on this machine.
const LOCAL_FILE_SCHEME: &str = "file://";

/// How long from the start of one round of readings to the start of the next.
///
/// A suggestion rather than a rule, like [`SOURCE_DEADLINE`]: the daemon owns
/// the schedule and this crate only says what suits an ordinary desktop. One
/// second is short enough to notice a seek while the user still remembers
/// making it, and long enough that a dozen players take under two percent of
/// it.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long one call may take before it is given up on.
///
/// A suggestion rather than a rule: [`MprisWatcher::connect`] takes the
/// deadline it is to use, because the daemon may know better than this crate
/// does.
///
/// The size is set by [`POLL_INTERVAL`] and not by how fast a player answers. A
/// source that has stopped answering must be given up on before the next round
/// begins, or rounds overlap and one round's failures arrive during the next. A
/// round is two calls deep, the listing and then every reading together, and
/// each is bounded separately, so a round costs twice the deadline at worst.
/// Half the interval is the largest deadline that keeps a whole round inside
/// one interval.
///
/// Nothing is given up to get it. Measured on one desktop machine on
/// 2026-09-16, a listing and a reading across three players cost about ten
/// milliseconds together over eight calls, so one call costs a little over a
/// millisecond and this deadline is some four hundred times what a healthy
/// player needs.
pub const SOURCE_DEADLINE: Duration = Duration::from_millis(500);

// The relationship between the two is why either holds the value it does, so it
// is checked rather than described. Moving either to where a round no longer
// fits inside the interval stops the build, which prose cannot do. Nanoseconds
// are what a `Duration` holds, so a violation smaller than a millisecond cannot
// round itself away.
const _: () = assert!(
    SOURCE_DEADLINE.as_nanos() * 2 <= POLL_INTERVAL.as_nanos(),
    "a source deadline above half the poll interval lets rounds overlap"
);

/// The properties one `GetAll` returns, keyed as the bus spelled them.
type Properties = HashMap<String, OwnedValue>;

/// The session bus itself did not answer in time.
///
/// Distinct from [`WatchError::Timeout`], which names the source that failed to
/// answer. Enumerating the bus is not attributable to any one player, so it is
/// reported as a transport failure instead.
#[derive(Debug, thiserror::Error)]
#[error("the session bus did not answer within {deadline:?}")]
struct BusTimeout {
    deadline: Duration,
}

/// Box a failure as a transport error.
fn transport(error: impl std::error::Error + Send + Sync + 'static) -> WatchError {
    WatchError::Transport(Box::new(error))
}

/// Await one call made to the session bus itself, under `deadline`.
///
/// Distinct from a call made to one player, which names that player when it
/// fails. This is the only place a [`BusTimeout`] is raised.
async fn bus_call<T, E>(
    deadline: Duration,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, WatchError>
where
    E: std::error::Error + Send + Sync + 'static,
{
    timeout(deadline, call)
        .await
        .map_err(|_elapsed| transport(BusTimeout { deadline }))?
        .map_err(transport)
}

/// The identity of a source, from the name it claimed on the bus.
///
/// Returns `None` for a name outside the MPRIS namespace, and for the bare
/// namespace itself, which names no player.
fn identity_from_bus_name(name: &str) -> Option<PlayerId> {
    let suffix = name.strip_prefix(MPRIS_PREFIX)?;

    (!suffix.is_empty()).then(|| PlayerId(suffix.to_owned()))
}

/// The application an identity belongs to.
///
/// One trailing component is removed when it qualifies an instance, and never
/// more than one: a player that names itself in reverse DNS puts dots inside
/// the part that is the application.
///
/// This rule is platform knowledge and lives here for that reason: `core` is
/// given both names and never derives one from the other.
fn app_from_identity(identity: &PlayerId) -> AppName {
    let whole = identity.0.as_str();
    let application = whole
        .rsplit_once('.')
        .filter(|&(before, last)| {
            // A qualifier carries a unique identifier after its prefix, so a
            // component that is exactly `instance` qualifies nothing. Requiring
            // a non-empty prefix keeps the application name non-empty, which the
            // contract demands of every source.
            !before.is_empty()
                && last.starts_with(INSTANCE_QUALIFIER)
                && last.len() > INSTANCE_QUALIFIER.len()
        })
        .map_or(whole, |(before, _)| before);

    AppName(application.to_owned())
}

/// What the source says it is doing.
///
/// MPRIS defines exactly three statuses. Anything else is a player extending
/// the interface, and is read as stopped: advancing progress through a file on
/// the strength of an undefined string is the worse of the two mistakes.
fn play_state(status: Option<&str>) -> PlayState {
    match status {
        Some("Playing") => PlayState::Playing,
        Some("Paused") => PlayState::Paused,
        _ => PlayState::Stopped,
    }
}

/// What this source is able to report.
///
/// Each of the four describes the source rather than the track it happens to
/// hold. `location` is whether the source publishes `xesam:url` at all, which
/// is a property of the player: mpv publishes one for everything it opens, and
/// a browser publishes none. Whether that address turns out to name a local
/// file changes with the track, so it is carried by `MediaRef` in the reading
/// and decides nothing here.
fn declare_capabilities(player: &Properties, metadata: &Properties) -> Capabilities {
    Capabilities {
        position: player.contains_key("Position"),
        // A length belongs to the MPRIS metadata schema, so the method can
        // always carry one. A track that omits `mpris:length` is NotReported
        // rather than Unsupported, which is the distinction `Known` exists for.
        duration: true,
        paused: boolean(player, "CanPause"),
        // A key holding an empty string names nothing, so it is not a location.
        // Declaring one would promise a reading that cannot arrive: the
        // contract requires a reported path or address to be non-empty.
        location: non_empty_text(metadata, "xesam:url").is_some(),
    }
}

/// How far into the media playback has reached.
///
/// Zero is the start of a file rather than an absence, so only a negative value
/// is treated as one. A player paused at the beginning reports zero, and
/// discarding it would leave the first seconds of every file unobserved.
fn position_of(player: &Properties, declared: bool) -> Known<Duration> {
    if !declared {
        return Known::Unsupported;
    }

    match micros(player, "Position") {
        Some(value) if value >= 0 => Known::Value(Duration::from_micros(value.unsigned_abs())),
        _ => Known::NotReported,
    }
}

/// How long the media is, as reported.
///
/// Advisory, and guarded twice. A length that is zero or negative is absent
/// rather than a value. A length shorter than the position it arrives with is a
/// lie that nothing downstream can tell from a genuinely short file.
fn duration_of(
    metadata: &Properties,
    position: Known<Duration>,
    declared: bool,
) -> Known<Duration> {
    if !declared {
        return Known::Unsupported;
    }

    let Some(length) = micros(metadata, "mpris:length").filter(|&value| value > 0) else {
        return Known::NotReported;
    };
    let length = Duration::from_micros(length.unsigned_abs());

    match position {
        Known::Value(position) if length < position => Known::NotReported,
        _ => Known::Value(length),
    }
}

/// A microsecond count, absent when missing or not the type MPRIS specifies.
fn micros(properties: &Properties, key: &str) -> Option<i64> {
    properties
        .get(key)
        .and_then(|value| i64::try_from(value).ok())
}

/// What the source has open, or nothing when it has nothing open.
///
/// `xesam:url` is preferred over `xesam:title`, and not merely tried first. A
/// player holding a filename that is not valid UTF-8 percent-encodes those
/// bytes into the URL, where they survive exactly; by the time the same name
/// reaches `xesam:title` it has been through a lossy conversion and is
/// destroyed. Observed on mpv 0.41.0 with a Shift-JIS filename.
fn media_from(metadata: &Properties) -> Option<MediaRef> {
    if let Some(url) = non_empty_text(metadata, "xesam:url") {
        return Some(match url.strip_prefix(LOCAL_FILE_SCHEME) {
            // A remainder that does not begin with a slash carries an
            // authority, so the path it names is on another machine.
            Some(path) if path.starts_with('/') => {
                MediaRef::LocalFile(RawPath::from_bytes(percent_decode(path)))
            }
            _ => MediaRef::Remote(url.to_owned()),
        });
    }

    non_empty_text(metadata, "xesam:title").map(|title| MediaRef::Title(title.to_owned()))
}

/// Decode percent escapes into bytes.
///
/// Into bytes and never into a `String`: a D-Bus string must be valid UTF-8, so
/// a player holding a filename that is not has no choice but to escape those
/// bytes, and decoding into text would fail or replace exactly the names this
/// program exists to read.
///
/// Anything that is not a complete escape is carried through unchanged. A
/// player that emits a stray `%` is reporting a filename with a `%` in it, and
/// refusing the whole reading over one character would lose the file.
fn percent_decode(text: &str) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();

    while let [first, tail @ ..] = rest {
        // An escape consumes three bytes and yields one; anything else, an
        // incomplete escape included, carries its first byte through.
        let (byte, remaining) = leading_escape(rest).unwrap_or((*first, tail));
        decoded.push(byte);
        rest = remaining;
    }

    decoded
}

/// The byte an escape at the front of `bytes` stands for, with what follows it.
///
/// Absent when `bytes` does not begin with a complete escape, which is what
/// carries a stray `%` through [`percent_decode`] unchanged.
fn leading_escape(bytes: &[u8]) -> Option<(u8, &[u8])> {
    let [b'%', high, low, rest @ ..] = bytes else {
        return None;
    };

    Some((hex_value(*high)? << 4 | hex_value(*low)?, rest))
}

/// The value of one hexadecimal digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A boolean property, false when absent or of another type.
fn boolean(properties: &Properties, key: &str) -> bool {
    properties
        .get(key)
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

/// A string property, absent when missing or of another type.
fn text<'a>(properties: &'a Properties, key: &str) -> Option<&'a str> {
    properties
        .get(key)
        .and_then(|value| <&str>::try_from(value).ok())
}

/// A string property that names something, absent also when it holds nothing.
///
/// The contract requires a reported path, address or title to be non-empty, and
/// the same rule decides whether a source is declared able to name what it
/// opened, so a declaration and the reading it governs cannot disagree about
/// what counts as a name.
fn non_empty_text<'a>(properties: &'a Properties, key: &str) -> Option<&'a str> {
    text(properties, key).filter(|value| !value.is_empty())
}

/// The metadata dictionary nested inside a player's properties.
///
/// A player with nothing open publishes no metadata, and one that publishes
/// something other than a dictionary is treated the same way: an empty map,
/// from which every key is absent.
fn metadata_of(player: &Properties) -> Properties {
    player
        .get("Metadata")
        .and_then(|value| Properties::try_from(value.clone()).ok())
        .unwrap_or_default()
}

/// What a source is, as its own properties describe it.
fn describe(identity: &PlayerId, player: &Properties, metadata: &Properties) -> SourceInfo {
    SourceInfo {
        player: identity.clone(),
        app: app_from_identity(identity),
        capabilities: declare_capabilities(player, metadata),
        state: play_state(text(player, "PlaybackStatus")),
    }
}

/// What one answer from a source amounts to: what the source is, and what it
/// has open.
#[derive(Debug)]
struct Observation {
    /// The source, as the answer describes it.
    source: SourceInfo,
    /// The reading, absent when the source has nothing open.
    reading: Option<PlayerSnapshot>,
}

/// Describe a source and read it, from one answer.
///
/// The metadata is decoded once and the reading takes its capabilities and
/// playback state from the description, so a round cannot publish a description
/// that contradicts the reading beside it.
fn observe(identity: &PlayerId, player: &Properties, at: Timestamp) -> Observation {
    let metadata = metadata_of(player);
    let source = describe(identity, player, &metadata);

    let Some(media) = media_from(&metadata) else {
        return Observation {
            source,
            reading: None,
        };
    };

    let position = position_of(player, source.capabilities.position);
    let snapshot = PlayerSnapshot {
        player: source.player.clone(),
        media,
        state: source.state,
        position,
        duration: duration_of(&metadata, position, source.capabilities.duration),
        observed_at: at,
    };

    Observation {
        source,
        reading: Some(snapshot),
    }
}

/// Every MPRIS player on the session bus.
#[derive(Debug)]
pub struct MprisWatcher<C> {
    connection: zbus::Connection,
    clock: C,
    deadline: Duration,
}

impl<C: Clock> MprisWatcher<C> {
    /// Open a connection to the session bus.
    ///
    /// The clock is taken rather than read, so a test can move twenty seconds
    /// of playback in no time at all.
    ///
    /// The deadline applies to every call this watcher makes, separately: this
    /// one, the listing of bus names in each round, and each source's own
    /// `GetAll`. It bounds one call rather than one round.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] when there is no session bus to reach,
    /// or when it does not answer within `deadline`.
    pub async fn connect(clock: C, deadline: Duration) -> Result<Self, WatchError> {
        let connection = bus_call(deadline, zbus::Connection::session()).await?;

        Ok(Self {
            connection,
            clock,
            deadline,
        })
    }

    /// Every MPRIS identity the bus currently carries.
    async fn identities(&self) -> Result<Vec<PlayerId>, WatchError> {
        let bus = DBusProxy::new(&self.connection).await.map_err(transport)?;
        let names = bus_call(self.deadline, bus.list_names()).await?;

        Ok(names
            .iter()
            .filter_map(|name| identity_from_bus_name(name))
            .collect())
    }

    /// Observe one source, keeping the identity so that a failure can name it.
    ///
    /// The identity returned here is the one a [`WatchError`] was built from,
    /// so the two halves of a reported failure cannot name different players.
    async fn read(&self, identity: PlayerId) -> (PlayerId, Result<Observation, WatchError>) {
        let observed = self
            .player_properties(&identity)
            .await
            .map(|player| observe(&identity, &player, self.clock.now()));

        (identity, observed)
    }

    /// Describe one source, or nothing if it did not answer.
    async fn listed(&self, identity: PlayerId) -> Option<SourceInfo> {
        let player = self.player_properties(&identity).await.ok()?;
        let metadata = metadata_of(&player);

        Some(describe(&identity, &player, &metadata))
    }

    /// Read every playback property of one source in a single call.
    async fn player_properties(&self, identity: &PlayerId) -> Result<Properties, WatchError> {
        let destination = format!("{MPRIS_PREFIX}{}", identity.0);
        let proxy = PropertiesProxy::builder(&self.connection)
            .destination(destination)
            .map_err(transport)?
            .path(PLAYER_OBJECT)
            .map_err(transport)?
            // Nothing here reads a property through the proxy's own accessors,
            // so caching would buy nothing and would cost a signal match rule
            // on every player for the lifetime of the connection.
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(transport)?;

        timeout(self.deadline, proxy.get_all(PLAYER_INTERFACE))
            .await
            .map_err(|_elapsed| WatchError::Timeout {
                player: identity.clone(),
                deadline: self.deadline,
            })?
            .map_err(transport)
    }
}

impl<C: Clock> PlayerWatcher for MprisWatcher<C> {
    /// Sources are queried together rather than in turn, so one player that
    /// accepts a call and never answers costs one deadline and not one each.
    /// Such a player is omitted: a listing has no channel for a per-source
    /// failure, and failing it wholesale because one player hung would blind
    /// the daemon to every other.
    ///
    /// Sorted by identity, so two calls with nothing changed return the same
    /// listing in the same order.
    async fn sources(&mut self) -> Result<Vec<SourceInfo>, WatchError> {
        let described = self
            .identities()
            .await?
            .into_iter()
            .map(|identity| self.listed(identity));

        let mut sources: Vec<SourceInfo> =
            join_all(described).await.into_iter().flatten().collect();
        sources.sort_by(|left, right| left.player.cmp(&right.player));

        Ok(sources)
    }

    /// Every source is read in the same round and under its own deadline. A
    /// source with nothing open contributes no snapshot rather than one whose
    /// media reference is empty, and is described all the same. A source that
    /// failed is reported in [`PollOutcome::failures`] rather than failing the
    /// round, and is described nowhere: it answered nothing to describe it by.
    ///
    /// One `GetAll` per source serves both halves, so reporting the listing
    /// alongside the readings costs the bus nothing beyond what a round of
    /// readings already costs it.
    async fn poll(&mut self) -> Result<PollOutcome, WatchError> {
        let readings = self
            .identities()
            .await?
            .into_iter()
            .map(|identity| self.read(identity));

        let mut sources = Vec::new();
        let mut snapshots = Vec::new();
        let mut failures = Vec::new();

        for (identity, observed) in join_all(readings).await {
            match observed {
                Ok(Observation { source, reading }) => {
                    sources.push(source);
                    snapshots.extend(reading);
                }
                Err(error) => failures.push((identity, error)),
            }
        }

        sources.sort_by(|left, right| left.player.cmp(&right.player));
        snapshots.sort_by(|left, right| left.player.cmp(&right.player));

        Ok(PollOutcome {
            sources,
            snapshots,
            failures,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PLAYER_INTERFACE, Properties, app_from_identity, declare_capabilities, duration_of,
        identity_from_bus_name, media_from, observe, percent_decode, play_state, position_of,
    };
    use benshi_core::clock::{Clock, TestClock, Timestamp};
    use benshi_core::path::RawPath;
    use benshi_core::{AppName, Known, MediaRef, PlayState, PlayerId};
    use std::time::Duration;
    use zbus::names::InterfaceName;
    use zbus::zvariant::{OwnedValue, Value};

    fn id(name: &str) -> PlayerId {
        PlayerId(name.to_owned())
    }

    fn a_string(value: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(value)).expect("a string is an owned value")
    }

    /// Build a property map from pairs, as one `GetAll` would return it.
    fn properties(pairs: Vec<(&str, OwnedValue)>) -> Properties {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect()
    }

    #[test]
    fn the_player_interface_constant_is_a_name_the_bus_would_accept() {
        // The constant is built unchecked so that no call pays to validate it.
        // The literal is held to the rule here instead.
        let interface = PLAYER_INTERFACE;

        assert!(InterfaceName::try_from(interface.as_str()).is_ok());
    }

    #[test]
    fn the_identity_is_the_whole_suffix_after_the_well_known_prefix() {
        assert_eq!(
            identity_from_bus_name("org.mpris.MediaPlayer2.mpv.instance1234"),
            Some(id("mpv.instance1234"))
        );
        assert_eq!(
            identity_from_bus_name("org.mpris.MediaPlayer2.Feishin"),
            Some(id("Feishin"))
        );
    }

    #[test]
    fn a_name_outside_the_mpris_namespace_is_not_a_source() {
        assert_eq!(identity_from_bus_name("org.freedesktop.DBus"), None);
        assert_eq!(identity_from_bus_name(":1.42"), None);
    }

    #[test]
    fn the_bare_namespace_names_no_player() {
        assert_eq!(identity_from_bus_name("org.mpris.MediaPlayer2"), None);
        assert_eq!(identity_from_bus_name("org.mpris.MediaPlayer2."), None);
    }

    #[test]
    fn an_instance_qualifier_is_not_part_of_the_application_name() {
        // Observed on the live session bus: chromium published
        // org.mpris.MediaPlayer2.chromium.instance16481, having published
        // instance30062 two days earlier. The qualifier carries a process id, so
        // an identity is not stable across restarts and cannot key policy.
        assert_eq!(
            app_from_identity(&id("chromium.instance16481")),
            AppName("chromium".to_owned())
        );
    }

    #[test]
    fn a_component_that_is_only_the_word_instance_qualifies_nothing() {
        // A qualifier carries a unique identifier after its prefix, so a
        // component that is exactly the prefix distinguishes no instance from
        // any other and is part of the name. This is the branch the length
        // check exists for, and nothing else reaches it.
        assert_eq!(
            app_from_identity(&id("mpv.instance")),
            AppName("mpv.instance".to_owned())
        );
    }

    #[test]
    fn an_instance_qualifier_need_not_be_a_number() {
        // Observed on 2026-09-16 by opening a second mpv: it took the name
        // org.mpris.MediaPlayer2.mpv.instance-mZlVuXZe. The specification's
        // example is a process id, so a rule demanding digits looks right and
        // would have made this source's application `mpv.instance-mZlVuXZe`,
        // which no denylist entry can match.
        assert_eq!(
            app_from_identity(&id("mpv.instance-mZlVuXZe")),
            AppName("mpv".to_owned())
        );
    }

    #[test]
    fn a_source_without_a_qualifier_is_its_own_application() {
        assert_eq!(
            app_from_identity(&id("Feishin")),
            AppName("Feishin".to_owned())
        );
        assert_eq!(app_from_identity(&id("mpv")), AppName("mpv".to_owned()));
    }

    #[test]
    fn a_dotted_application_name_survives_whole() {
        // Players that name themselves in reverse DNS put dots in the part that
        // is the application, so the rule strips one trailing component and
        // never takes the first.
        assert_eq!(
            app_from_identity(&id("io.bassi.Amberol")),
            AppName("io.bassi.Amberol".to_owned())
        );
    }

    #[test]
    fn an_application_name_is_never_emptied_by_the_rule() {
        // The contract requires every source to name its application. A player
        // whose whole identity looks like a qualifier must keep it rather than
        // be stripped to nothing.
        assert_eq!(
            app_from_identity(&id("instance42")),
            AppName("instance42".to_owned())
        );
        assert_eq!(
            app_from_identity(&id("instance")),
            AppName("instance".to_owned())
        );
    }

    #[test]
    fn the_three_playback_states_are_translated() {
        assert_eq!(play_state(Some("Playing")), PlayState::Playing);
        assert_eq!(play_state(Some("Paused")), PlayState::Paused);
        assert_eq!(play_state(Some("Stopped")), PlayState::Stopped);
    }

    #[test]
    fn an_unknown_playback_state_stops_rather_than_advances() {
        // MPRIS defines exactly three. Anything else is a player extending the
        // interface, and treating it as Playing would advance progress through
        // a file on the strength of a string nobody defined.
        assert_eq!(play_state(Some("Buffering")), PlayState::Stopped);
        assert_eq!(play_state(None), PlayState::Stopped);
    }

    #[test]
    fn a_source_that_publishes_no_position_property_cannot_report_one() {
        let without = properties(vec![("CanPause", OwnedValue::from(true))]);
        assert!(!declare_capabilities(&without, &Properties::new()).position);

        let with = properties(vec![("Position", OwnedValue::from(0_i64))]);
        assert!(declare_capabilities(&with, &Properties::new()).position);
    }

    #[test]
    fn pausing_is_declared_by_the_player_rather_than_assumed() {
        let cannot = properties(vec![("CanPause", OwnedValue::from(false))]);
        assert!(!declare_capabilities(&cannot, &Properties::new()).paused);

        let can = properties(vec![("CanPause", OwnedValue::from(true))]);
        assert!(declare_capabilities(&can, &Properties::new()).paused);
    }

    #[test]
    fn every_mpris_source_can_carry_a_duration() {
        // A length belongs to the MPRIS metadata schema, so the method can
        // always carry one. A track that omits mpris:length is NotReported, not
        // Unsupported, and that is the distinction Known exists to draw.
        assert!(declare_capabilities(&Properties::new(), &Properties::new()).duration);
    }

    #[test]
    fn a_location_is_declared_by_publishing_one_at_all() {
        // Observed live: mpv publishes xesam:url for everything it opens, and
        // Chromium and Feishin publish none. A remote address is a location as
        // much as a path is, so both declare the capability. Reading the scheme
        // here instead would make the declaration change with the track.
        let local = properties(vec![(
            "xesam:url",
            a_string("file:///anime/show%20-%2003.mkv"),
        )]);
        assert!(declare_capabilities(&Properties::new(), &local).location);

        let remote = properties(vec![(
            "xesam:url",
            a_string("https://example.invalid/s.m3u8"),
        )]);
        assert!(declare_capabilities(&Properties::new(), &remote).location);

        let absent = properties(vec![("xesam:title", a_string("Some Streaming Site"))]);
        assert!(!declare_capabilities(&Properties::new(), &absent).location);
    }

    /// A player's properties with the given metadata nested inside, as one
    /// `GetAll` on the Player interface returns them.
    fn a_player(pairs: Vec<(&str, OwnedValue)>, metadata: Vec<(&str, OwnedValue)>) -> Properties {
        let mut player = properties(pairs);
        player.insert(
            "Metadata".to_owned(),
            OwnedValue::from(properties(metadata)),
        );
        player
    }

    #[test]
    fn a_reading_is_timed_by_the_clock_the_watcher_was_given() {
        // The clock is a dependency so that twenty seconds of playback cost no
        // time in a test. This is the seam: nothing in the translation reads a
        // real clock, so the moment a reading carries is the one handed to it.
        let player = a_player(
            vec![("Position", OwnedValue::from(0_i64))],
            vec![("xesam:url", a_string("file:///anime/ep.mkv"))],
        );
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(20));

        let snapshot = observe(&id("mpv"), &player, clock.now())
            .reading
            .expect("a reading");

        assert_eq!(snapshot.observed_at, clock.now());
        assert_eq!(
            snapshot.observed_at.since(Timestamp::epoch()),
            Duration::from_secs(20)
        );
    }

    #[test]
    fn a_length_is_checked_against_the_position_the_reading_publishes() {
        // The two guards are wired together here and nowhere else: a length is
        // discarded for contradicting the position that this snapshot carries,
        // not some other reading of the same map.
        let player = a_player(
            vec![("Position", OwnedValue::from(30_000_000_i64))],
            vec![
                ("xesam:url", a_string("file:///anime/ep.mkv")),
                ("mpris:length", OwnedValue::from(10_000_000_i64)),
            ],
        );

        let snapshot = observe(&id("mpv"), &player, Timestamp::epoch())
            .reading
            .expect("a reading");

        assert_eq!(snapshot.position, Known::Value(Duration::from_secs(30)));
        assert_eq!(snapshot.duration, Known::NotReported);
    }

    #[test]
    fn a_source_with_nothing_open_produces_no_reading_and_is_still_described() {
        // Chromium idle publishes properties but no metadata. A source with
        // nothing open contributes no snapshot rather than one naming nothing,
        // and is described all the same: policy is keyed on the application, so
        // a source the round does not name is one the daemon cannot explain
        // itself about.
        let player = a_player(vec![("Position", OwnedValue::from(0_i64))], vec![]);

        let observed = observe(&id("chromium"), &player, Timestamp::epoch());

        assert_eq!(observed.reading, None);
        assert_eq!(observed.source.player, id("chromium"));
        assert_eq!(observed.source.app, AppName("chromium".to_owned()));
    }

    #[test]
    fn a_position_of_zero_is_the_start_of_a_file_and_not_an_absence() {
        // Observed on mpv 0.41.0 paused at the beginning: Position is x 0. Zero
        // is a reading. Treating it as absence would leave the opening of every
        // file unobserved, and it is why position and length are guarded
        // differently rather than by one rule.
        let player = properties(vec![("Position", OwnedValue::from(0_i64))]);

        assert_eq!(position_of(&player, true), Known::Value(Duration::ZERO));
    }

    #[test]
    fn a_negative_position_is_not_reported() {
        let player = properties(vec![("Position", OwnedValue::from(-1_i64))]);

        assert_eq!(position_of(&player, true), Known::NotReported);
    }

    #[test]
    fn a_position_from_a_source_that_cannot_report_one_is_unsupported() {
        let player = properties(vec![("Position", OwnedValue::from(5_000_000_i64))]);

        assert_eq!(position_of(&player, false), Known::Unsupported);
    }

    #[test]
    fn a_zero_length_is_absent_rather_than_zero() {
        let metadata = properties(vec![("mpris:length", OwnedValue::from(0_i64))]);

        assert_eq!(
            duration_of(&metadata, Known::NotReported, true),
            Known::NotReported
        );
    }

    #[test]
    fn a_negative_length_is_absent() {
        let metadata = properties(vec![("mpris:length", OwnedValue::from(-1_i64))]);

        assert_eq!(
            duration_of(&metadata, Known::NotReported, true),
            Known::NotReported
        );
    }

    #[test]
    fn a_length_below_the_reported_position_is_discarded() {
        // Players lie about length. A file cannot be shorter than how far into
        // it playback has reached, and nothing downstream could tell such a
        // reading from a genuinely short file.
        let metadata = properties(vec![("mpris:length", OwnedValue::from(10_000_000_i64))]);
        let position = Known::Value(Duration::from_secs(30));

        assert_eq!(duration_of(&metadata, position, true), Known::NotReported);
    }

    #[test]
    fn a_length_equal_to_the_position_is_kept() {
        // The last microsecond of a file is a legitimate reading, so the guard
        // discards a length below the position and not one that equals it.
        let metadata = properties(vec![("mpris:length", OwnedValue::from(30_000_000_i64))]);
        let position = Known::Value(Duration::from_secs(30));

        assert_eq!(
            duration_of(&metadata, position, true),
            Known::Value(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_missing_length_from_a_capable_source_is_not_reported() {
        // The player did not say. That is a different fact from the method
        // being unable to carry a length, and the two must stay distinguishable.
        assert_eq!(
            duration_of(&Properties::new(), Known::NotReported, true),
            Known::NotReported
        );
    }

    #[test]
    fn a_length_from_an_incapable_source_is_unsupported() {
        let metadata = properties(vec![("mpris:length", OwnedValue::from(23_000_000_i64))]);

        assert_eq!(
            duration_of(&metadata, Known::NotReported, false),
            Known::Unsupported
        );
    }

    #[test]
    fn a_shift_jis_filename_survives_percent_decoding_byte_for_byte() {
        // Taken from mpv 0.41.0 on 2026-09-16, which published this URL for a
        // file whose name is not valid UTF-8. The same name in xesam:title came
        // back as U+FFFD replacement characters.
        let decoded = percent_decode("/anime/%83%5C%83%8C%83b%83%5E%20-%2003.oga");

        assert_eq!(
            decoded,
            b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.oga".to_vec()
        );
    }

    #[test]
    fn a_windows_1251_filename_survives_percent_decoding_byte_for_byte() {
        let decoded = percent_decode("/anime/%CF%F0%E8%EA%EB%FE%F7%E5%ED%E8%FF.mkv");

        assert_eq!(
            decoded,
            b"/anime/\xcf\xf0\xe8\xea\xeb\xfe\xf7\xe5\xed\xe8\xff.mkv".to_vec()
        );
    }

    #[test]
    fn an_incomplete_escape_is_carried_through_rather_than_failing() {
        // A player emitting a bare percent is naming a file with a percent in
        // it. Refusing the reading over one character would lose the file.
        assert_eq!(percent_decode("/50%/a%2"), b"/50%/a%2".to_vec());
        assert_eq!(percent_decode("/a%zz"), b"/a%zz".to_vec());
    }

    #[test]
    fn a_file_url_becomes_a_local_path() {
        let metadata = properties(vec![("xesam:url", a_string("file:///anime/ep%2003.mkv"))]);

        assert_eq!(
            media_from(&metadata),
            Some(MediaRef::LocalFile(RawPath::from_bytes(
                b"/anime/ep 03.mkv".to_vec()
            )))
        );
    }

    #[test]
    fn a_url_of_another_scheme_becomes_a_remote_reference() {
        // Observed on mpv 0.41.0 opened on an HTTP address. The address is what
        // recognition would work from, so it is carried rather than discarded.
        let address = "http://127.0.0.1:34715/remote-episode.oga";
        let metadata = properties(vec![("xesam:url", a_string(address))]);

        assert_eq!(
            media_from(&metadata),
            Some(MediaRef::Remote(address.to_owned()))
        );
    }

    #[test]
    fn a_file_url_naming_a_host_is_not_a_local_path() {
        // A remainder that does not begin with a slash carries an authority, so
        // the path is on another machine. No observed player emits one, and
        // guessing that it is local would produce a path that opens nothing.
        let metadata = properties(vec![("xesam:url", a_string("file://server/share/ep.mkv"))]);

        assert_eq!(
            media_from(&metadata),
            Some(MediaRef::Remote("file://server/share/ep.mkv".to_owned()))
        );
    }

    #[test]
    fn a_source_with_no_url_falls_back_to_its_title() {
        let metadata = properties(vec![("xesam:title", a_string("Episode 3 - Some Site"))]);

        assert_eq!(
            media_from(&metadata),
            Some(MediaRef::Title("Episode 3 - Some Site".to_owned()))
        );
    }

    #[test]
    fn a_source_with_nothing_open_reports_no_media() {
        // Chromium with nothing playing publishes no metadata at all. A source
        // with nothing open contributes no reading, rather than a reading whose
        // media reference is empty.
        assert_eq!(media_from(&Properties::new()), None);

        let blank = properties(vec![
            ("xesam:url", a_string("")),
            ("xesam:title", a_string("")),
        ]);
        assert_eq!(media_from(&blank), None);
    }

    #[test]
    fn a_url_of_no_characters_is_not_a_location() {
        // The contract requires a reported path or address to be non-empty, so
        // a source publishing the key with nothing in it must not be declared
        // able to name what it opened. Testing the key alone would promise a
        // reading that cannot arrive.
        let empty = properties(vec![("xesam:url", a_string(""))]);

        assert!(!declare_capabilities(&Properties::new(), &empty).location);
    }
}
