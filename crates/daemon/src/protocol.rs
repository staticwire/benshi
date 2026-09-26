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
use benshi_core::recognise::altname::Assignment;
use benshi_core::recognise::parse::{Episode, Parsed};
use benshi_core::recognise::{Recognition, Score, Stage};
use benshi_core::{AppName, Capabilities, MediaRef, PlayState, PlayerId};
use benshi_detect::SourceInfo;
use serde::{Deserialize, Serialize};

use crate::bus::BusEvent;
use crate::recognition::Decision;

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

    /// The most recent recognition decision, and what it was decided by.
    Why,
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
///
/// `PartialEq` and not `Eq`, because an explanation carries a score as a
/// floating-point number, and those have no total equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

    /// The most recent decision, in answer to [`Request::Why`], and nothing
    /// where no reading has been decided yet.
    Why(Option<Explanation>),
}

/// The most recent decision, as a client is told it.
///
/// Whose file, which file, what its name spelled and what recognition
/// answered. The parse travels whole rather than as a title, because the
/// season and the part are what the parser cut out of the title, and they are
/// what an explanation of an assignment has to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// The source whose reading was decided.
    pub player: PlayerId,
    /// What it had open.
    pub media: MediaRef,
    /// What the name spelled, before anything was matched.
    pub spelled: Parsed,
    /// What recognition answered.
    pub outcome: Outcome,
}

impl Explanation {
    /// A decision, as a client is told it.
    #[must_use]
    pub fn of(decision: &Decision) -> Self {
        Self {
            player: decision.player.clone(),
            media: decision.media.clone(),
            spelled: decision.parsed.clone(),
            outcome: Outcome::of(&decision.answer),
        }
    }
}

/// What recognition answered, on the wire.
///
/// A form of its own rather than the domain's, because a score is a checked
/// proportion that deliberately does not deserialise: a message carrying a
/// score of five must not be able to become one. Here a score is a number, and
/// a consumer that needs the checked kind converts through the check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// A title, an episode and the stage that decided.
    Recognised {
        /// The title as the corpus spells it.
        title: String,
        /// The episode in the release's numbering, as the name spelled it.
        episode: Episode,
        /// Which stage decided, and what it decided by.
        stage: DecidedBy,
    },
    /// Several titles the name reaches, and nothing in it to choose between.
    Ambiguous {
        /// The entries it reaches, as the corpus spells them.
        candidates: Vec<String>,
    },
    /// Nothing matched.
    Unrecognised {
        /// How close the best candidate came, where there was one to compare.
        best: Option<f64>,
    },
}

impl Outcome {
    fn of(answer: &Recognition) -> Self {
        match answer {
            Recognition::Recognised(found) => Self::Recognised {
                title: found.title.clone(),
                episode: found.episode,
                stage: DecidedBy::of(&found.stage),
            },
            Recognition::Ambiguous(ambiguity) => Self::Ambiguous {
                candidates: ambiguity.candidates.clone(),
            },
            Recognition::Unrecognised(refusal) => Self::Unrecognised {
                best: refusal.best.map(Score::value),
            },
        }
    }
}

/// The stage that decided, and what it decided by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DecidedBy {
    /// The assignment the user made, as they made it.
    Altname(Assignment),
    /// An exact match on the normalised key.
    Key,
    /// The best candidate the scorer found, and how well it scored.
    Scored(f64),
}

impl DecidedBy {
    fn of(stage: &Stage) -> Self {
        match stage {
            Stage::Altname(assignment) => Self::Altname(assignment.clone()),
            Stage::Key => Self::Key,
            Stage::Scored(score) => Self::Scored(score.value()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DecidedBy, Explanation, Outcome, Request, Response, SourceListing};
    use crate::bus::BusEvent;
    use crate::recognition::{Decision, Recogniser};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::Policy;
    use benshi_core::recognise::altname::Assignment;
    use benshi_core::recognise::parse::Episode;
    use benshi_core::recognise::{Ambiguity, Match, Recognition, Refusal, Score, Stage};
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

    /// A decision about the file [`a_snapshot`] has open, answered this way.
    ///
    /// Parsed the way a round parses it, so that what the tests explain is
    /// what the daemon would.
    fn a_decision(answer: Recognition) -> Decision {
        let snapshot = a_snapshot();
        let MediaRef::LocalFile(path) = &snapshot.media else {
            panic!("the snapshot has a file open");
        };
        let (parsed, _refused) = Recogniser::empty().decide(path);

        Decision {
            player: snapshot.player.clone(),
            media: snapshot.media.clone(),
            parsed,
            answer,
        }
    }

    fn an_assignment() -> Assignment {
        Assignment {
            spelled: "SnK".to_owned(),
            season: Some(3),
            part: None,
            title: "Shingeki no Kyojin".to_owned(),
        }
    }

    /// A match of this title by this stage, for building decisions from.
    fn found_by(stage: Stage) -> Recognition {
        Recognition::Recognised(Match {
            title: "Shingeki no Kyojin".to_owned(),
            episode: Episode::Only(3),
            stage,
        })
    }

    /// One of every answer recognition gives, so that an explanation of each
    /// is on the wire below.
    fn every_recognition() -> Vec<Recognition> {
        vec![
            found_by(Stage::Altname(an_assignment())),
            found_by(Stage::Key),
            found_by(Stage::Scored(Score::new(0.83).expect("a proportion"))),
            Recognition::Ambiguous(Ambiguity {
                parsed: "Fruits Basket".to_owned(),
                candidates: vec![
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned(),
                ],
            }),
            Recognition::Unrecognised(Refusal {
                parsed: "Some Other Show".to_owned(),
                best: Some(Score::new(0.4).expect("a proportion")),
            }),
            Recognition::Unrecognised(Refusal {
                parsed: String::new(),
                best: None,
            }),
        ]
    }

    /// One of every response, so that a check over all of them cannot miss one.
    fn every_response() -> Vec<Response> {
        let mut responses = vec![
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
            Response::Why(None),
        ];
        responses.extend(
            every_recognition()
                .into_iter()
                .map(|answer| Response::Why(Some(Explanation::of(&a_decision(answer))))),
        );

        responses
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
            Request::Why,
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
    fn an_explanation_says_whose_file_it_is_and_what_the_name_spelled() {
        // The three things a person asking why needs before the answer: which
        // player, which file, and what the parser made of its name. The parse
        // travels whole rather than as a title, because a season the parser
        // cut out of the title is exactly what an explanation has to show.
        let explained = Explanation::of(&a_decision(found_by(Stage::Key)));

        assert_eq!(explained.player, a_snapshot().player);
        assert_eq!(explained.media, a_snapshot().media);
        assert_eq!(explained.spelled.episode, Episode::Only(3));
        assert_eq!(explained.spelled.title.as_deref(), Some("ソレッタ"));
    }

    #[test]
    fn an_explanation_carries_the_assignment_a_match_was_decided_by() {
        // What a user who made an assignment sees back: the text they gave
        // and the season it was bound to, which is why a file of another
        // season did not follow it.
        let explained = Explanation::of(&a_decision(found_by(Stage::Altname(an_assignment()))));

        assert_eq!(
            explained.outcome,
            Outcome::Recognised {
                title: "Shingeki no Kyojin".to_owned(),
                episode: Episode::Only(3),
                stage: DecidedBy::Altname(an_assignment()),
            }
        );
    }

    #[test]
    fn an_explanation_of_a_score_carries_the_score_it_won_with() {
        let explained = Explanation::of(&a_decision(found_by(Stage::Scored(
            Score::new(0.83).expect("a proportion"),
        ))));

        assert_eq!(
            explained.outcome,
            Outcome::Recognised {
                title: "Shingeki no Kyojin".to_owned(),
                episode: Episode::Only(3),
                stage: DecidedBy::Scored(0.83),
            }
        );
    }

    #[test]
    fn an_explanation_of_an_ambiguity_names_every_candidate() {
        let explained = Explanation::of(&a_decision(Recognition::Ambiguous(Ambiguity {
            parsed: "Fruits Basket".to_owned(),
            candidates: vec![
                "Fruits Basket".to_owned(),
                "Fruits Basket (2019)".to_owned(),
            ],
        })));

        assert_eq!(
            explained.outcome,
            Outcome::Ambiguous {
                candidates: vec![
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned(),
                ],
            }
        );
    }

    #[test]
    fn an_explanation_of_a_refusal_carries_how_close_the_best_came() {
        // Absent and low are different answers, and they stay different on
        // the wire: one points at the list and the other at the name.
        let close = Explanation::of(&a_decision(Recognition::Unrecognised(Refusal {
            parsed: "Some Other Show".to_owned(),
            best: Some(Score::new(0.4).expect("a proportion")),
        })));
        let nothing = Explanation::of(&a_decision(Recognition::Unrecognised(Refusal {
            parsed: "Some Other Show".to_owned(),
            best: None,
        })));

        assert_eq!(close.outcome, Outcome::Unrecognised { best: Some(0.4) });
        assert_eq!(nothing.outcome, Outcome::Unrecognised { best: None });
    }

    #[test]
    fn a_malformed_line_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Request>("not json at all").is_err());
        assert!(serde_json::from_str::<Request>("{}").is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"Nonsense"}"#).is_err());
    }
}
