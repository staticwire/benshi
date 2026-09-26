//! The fixture set, from the filename to the decision.
//!
//! An integration test rather than more unit tests, for the reason the
//! timeline's has and one of its own. **It uses nothing but the public API**: parse a name, build
//! a corpus and its index, record an assignment, decide. If any of that stops
//! being reachable from outside the crate, this file stops compiling, and
//! nothing in the modules' own tests would notice. **And every name is decided
//! by the whole sequence**, the way the daemon sends it, rather than by the one
//! stage a unit test is about.
//!
//! One filename per rule, in `fixtures/filenames.jsonl`, each invented rather
//! than taken from a library: the repository is public, and a library listing
//! says what somebody watches. A line names the rule it holds, the list it is
//! decided against, and the answer. **Where the answer is a known wrong or
//! weak one, the line says what is owed and to what**: to the list, for an
//! entry's format and episode count; to the year, which the list's side does
//! not carry; to the parser, for text it dropped or placed wrongly. Those lines
//! pin where the answer lands today, so that a change to it is noticed rather
//! than assumed.
//!
//! A score is never pinned. A fixture carrying one would break on every change
//! to the arithmetic while holding no rule of its own; whether the best
//! candidate was scored at all is the fact a refusal is checked for.
//!
//! What this file does **not** do is catch something alone, and that was
//! measured rather than assumed. Of the 147 breaks in the list that proves the
//! recognition tests can fail, 69 redden the test here, and every one of those
//! reddens a module test as well. That is a fact about the breaks the list
//! holds and not a reason to drop the file; a break that reaches only here is
//! the thing to write when one is wanted.

use benshi_core::path::RawPath;
use benshi_core::recognise::altname::Altnames;
use benshi_core::recognise::corpus::Corpus;
use benshi_core::recognise::index::Index;
use benshi_core::recognise::parse::{Episode, Parsed, parse};
use benshi_core::recognise::{Recognition, Stage, decide};
use serde::Deserialize;

const FIXTURES: &str = include_str!("fixtures/filenames.jsonl");

/// One line of the fixture file.
#[derive(Debug, Deserialize)]
struct Fixture {
    /// The filename, escaped the way a trace escapes a path, so that a name
    /// which is not valid UTF-8 can be written down in a text file.
    name: RawPath,
    /// The rule this name exists to hold.
    rule: String,
    /// The list it is decided against.
    list: Vec<Entry>,
    /// What the user named by hand before the name was decided.
    #[serde(default)]
    altnames: Vec<Named>,
    /// The answer.
    expect: Expect,
    /// What is owed, and to what, where the answer is a known wrong or weak one.
    #[serde(default)]
    owed: Option<String>,
}

/// One entry of the list, with the spellings it carries beside its title.
#[derive(Debug, Deserialize)]
struct Entry {
    title: String,
    #[serde(default)]
    spellings: Vec<String>,
}

/// An assignment, typed as text or made from what a file spelled.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Named {
    Typed { spelled: String, title: String },
    FromFile { file: String, title: String },
}

/// The answer a fixture expects, in the fixture file's words.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Expect {
    Recognised {
        title: String,
        episode: EpisodeSpelled,
        stage: StageName,
    },
    Ambiguous(Vec<String>),
    Unrecognised(Refused),
}

/// An episode as the fixture file writes it: a number, or a word for the
/// three answers that are not one.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum EpisodeSpelled {
    Only(u32),
    Words(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StageName {
    Altname,
    Key,
    Score,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Refused {
    Scored,
    NothingScored,
}

/// An answer reduced to what a fixture can say about it.
#[derive(Debug, PartialEq)]
enum Shape {
    Recognised {
        title: String,
        episode: Episode,
        stage: StageName,
    },
    Ambiguous(Vec<String>),
    Unrecognised(Refused),
}

impl Shape {
    /// What a fixture expects, read into the same shape an answer reduces to.
    fn expected(of: &Expect, rule: &str) -> Self {
        match of {
            Expect::Recognised {
                title,
                episode,
                stage,
            } => Self::Recognised {
                title: title.clone(),
                episode: match episode {
                    EpisodeSpelled::Only(number) => Episode::Only(*number),
                    EpisodeSpelled::Words(words) => match words.as_str() {
                        "several" => Episode::Several,
                        "not whole" => Episode::NotWhole,
                        "none" => Episode::Absent,
                        other => panic!(
                            "{rule}: an episode is a number, \"several\", \"not whole\" or \
                             \"none\", and {other:?} is none of those"
                        ),
                    },
                },
                stage: *stage,
            },
            Expect::Ambiguous(candidates) => Self::Ambiguous(candidates.clone()),
            Expect::Unrecognised(refused) => Self::Unrecognised(*refused),
        }
    }

    /// What recognition answered, reduced.
    fn of(answer: &Recognition) -> Self {
        match answer {
            Recognition::Recognised(found) => Self::Recognised {
                title: found.title.clone(),
                episode: found.episode,
                stage: match found.stage {
                    Stage::Altname(_) => StageName::Altname,
                    Stage::Key => StageName::Key,
                    Stage::Scored(_) => StageName::Score,
                },
            },
            Recognition::Ambiguous(ambiguity) => Self::Ambiguous(ambiguity.candidates.clone()),
            Recognition::Unrecognised(refusal) => Self::Unrecognised(if refusal.best.is_some() {
                Refused::Scored
            } else {
                Refused::NothingScored
            }),
        }
    }
}

/// Every line of the fixture file, one fixture each.
///
/// A line that does not parse, a blank one included, fails by its number
/// rather than leaving a hole in the set.
fn fixtures() -> Vec<Fixture> {
    FIXTURES
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|why| panic!("fixture line {} does not parse: {why}", index + 1))
        })
        .collect()
}

/// A name parsed the way the daemon parses it: the file's name and not its path.
fn parsed(name: &RawPath) -> Parsed {
    parse(&RawPath::from_bytes(name.file_name().to_vec()))
}

fn corpus_of(list: &[Entry]) -> Corpus {
    let mut corpus = Corpus::new();
    for entry in list {
        corpus.file(&entry.title, &entry.spellings);
    }

    corpus
}

/// The assignments a fixture makes, each checked against the corpus as a
/// user's would be, so a fixture cannot assign what the program refuses.
fn altnames_of(named: &[Named], corpus: &Corpus, rule: &str) -> Altnames {
    let mut altnames = Altnames::new();
    for assignment in named {
        let made = match assignment {
            Named::Typed { spelled, title } => altnames.name(spelled, title, corpus),
            Named::FromFile { file, title } => {
                let file = RawPath::from_bytes(file.as_bytes().to_vec());
                altnames.name_as_parsed(&parsed(&file), title, corpus)
            }
        };
        made.unwrap_or_else(|refused| panic!("{rule}: the assignment was refused: {refused}"));
    }

    altnames
}

#[test]
fn every_fixture_resolves_to_the_answer_it_expects() {
    // Every mismatch is reported at once rather than the first one: a change
    // to a rule reaches several fixtures, and the set of them is the finding.
    let mut wrong = Vec::new();

    for fixture in fixtures() {
        let corpus = corpus_of(&fixture.list);
        let index = Index::of(&corpus);
        let altnames = altnames_of(&fixture.altnames, &corpus, &fixture.rule);

        let answer = decide(&parsed(&fixture.name), &altnames, &corpus, &index);

        let got = Shape::of(&answer);
        let wanted = Shape::expected(&fixture.expect, &fixture.rule);
        if got != wanted {
            wrong.push(format!(
                "{}\n  name: {:?}\n  expected: {wanted:?}\n  got: {got:?}\n  from: {answer:?}",
                fixture.rule, fixture.name
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} fixture(s) did not resolve as expected:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

#[test]
fn the_fixture_set_holds_one_name_per_rule() {
    // Two fixtures for one rule are one fixture and a copy, and a rule left
    // blank holds nothing at all.
    let fixtures = fixtures();
    assert!(!fixtures.is_empty(), "the fixture file is empty");

    let mut rules: Vec<&str> = fixtures
        .iter()
        .map(|fixture| fixture.rule.as_str())
        .collect();
    rules.sort_unstable();
    rules.dedup();
    assert_eq!(
        rules.len(),
        fixtures.len(),
        "two fixtures hold the same rule"
    );

    for fixture in &fixtures {
        assert!(
            !fixture.rule.trim().is_empty(),
            "a fixture names no rule: {fixture:?}"
        );
        if let Some(owed) = &fixture.owed {
            assert!(
                !owed.trim().is_empty(),
                "{}: owed, but it does not say to what",
                fixture.rule
            );
        }
    }
}
