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
use benshi_core::recognize::altname::Assignment;
use benshi_core::recognize::parse::{Episode, Parsed};
use benshi_core::recognize::{Recognition, Score, Stage};
use benshi_core::settle::{Fact, Settled};
use benshi_core::token::Token;
use benshi_core::{AppName, Capabilities, MediaRef, PlayState, PlayerId, ShowId};
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
    /// connection. It is answered with [`Response::Ok`] once the stream is on,
    /// and the events follow: a quiet daemon sends none for minutes, and the
    /// answer is what tells it from one that will never answer.
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

    /// Check a token with AniList and keep it in the secret store.
    ///
    /// Answered twice: with [`Response::Ok`] once it is read, and with
    /// [`Response::LoggedIn`] or [`Response::Error`] once it is done. The
    /// store may ask a person to unlock it and the second answer waits for
    /// them, so the first is what tells a daemon at work from one that will
    /// never answer.
    Login {
        /// The token, as AniList's page gives it.
        token: Token,
    },

    /// Remove the token kept in the secret store.
    ///
    /// Answered twice, as [`Request::Login`] is: with [`Response::Ok`], and
    /// with [`Response::LoggedOut`] or [`Response::Error`].
    Logout,
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

    /// The request was carried out and has nothing to return. In answer to
    /// [`Request::Watch`], that the stream is on and what follows is it; to
    /// [`Request::Login`] and [`Request::Logout`], that the request is read
    /// and what it comes to follows.
    Ok,

    /// The request could not be carried out.
    Error {
        /// What went wrong, rendered for a person.
        message: String,
    },

    /// The most recent decision, in answer to [`Request::Why`], and nothing
    /// where no reading has been decided yet.
    Why(Option<Box<Explanation>>),

    /// Whose the token is, in answer to [`Request::Login`], once it is kept.
    LoggedIn {
        /// The name of the account on AniList.
        name: String,
        /// When the token runs out, in seconds since the Unix epoch, and none
        /// where the token does not say.
        expires: Option<u64>,
    },

    /// The token is no longer kept, in answer to [`Request::Logout`].
    LoggedOut,
}

/// The most recent decision, as a client is told it.
///
/// Whose file, which file, what its name spelled, what recognition answered
/// or the show kept in its place, and what the facts made of it: the fact
/// that refused the file, or the entries refused and the one chosen of
/// several. The parse travels whole rather than as a title, because the
/// season and the part are what the parser cut out of the title, and they
/// are what an explanation of an assignment has to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Explanation {
    /// The source whose reading was decided.
    pub player: PlayerId,
    /// What it had open.
    pub media: MediaRef,
    /// What the name spelled, in the reading the answer was decided by.
    pub spelled: Parsed,
    /// What recognition answered, or the show kept for a name it refused that
    /// spells a title.
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
            outcome: Outcome::of(&decision.answer, &decision.settled),
        }
    }
}

/// What recognition answered, or the show kept in its place, on the wire.
///
/// A form of its own rather than the domain's, because a score is a checked
/// proportion that deliberately does not deserialize: a message carrying a
/// score of five must not be able to become one. Here a score is a number, and
/// a consumer that needs the checked kind converts through the check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// A title, an episode, the stage that decided, and the fact of the
    /// entry that refused the file, where one did.
    Recognized {
        /// The title as the corpus spells it, or as the name spelled it where
        /// the stage is [`DecidedBy::Unlisted`].
        title: String,
        /// The episode in the release's numbering, as it was read from the name.
        episode: Episode,
        /// Which stage decided, and what it decided by.
        stage: DecidedBy,
        /// The fact of the entry that refused the file, and none where the
        /// facts let it stand.
        refused: Option<Fact>,
    },
    /// Several entries the name reaches, the ones the facts of their
    /// entries refused, and the one they chose.
    Ambiguous {
        /// The title of each entry it reaches, as the corpus spells it. Two
        /// entries can share one.
        candidates: Vec<String>,
        /// The title of each entry a fact refused, with the fact, in the
        /// order of the candidates.
        refused: Vec<(String, Fact)>,
        /// The title of the entry the facts left alone, with the episode the
        /// name spells, and none where they left none or several.
        chosen: Option<(String, Episode)>,
    },
    /// Nothing matched.
    Unrecognized {
        /// How close the best candidate came, where there was one to compare.
        best: Option<f64>,
    },
}

impl Outcome {
    fn of(answer: &Recognition, settled: &Settled) -> Self {
        match answer {
            Recognition::Recognized(found) => Self::Recognized {
                title: found.title.clone(),
                episode: found.episode,
                stage: DecidedBy::of(&found.stage),
                refused: match settled {
                    Settled::Refused { fact, .. } => Some(*fact),
                    Settled::Stands { .. }
                    | Settled::Chosen { .. }
                    | Settled::Undecided { .. }
                    | Settled::Unnamed => None,
                },
            },
            Recognition::Ambiguous(ambiguity) => {
                let title_of = |show: ShowId| {
                    ambiguity
                        .candidates
                        .iter()
                        .find(|candidate| candidate.show == show)
                        .map(|candidate| candidate.title.clone())
                        .unwrap_or_default()
                };
                let (refused, chosen) = match settled {
                    Settled::Chosen {
                        show,
                        episode,
                        refused,
                    } => (refused.as_slice(), Some((title_of(*show), *episode))),
                    Settled::Undecided { refused } => (refused.as_slice(), None),
                    Settled::Stands { .. } | Settled::Refused { .. } | Settled::Unnamed => {
                        (&[][..], None)
                    }
                };

                Self::Ambiguous {
                    candidates: ambiguity
                        .candidates
                        .iter()
                        .map(|candidate| candidate.title.clone())
                        .collect(),
                    refused: refused
                        .iter()
                        .map(|&(show, fact)| (title_of(show), fact))
                        .collect(),
                    chosen,
                }
            }
            Recognition::Unrecognized(refusal) => Self::Unrecognized {
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
    /// An exact match on the normalized key.
    Key,
    /// The best candidate the scorer found, and how well it scored.
    Scored(f64),
    /// A name the list does not hold, kept as a show of its own.
    Unlisted,
}

impl DecidedBy {
    fn of(stage: &Stage) -> Self {
        match stage {
            Stage::Altname(assignment) => Self::Altname(assignment.clone()),
            Stage::Key => Self::Key,
            Stage::Scored(score) => Self::Scored(score.value()),
            Stage::Unlisted => Self::Unlisted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DecidedBy, Explanation, Outcome, Request, Response, SourceListing};
    use crate::bus::BusEvent;
    use crate::recognition::{Decision, Recognizer};
    use benshi_core::clock::Timestamp;
    use benshi_core::path::RawPath;
    use benshi_core::policy::Policy;
    use benshi_core::recognize::altname::Assignment;
    use benshi_core::recognize::parse::{Episode, Kind};
    use benshi_core::recognize::{Ambiguity, Candidate, Match, Recognition, Refusal, Score, Stage};
    use benshi_core::settle::{Fact, Settled};
    use benshi_core::token::Token;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, ShowId,
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

    /// A decision about the file [`a_snapshot`] has open, answered this way
    /// and refused by no fact.
    ///
    /// Parsed the way a round parses it, so that what the tests explain is
    /// what the daemon would.
    fn a_decision(answer: Recognition) -> Decision {
        let settled = match &answer {
            Recognition::Recognized(found) => Settled::Stands {
                show: found.show,
                episode: found.episode,
            },
            Recognition::Ambiguous(_) | Recognition::Unrecognized(_) => Settled::Unnamed,
        };
        decided_as(answer, settled)
    }

    /// A decision about the file [`a_snapshot`] has open, answered this way
    /// and settled this way.
    fn decided_as(answer: Recognition, settled: Settled) -> Decision {
        let snapshot = a_snapshot();
        let MediaRef::LocalFile(path) = &snapshot.media else {
            panic!("the snapshot has a file open");
        };
        let (parsed, _refused) = Recognizer::empty().decide(path);

        Decision {
            player: snapshot.player.clone(),
            media: snapshot.media.clone(),
            parsed,
            answer,
            settled,
        }
    }

    /// One of each fact that refuses a file.
    fn every_fact() -> [Fact; 6] {
        [
            Fact::NotReleased,
            Fact::Year {
                spelled: 2001,
                began: 2019,
            },
            Fact::Episodes(2),
            Fact::Length {
                file: Duration::from_secs(114),
                episode: Duration::from_mins(40),
            },
            Fact::PastCount {
                episode: 5,
                episodes: 1,
            },
            Fact::Kind(Kind::Extra),
        ]
    }

    fn an_assignment() -> Assignment {
        Assignment {
            spelled: "SnK".to_owned(),
            season: Some(3),
            part: None,
            show: ShowId(1),
            title: "Shingeki no Kyojin".to_owned(),
        }
    }

    /// A match of this title by this stage, for building decisions from.
    fn found_by(stage: Stage) -> Recognition {
        Recognition::Recognized(Match {
            show: ShowId(1),
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
            found_by(Stage::Unlisted),
            Recognition::Ambiguous(Ambiguity {
                parsed: "Fruits Basket".to_owned(),
                candidates: vec![
                    Candidate {
                        show: ShowId(1),
                        title: "Fruits Basket".to_owned(),
                    },
                    Candidate {
                        show: ShowId(2),
                        title: "Fruits Basket (2019)".to_owned(),
                    },
                ],
            }),
            Recognition::Unrecognized(Refusal {
                parsed: "Some Other Show".to_owned(),
                best: Some(Score::new(0.4).expect("a proportion")),
            }),
            Recognition::Unrecognized(Refusal {
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
            Response::LoggedIn {
                name: "someone".to_owned(),
                expires: Some(1_900_000_000),
            },
            Response::LoggedIn {
                name: "someone".to_owned(),
                expires: None,
            },
            Response::LoggedOut,
        ];
        responses.extend(
            every_recognition()
                .into_iter()
                .map(|answer| Response::Why(Some(Box::new(Explanation::of(&a_decision(answer)))))),
        );
        responses.extend(every_fact().into_iter().map(|fact| {
            let refused = decided_as(
                found_by(Stage::Key),
                Settled::Refused {
                    show: ShowId(1),
                    fact,
                },
            );
            Response::Why(Some(Box::new(Explanation::of(&refused))))
        }));
        let chosen = decided_as(
            Recognition::Ambiguous(Ambiguity {
                parsed: "Show Title".to_owned(),
                candidates: vec![
                    Candidate {
                        show: ShowId(1),
                        title: "Show Title".to_owned(),
                    },
                    Candidate {
                        show: ShowId(2),
                        title: "Show Title (2019)".to_owned(),
                    },
                ],
            }),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(3),
                refused: vec![(ShowId(1), Fact::NotReleased)],
            },
        );
        responses.push(Response::Why(Some(Box::new(Explanation::of(&chosen)))));

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
            Request::Login {
                token: Token::new("header.payload.signature"),
            },
            Request::Logout,
        ]
    }

    /// Every message either side may put on the wire, serialized.
    ///
    /// Both directions, because the rules below are about the transport and
    /// hold in both. A check written over one of them would say "every message"
    /// and mean half of them.
    fn every_message() -> Vec<String> {
        let mut lines = Vec::new();

        for request in every_request() {
            lines.push(serde_json::to_string(&request).expect("serialize"));
        }
        for response in every_response() {
            lines.push(serde_json::to_string(&response).expect("serialize"));
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
            let text = serde_json::to_string(&request).expect("serialize");
            let back: Request = serde_json::from_str(&text).expect("deserialize");

            assert_eq!(request, back);
        }
    }

    #[test]
    fn a_response_read_back_is_the_response_that_was_sent() {
        for response in every_response() {
            let text = serde_json::to_string(&response).expect("serialize");
            let back: Response = serde_json::from_str(&text).expect("deserialize");

            assert_eq!(response, back);
        }
    }

    #[test]
    fn a_reading_of_a_filename_that_is_not_utf8_survives_the_wire() {
        // A name out of a Japanese archive unpacked with the wrong encoding is
        // not valid UTF-8, and it is the reading a user is most likely to be
        // watching when something goes wrong. Replacing the bytes it cannot
        // read would make the file unrecognizable and the stream a lie.
        let published = BusEvent::Snapshot(a_snapshot());

        let text = serde_json::to_string(&Response::Event(published.clone())).expect("serialize");
        let back: Response = serde_json::from_str(&text).expect("deserialize");

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
            Outcome::Recognized {
                title: "Shingeki no Kyojin".to_owned(),
                episode: Episode::Only(3),
                stage: DecidedBy::Altname(an_assignment()),
                refused: None,
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
            Outcome::Recognized {
                title: "Shingeki no Kyojin".to_owned(),
                episode: Episode::Only(3),
                stage: DecidedBy::Scored(0.83),
                refused: None,
            }
        );
    }

    #[test]
    fn an_explanation_of_a_file_a_fact_refused_carries_the_fact() {
        // What `benshi why` names a refusal by: the match recognition made,
        // and the fact of its entry that refused the file.
        for fact in every_fact() {
            let explained = Explanation::of(&decided_as(
                found_by(Stage::Key),
                Settled::Refused {
                    show: ShowId(1),
                    fact,
                },
            ));

            assert_eq!(
                explained.outcome,
                Outcome::Recognized {
                    title: "Shingeki no Kyojin".to_owned(),
                    episode: Episode::Only(3),
                    stage: DecidedBy::Key,
                    refused: Some(fact),
                }
            );
        }
    }

    #[test]
    fn an_explanation_of_a_name_the_list_does_not_hold_says_so() {
        let explained = Explanation::of(&a_decision(found_by(Stage::Unlisted)));

        assert_eq!(
            explained.outcome,
            Outcome::Recognized {
                title: "Shingeki no Kyojin".to_owned(),
                episode: Episode::Only(3),
                stage: DecidedBy::Unlisted,
                refused: None,
            }
        );
    }

    #[test]
    fn an_explanation_of_an_ambiguity_names_every_candidate() {
        let explained = Explanation::of(&a_decision(Recognition::Ambiguous(Ambiguity {
            parsed: "Fruits Basket".to_owned(),
            candidates: vec![
                Candidate {
                    show: ShowId(1),
                    title: "Fruits Basket".to_owned(),
                },
                Candidate {
                    show: ShowId(2),
                    title: "Fruits Basket (2019)".to_owned(),
                },
            ],
        })));

        assert_eq!(
            explained.outcome,
            Outcome::Ambiguous {
                candidates: vec![
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned(),
                ],
                refused: Vec::new(),
                chosen: None,
            }
        );
    }

    #[test]
    fn an_explanation_of_an_ambiguity_the_facts_settled_names_what_they_refused_and_chose() {
        let several = || {
            Recognition::Ambiguous(Ambiguity {
                parsed: "Show Title".to_owned(),
                candidates: vec![
                    Candidate {
                        show: ShowId(1),
                        title: "Show Title".to_owned(),
                    },
                    Candidate {
                        show: ShowId(2),
                        title: "Show Title (2019)".to_owned(),
                    },
                    Candidate {
                        show: ShowId(3),
                        title: "Show Title".to_owned(),
                    },
                ],
            })
        };
        let year = Fact::Year {
            spelled: 2019,
            began: 2001,
        };

        let chosen = Explanation::of(&decided_as(
            several(),
            Settled::Chosen {
                show: ShowId(2),
                episode: Episode::Only(5),
                refused: vec![(ShowId(1), year), (ShowId(3), Fact::NotReleased)],
            },
        ));
        let undecided = Explanation::of(&decided_as(
            several(),
            Settled::Undecided {
                refused: vec![(ShowId(3), Fact::NotReleased)],
            },
        ));

        assert_eq!(
            chosen.outcome,
            Outcome::Ambiguous {
                candidates: vec![
                    "Show Title".to_owned(),
                    "Show Title (2019)".to_owned(),
                    "Show Title".to_owned(),
                ],
                refused: vec![
                    ("Show Title".to_owned(), year),
                    ("Show Title".to_owned(), Fact::NotReleased),
                ],
                chosen: Some(("Show Title (2019)".to_owned(), Episode::Only(5))),
            }
        );
        assert_eq!(
            undecided.outcome,
            Outcome::Ambiguous {
                candidates: vec![
                    "Show Title".to_owned(),
                    "Show Title (2019)".to_owned(),
                    "Show Title".to_owned(),
                ],
                refused: vec![("Show Title".to_owned(), Fact::NotReleased)],
                chosen: None,
            }
        );
    }

    #[test]
    fn an_explanation_of_a_refusal_carries_how_close_the_best_came() {
        // Absent and low are different answers, and they stay different on
        // the wire: one points at the list and the other at the name.
        let close = Explanation::of(&a_decision(Recognition::Unrecognized(Refusal {
            parsed: "Some Other Show".to_owned(),
            best: Some(Score::new(0.4).expect("a proportion")),
        })));
        let nothing = Explanation::of(&a_decision(Recognition::Unrecognized(Refusal {
            parsed: "Some Other Show".to_owned(),
            best: None,
        })));

        assert_eq!(close.outcome, Outcome::Unrecognized { best: Some(0.4) });
        assert_eq!(nothing.outcome, Outcome::Unrecognized { best: None });
    }

    #[test]
    fn a_login_carries_the_token_as_its_text_and_shows_none_of_it_for_debugging() {
        let asked = Request::Login {
            token: Token::new("header.payload.signature"),
        };

        assert_eq!(
            serde_json::to_string(&asked).expect("serialize"),
            r#"{"type":"Login","body":{"token":"header.payload.signature"}}"#
        );
        assert!(!format!("{asked:?}").contains("payload"), "{asked:?}");
    }

    #[test]
    fn a_malformed_line_is_rejected_rather_than_guessed() {
        assert!(serde_json::from_str::<Request>("not json at all").is_err());
        assert!(serde_json::from_str::<Request>("{}").is_err());
        assert!(serde_json::from_str::<Request>(r#"{"type":"Nonsense"}"#).is_err());
    }
}
