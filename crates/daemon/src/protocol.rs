//! What a client may ask the daemon, and what the daemon answers.
//!
//! Newline-delimited JSON: one request per line, one or more answers per line.
//! The format is chosen for the moment something is wrong, when `socat -
//! UNIX-CONNECT:...` has to be enough to see what the daemon is saying. A
//! binary encoding would be smaller and would need our own code to read, which
//! is exactly what is unavailable when our own code is the suspect.
//!
//! Every message is a JSON object carrying a `type`, so a reader that knows
//! nothing about these types can still tell one message from another, and a
//! value that is not an object cannot be mistaken for a complete message.

use benshi_core::policy::Policy;
use benshi_core::{AppName, Capabilities, PlayState, PlayerId};
use benshi_detect::SourceInfo;
use serde::{Deserialize, Serialize};

use crate::bus::BusEvent;

/// One request from a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum Request {
    /// Every source the daemon has seen, with its policy and capabilities.
    Sources,

    /// Every event from now on, until the connection closes.
    ///
    /// The last request on a connection: what follows is a stream rather than
    /// an answer, and a client that wants to ask something else opens another
    /// connection.
    Watch,

    /// Set the policy for an application.
    SetPolicy {
        /// Which application.
        ///
        /// The application and not a source's identity. Two windows of one
        /// player are two identities and one application, and an identity that
        /// carries a process id does not survive the player restarting.
        app: AppName,
        /// What to do with readings from it.
        policy: Policy,
    },
}

/// One source as a listing shows it.
///
/// The source as the platform describes it, with the policy in force for it.
/// The two travel together because the listing exists to answer "why is my
/// player being ignored", and an answer assembled from two calls could show a
/// policy that was no longer the one being applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceListing {
    /// The stable identity the platform provides, unique per instance.
    pub player: PlayerId,
    /// The application it belongs to, which policy is keyed on.
    pub app: AppName,
    /// What the daemon does with readings from it.
    pub policy: Policy,
    /// What it is able to report.
    pub capabilities: Capabilities,
    /// What it was doing when the daemon last looked.
    pub state: PlayState,
}

impl SourceListing {
    /// One source, with the policy in force for its application.
    #[must_use]
    pub fn of(source: &SourceInfo, policy: Policy) -> Self {
        Self {
            player: source.player.clone(),
            app: source.app.clone(),
            policy,
            capabilities: source.capabilities,
            state: source.state,
        }
    }
}

/// One line of answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "body")]
pub enum Response {
    /// Every source, in answer to [`Request::Sources`].
    Sources(Vec<SourceListing>),

    /// One event, in answer to [`Request::Watch`].
    Event(BusEvent),

    /// Events were dropped because this client was not reading fast enough.
    ///
    /// Said rather than passed over. A gap nobody reports reads as nothing
    /// having happened, which on a diagnostic stream is the one reading that
    /// must never be given by accident.
    Lagged {
        /// How many events were dropped.
        missed: u64,
    },

    /// The request was carried out and has nothing to return.
    Ok,

    /// The request could not be carried out.
    Error {
        /// What went wrong, rendered for a person.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{Request, Response, SourceListing};
    use crate::bus::BusEvent;
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::Policy;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot,
    };
    use benshi_detect::SourceInfo;
    use std::time::Duration;

    /// A filename in Shift-JIS, which is not valid UTF-8.
    ///
    /// The case a wire format is most likely to get wrong, and the one a user
    /// with a Japanese archive hits first.
    const SHIFT_JIS_NAME: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.mkv";

    fn a_source() -> SourceInfo {
        SourceInfo {
            player: PlayerId("mpv.instance1701".to_owned()),
            app: AppName("mpv".to_owned()),
            capabilities: Capabilities {
                position: true,
                duration: true,
                paused: true,
                location: true,
            },
            state: PlayState::Playing,
        }
    }

    fn a_snapshot() -> PlayerSnapshot {
        PlayerSnapshot {
            player: PlayerId("mpv.instance1701".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_micros(93_456_789)),
            duration: Known::NotReported,
            observed_at: Timestamp::epoch(),
        }
    }

    /// One of every response, so that a check over all of them cannot miss one.
    fn every_response() -> Vec<Response> {
        vec![
            Response::Sources(vec![SourceListing::of(&a_source(), Policy::Auto)]),
            Response::Sources(Vec::new()),
            Response::Event(BusEvent::Snapshot(a_snapshot())),
            Response::Event(BusEvent::SourcesChanged {
                sources: vec![PlayerId("mpv".to_owned())],
            }),
            Response::Lagged { missed: 17 },
            Response::Ok,
            Response::Error {
                message: "expected value at line 1 column 1".to_owned(),
            },
        ]
    }

    /// One of every request.
    fn every_request() -> Vec<Request> {
        vec![
            Request::Sources,
            Request::Watch,
            Request::SetPolicy {
                app: AppName("firefox".to_owned()),
                policy: Policy::Allow,
            },
        ]
    }

    /// Every message either side may put on the wire, serialised.
    ///
    /// Both directions, because the rules below are about the transport and
    /// hold in both. A check written over one of them would say "every message"
    /// and mean half of them.
    fn every_message() -> Vec<String> {
        let mut lines = Vec::new();

        for request in every_request() {
            lines.push(serde_json::to_string(&request).expect("serialise"));
        }
        for response in every_response() {
            lines.push(serde_json::to_string(&response).expect("serialise"));
        }

        lines
    }

    #[test]
    fn every_message_is_one_json_object_on_one_line() {
        // The transport is newline-delimited, so a message that spans lines
        // would be read as two, and a message that is not an object cannot be
        // told from a fragment. This is also what makes the socket readable
        // with socat and nothing else.
        for line in every_message() {
            assert!(line.starts_with('{'), "not an object: {line}");
            assert!(line.ends_with('}'), "not an object: {line}");
            assert!(!line.contains('\n'), "more than one line: {line}");
        }
    }

    #[test]
    fn every_message_names_its_kind() {
        // A reader that does not know our types still has to be able to tell
        // one message from another, which a bare value would not allow.
        for line in every_message() {
            assert!(line.contains("\"type\""), "no kind: {line}");
        }
    }

    #[test]
    fn a_request_read_back_is_the_request_that_was_sent() {
        for request in every_request() {
            let text = serde_json::to_string(&request).expect("serialise");
            let back: Request = serde_json::from_str(&text).expect("deserialise");

            assert_eq!(request, back);
        }
    }

    #[test]
    fn a_response_read_back_is_the_response_that_was_sent() {
        for response in every_response() {
            let text = serde_json::to_string(&response).expect("serialise");
            let back: Response = serde_json::from_str(&text).expect("deserialise");

            assert_eq!(response, back);
        }
    }

    #[test]
    fn a_reading_of_a_filename_that_is_not_utf8_survives_the_wire() {
        // A name out of a Japanese archive unpacked with the wrong encoding is
        // not valid UTF-8, and it is the reading a user is most likely to be
        // watching when something goes wrong. Replacing the bytes it cannot
        // read would make the file unrecognisable and the stream a lie.
        let published = BusEvent::Snapshot(a_snapshot());

        let text = serde_json::to_string(&Response::Event(published.clone())).expect("serialise");
        let back: Response = serde_json::from_str(&text).expect("deserialise");

        let Response::Event(BusEvent::Snapshot(snapshot)) = back else {
            panic!("expected a reading, got {back:?}");
        };
        match snapshot.media {
            MediaRef::LocalFile(path) => assert_eq!(path.as_bytes(), SHIFT_JIS_NAME),
            other => panic!("expected a file, got {other:?}"),
        }
    }

    #[test]
    fn a_listing_carries_the_policy_in_force_beside_the_source() {
        // The listing exists to answer "why is my player being ignored", so the
        // policy and the source have to arrive together. Two calls would let
        // them disagree.
        let listing = SourceListing::of(&a_source(), Policy::Deny);

        assert_eq!(listing.player, PlayerId("mpv.instance1701".to_owned()));
        assert_eq!(listing.app, AppName("mpv".to_owned()));
        assert_eq!(listing.policy, Policy::Deny);
        assert_eq!(listing.capabilities, a_source().capabilities);
        assert_eq!(listing.state, PlayState::Playing);
    }

    #[test]
    fn a_malformed_line_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Request>("not json at all").is_err());
        assert!(serde_json::from_str::<Request>("{}").is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"Nonsense"}"#).is_err());
    }
}
