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

use benshi_core::{AppName, Capabilities, PlayState, PlayerId};
use futures_util::future::join_all;
use tokio::time::timeout;
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::InterfaceName;
use zbus::proxy::CacheProperties;
use zbus::zvariant::OwnedValue;

use crate::{SourceInfo, WatchError};

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
/// Three of the four are stable facts about the source. `file_path` is not, and
/// cannot be: MPRIS offers no way for a player to declare that it will name a
/// file, so the declaration is re-derived from what the source has open at each
/// listing. A player that switches between a local file and a stream therefore
/// changes this capability, which is the one place this adapter reports a fact
/// about the moment rather than about the source.
fn declare_capabilities(player: &Properties, metadata: &Properties) -> Capabilities {
    Capabilities {
        position: player.contains_key("Position"),
        // A length belongs to the MPRIS metadata schema, so the method can
        // always carry one. A track that omits `mpris:length` is NotReported
        // rather than Unsupported, which is the distinction `Known` exists for.
        duration: true,
        paused: boolean(player, "CanPause"),
        file_path: text(metadata, "xesam:url")
            .is_some_and(|url| url.starts_with(LOCAL_FILE_SCHEME)),
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

/// Every MPRIS player on the session bus.
#[derive(Debug)]
pub struct MprisWatcher {
    connection: zbus::Connection,
    deadline: Duration,
}

impl MprisWatcher {
    /// Open a connection to the session bus.
    ///
    /// The deadline applies to this call and to every later call on one source.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] when there is no session bus to reach,
    /// or when it does not answer within `deadline`.
    pub async fn connect(deadline: Duration) -> Result<Self, WatchError> {
        let connection = bus_call(deadline, zbus::Connection::session()).await?;

        Ok(Self {
            connection,
            deadline,
        })
    }

    /// Every source the session bus currently carries.
    ///
    /// Sources are queried together rather than in turn, so one player that
    /// accepts a call and never answers costs one deadline and not one each.
    /// Such a player is omitted from the listing: `sources` has no channel for
    /// a per-source failure, and a listing that fails wholesale because one
    /// player hung would blind the daemon to every other.
    ///
    /// The listing is sorted by identity so that two calls with nothing changed
    /// return the same thing in the same order.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Transport`] when the bus itself cannot be listed.
    pub async fn sources(&self) -> Result<Vec<SourceInfo>, WatchError> {
        let bus = DBusProxy::new(&self.connection).await.map_err(transport)?;

        let names = bus_call(self.deadline, bus.list_names()).await?;

        let described = names
            .iter()
            .filter_map(|name| identity_from_bus_name(name))
            .map(|identity| self.describe(identity));

        let mut sources: Vec<SourceInfo> =
            join_all(described).await.into_iter().flatten().collect();
        sources.sort_by(|left, right| left.player.cmp(&right.player));

        Ok(sources)
    }

    /// Describe one source, or nothing if it did not answer.
    async fn describe(&self, identity: PlayerId) -> Option<SourceInfo> {
        let player = self.player_properties(&identity).await.ok()?;
        let metadata = metadata_of(&player);

        Some(SourceInfo {
            app: app_from_identity(&identity),
            capabilities: declare_capabilities(&player, &metadata),
            state: play_state(text(&player, "PlaybackStatus")),
            player: identity,
        })
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

#[cfg(test)]
mod tests {
    use super::{
        PLAYER_INTERFACE, Properties, app_from_identity, declare_capabilities,
        identity_from_bus_name, play_state,
    };
    use benshi_core::{AppName, PlayState, PlayerId};
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
    fn a_path_is_declared_only_where_the_source_names_a_local_file() {
        // Observed live: mpv publishes xesam:url for a local file, chromium and
        // Feishin publish no xesam:url at all. A player streaming over HTTP
        // publishes one that names no file, and declaring a path for it would
        // promise a filename that never arrives.
        let local = properties(vec![(
            "xesam:url",
            a_string("file:///anime/show%20-%2003.mkv"),
        )]);
        assert!(declare_capabilities(&Properties::new(), &local).file_path);

        let remote = properties(vec![(
            "xesam:url",
            a_string("https://example.invalid/s.m3u8"),
        )]);
        assert!(!declare_capabilities(&Properties::new(), &remote).file_path);

        let absent = properties(vec![("xesam:title", a_string("Some Streaming Site"))]);
        assert!(!declare_capabilities(&Properties::new(), &absent).file_path);
    }
}
