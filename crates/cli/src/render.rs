//! Turning what the daemon said into what a person reads.
//!
//! Every function here is pure, so what the daemon's answers look like on a
//! terminal is testable without a socket, a daemon or a platform. Nothing in
//! this module talks to anything.
//!
//! This is also where bytes become text. A filename travels as bytes from the
//! adapter that read it, through the bus and the socket, to here; only
//! something showing one to a person may guess what encoding those bytes were
//! in.

use std::fmt::Write as _;
use std::time::{Duration, UNIX_EPOCH};

use benshi_core::encoding::{Confidence, decode};
use benshi_core::list::Status;
use benshi_core::pin::Episodes;
use benshi_core::policy::Policy;
use benshi_core::recognize::parse::{Episode, Kind, Parsed};
use benshi_core::settle::Fact;
use benshi_core::{Capabilities, Known, MediaRef, PlayState};
use benshi_daemon::bus::BusEvent;
use benshi_daemon::delivery::{Action, Destination, DryRun, Rehearsed, Unsent};
use benshi_daemon::protocol::{
    DecidedBy, Explanation, NameListing, Outcome, PinListing, SourceListing,
};
use benshi_sync::plan::{Day, Standing, Why};
use benshi_sync::resolve::{By, Unresolved};
use humantime::format_rfc3339_seconds;

/// What is shown for a source that cannot name what it opened.
///
/// A source that publishes no location gives recognition nothing but what the
/// page or window calls itself. That is a different problem from recognizing a
/// filename, and a user deciding whether to allow such a source has to be told
/// which one they are getting.
const TITLE_ONLY: &str = "title only";

/// What is shown where a value could have been reported and was not.
const NOT_REPORTED: &str = "--:--";

/// What is shown where the source cannot carry the value at all.
///
/// Different from [`NOT_REPORTED`] on purpose. The domain keeps "did not say"
/// apart from "cannot say" from the adapter down; collapsing them here would
/// throw the distinction away at the last place it could still be seen.
const UNSUPPORTED: &str = "n/a";

/// What is shown when no reading has been decided yet.
const NOTHING_DECIDED: &str = "nothing has been decided yet\n";

/// What is shown when nothing has been mapped.
const NOTHING_MAPPED: &str = "nothing is mapped\n";

/// What a dry run shows where no operation is past the cursor.
const NOTHING_WAITS: &str = "nothing waits to be sent\n";

/// What a source is able to report, and what that leaves recognition to work
/// from.
///
/// Two facts rather than one. The tokens say where playback has reached; the
/// trailing note says what is playing, and it is spelled out rather than left
/// to be inferred from an absent `loc`.
#[must_use]
pub fn capabilities(of: Capabilities) -> String {
    let declared: Vec<&str> = [
        (of.position, "pos"),
        (of.duration, "dur"),
        (of.paused, "pause"),
        (of.location, "loc"),
    ]
    .into_iter()
    .filter_map(|(declared, name)| declared.then_some(name))
    .collect();
    let tokens = declared.join("+");

    if of.location {
        // `loc` is already among the tokens, and the note would contradict it.
        return tokens;
    }
    if tokens.is_empty() {
        return TITLE_ONLY.to_owned();
    }

    format!("{tokens}, {TITLE_ONLY}")
}

/// What a source is doing, as far as it will say.
#[must_use]
pub fn state(of: PlayState) -> &'static str {
    match of {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        // Not "stopped", which reads as playback having been stopped. The fact
        // this state carries is that the source has nothing open at all.
        PlayState::Stopped => "idle",
    }
}

/// What the daemon does with readings from a source.
#[must_use]
pub fn policy(of: Policy) -> &'static str {
    match of {
        Policy::Auto => "auto",
        Policy::Allow => "allow",
        Policy::Deny => "deny",
    }
}

/// A moment within the media, or why there is none.
#[must_use]
pub fn moment(of: Known<Duration>) -> String {
    match of {
        Known::Value(reached) => {
            let seconds = reached.as_secs();
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        }
        Known::NotReported => NOT_REPORTED.to_owned(),
        Known::Unsupported => UNSUPPORTED.to_owned(),
    }
}

/// What a source has open, as text.
///
/// A path is bytes and is decoded here and nowhere earlier. A name that is not
/// valid UTF-8 is read through the encoding [`decode`] infers rather than
/// replaced, because a row of replacement characters is exactly what a user
/// watching this stream is trying to get behind.
#[must_use]
pub fn media(of: &MediaRef) -> String {
    match of {
        MediaRef::LocalFile(path) => decode(path.as_bytes()).text,
        MediaRef::Remote(address) => address.clone(),
        MediaRef::Title(title) => title.clone(),
    }
}

/// Every source the daemon knows about, one to a line.
///
/// Columns are padded to the widest entry rather than to a fixed width: an
/// identity carries whatever the platform put in it, and truncating it would
/// hide the part that tells two windows of one player apart.
#[must_use]
pub fn listing(sources: &[SourceListing]) -> String {
    if sources.is_empty() {
        return "no source is open\n".to_owned();
    }

    let identity = width(sources.iter().map(|source| source.player.0.as_str()));
    let application = width(sources.iter().map(|source| source.app.0.as_str()));
    let mut shown = String::new();

    for source in sources {
        writeln!(
            shown,
            "{:identity$}  {:application$}  {:5}  {:7}  {}",
            source.player.0,
            source.app.0,
            policy(source.policy),
            state(source.state),
            capabilities(source.capabilities)
        )
        .expect("a String accepts every write");
    }

    shown
}

/// The widest of a column's entries, in bytes.
///
/// Bytes, while `{:width$}` pads in characters. That makes a column wider than
/// it needs to be and never misaligns one: a name has at most as many
/// characters as bytes, so every entry is padded to the same count. What would
/// misalign a column is a glyph the terminal draws double width, which neither
/// count sees. A D-Bus bus name is ASCII by the specification, and so is the
/// address Kodi is read at, so with the sources benshi reads neither arises.
fn width<'a>(entries: impl Iterator<Item = &'a str>) -> usize {
    entries.map(str::len).max().unwrap_or_default()
}

/// The most recent decision, or that there is none yet.
#[must_use]
pub fn why(of: Option<&Explanation>) -> String {
    of.map_or_else(|| NOTHING_DECIDED.to_owned(), explanation)
}

/// One decision, as a person reads it.
///
/// Whose file, what its name spelled, what was answered, and for a match the
/// stage that decided and what it decided by. The stage is printed by its name
/// and never by a number: a number computed from a position in the sequence is
/// the thing that moves when a stage is inserted.
#[must_use]
pub fn explanation(of: &Explanation) -> String {
    format!(
        "{}  {}\nspelled: {}\n{}",
        of.player,
        media(&of.media),
        spelled(&of.spelled),
        answer(&of.outcome)
    )
}

/// What recognition answered, or the show kept in its place, as the lines
/// below the parse.
///
/// Each line ends in a newline. A match takes two, what it was recognized as
/// and the stage that decided, and a third where a fact of its entry refused
/// the file. An ambiguity takes one, and a line for each entry a fact
/// refused and one for the entry the facts chose.
fn answer(of: &Outcome) -> String {
    match of {
        Outcome::Recognized {
            title,
            episode,
            stage,
            refused,
        } => {
            let mut shown = format!(
                "recognized: \"{title}\", {}\nstage: {}\n",
                episode_words(*episode),
                decided_by(stage)
            );
            if let Some(fact) = refused {
                writeln!(shown, "refused: {}", refusal(fact))
                    .expect("a String accepts every write");
            }
            shown
        }
        Outcome::Ambiguous {
            candidates,
            refused,
            chosen,
        } => {
            let quoted: Vec<String> = candidates
                .iter()
                .map(|candidate| format!("\"{candidate}\""))
                .collect();
            let mut shown = format!("ambiguous: {}\n", quoted.join(", "));
            for (title, fact) in refused {
                writeln!(shown, "refused: \"{title}\", {}", refusal(fact))
                    .expect("a String accepts every write");
            }
            if let Some((title, episode)) = chosen {
                writeln!(shown, "chosen: \"{title}\", {}", episode_words(*episode))
                    .expect("a String accepts every write");
            }
            shown
        }
        Outcome::Unrecognized { best: Some(score) } => {
            format!("unrecognized: closest candidate scored {score:.2}\n")
        }
        Outcome::Unrecognized { best: None } => {
            "unrecognized: nothing to score against\n".to_owned()
        }
    }
}

/// What a name spelled, in words, and whether its bytes had to be inferred.
///
/// The encoding is mentioned only where it was inferred. A title recovered by
/// inference is a weaker input than one that could not have been anything
/// else, and the person reading this is the one who can tell whether the
/// guess was right.
fn spelled(of: &Parsed) -> String {
    let mut parts = vec![of.title.as_ref().map_or_else(
        || "no title".to_owned(),
        |title| {
            if of.from_directory {
                format!("title \"{title}\" from directory")
            } else {
                format!("title \"{title}\"")
            }
        },
    )];
    if let Some(season) = of.season {
        parts.push(format!("season {season}"));
    }
    if let Some(part) = of.part {
        parts.push(format!("part {part}"));
    }
    parts.push(episode_words(of.episode));
    if let Some(year) = of.year {
        parts.push(format!("year {year}"));
    }
    if let Some(group) = &of.release_group {
        parts.push(format!("group \"{group}\""));
    }
    if of.confidence == Confidence::Detected {
        parts.push("encoding inferred".to_owned());
    }

    parts.join(", ")
}

/// An episode in words.
///
/// Only the first answer is a number. Several, a half and none are said as
/// such, because a number printed for any of them would read as an episode
/// somebody could record progress against.
fn episode_words(of: Episode) -> String {
    match of {
        Episode::Only(number) => format!("episode {number}"),
        Episode::Several => "several episodes".to_owned(),
        Episode::NotWhole => "an episode that is not a whole number".to_owned(),
        Episode::Absent => "no episode".to_owned(),
    }
}

/// The fact of an entry that refused a file, with what the name or the file
/// says beside what the entry holds where the fact compares the two.
///
/// A length is said as `watch` says one, in minutes and seconds.
fn refusal(of: &Fact) -> String {
    match of {
        Fact::NotReleased => "entry not released yet".to_owned(),
        Fact::Year { spelled, began } => {
            format!("name spells year {spelled}, entry began in {began}")
        }
        Fact::Kind(Kind::Extra) => "name spells OVA or special, entry is TV series".to_owned(),
        Fact::Kind(Kind::Film) => "name spells film, entry is TV series".to_owned(),
        Fact::Episodes(count) => format!("name spells no episode, entry has {count} episodes"),
        Fact::PastCount { episode, episodes } => {
            let unit = if *episodes == 1 {
                "episode"
            } else {
                "episodes"
            };
            format!("name spells episode {episode}, entry has {episodes} {unit}")
        }
        Fact::Length { file, episode } => format!(
            "file is {} long, under half of entry's {} episode",
            moment(Known::Value(*file)),
            moment(Known::Value(*episode))
        ),
    }
}

/// The stage that decided, by its name, and what it decided by.
fn decided_by(of: &DecidedBy) -> String {
    match of {
        DecidedBy::Altname(assignment) => {
            let mut shown = format!("altname, assigned \"{}\"", assignment.spelled);
            if let Some(season) = assignment.season {
                write!(shown, ", season {season}").expect("a String accepts every write");
            }
            if let Some(part) = assignment.part {
                write!(shown, ", part {part}").expect("a String accepts every write");
            }
            shown
        }
        DecidedBy::Key => "key".to_owned(),
        DecidedBy::Scored(score) => format!("score {score:.2}"),
        DecidedBy::Unlisted => "not on list, kept as show of its own".to_owned(),
    }
}

/// One event from the daemon, as a line for a person.
///
/// The machine-readable form is what the socket already carries, so nothing is
/// lost by rendering here: `socat - UNIX-CONNECT:...` gives the JSON unchanged.
#[must_use]
pub fn event(of: &BusEvent) -> String {
    match of {
        BusEvent::Snapshot(snapshot) => format!(
            "{}  {}  {} / {}  {}",
            snapshot.player.0,
            state(snapshot.state),
            moment(snapshot.position),
            moment(snapshot.duration),
            media(&snapshot.media)
        ),
        BusEvent::SourcesChanged { sources } if sources.is_empty() => {
            "sources: no source is open".to_owned()
        }
        BusEvent::SourcesChanged { sources } => {
            let named: Vec<&str> = sources.iter().map(|player| player.0.as_str()).collect();
            format!("sources: {}", named.join(", "))
        }
        BusEvent::SourceFailed { player, reason } => format!("{}  failed: {reason}", player.0),
        BusEvent::State(Some(session)) => format!("state: {}", session.snapshot.player.0),
        BusEvent::State(None) => "state: nothing is playing".to_owned(),
    }
}

/// What is shown once the token is removed.
pub const LOGGED_OUT: &str = "AniList token removed\n";

/// Whose the token is and until when it holds, as a login answers.
///
/// The time is said in UTC. A time past the year 9999, for which no date is
/// written, is said as one the token does not give.
#[must_use]
pub fn logged_in(name: &str, expires: Option<u64>) -> String {
    match expires.and_then(day_and_minute) {
        Some(until) => format!("logged in to AniList as {name}, token valid until {until}\n"),
        None => format!("logged in to AniList as {name}; token does not say when it runs out\n"),
    }
}

/// A time in seconds since the Unix epoch, as its day and minute in UTC:
/// `2030-03-17 17:46 UTC`.
fn day_and_minute(seconds: u64) -> Option<String> {
    let at = UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?;
    let mut written = String::new();
    // An error where the time is past the year 9999.
    write!(written, "{}", format_rfc3339_seconds(at)).ok()?;

    Some(format!(
        "{} {} UTC",
        written.get(..10)?,
        written.get(11..16)?
    ))
}

/// What a name means now, and what it meant before where it meant another
/// entry.
#[must_use]
pub fn mapped(spelled: &str, anilist: i64, title: &str, replaced: Option<&NameListing>) -> String {
    let mut shown = format!("mapped: \"{spelled}\" to {}", meaning(Some(anilist), title));
    if let Some(before) = replaced {
        write!(
            shown,
            ", in place of {}",
            meaning(before.anilist, &before.title)
        )
        .expect("a String accepts every write");
    }
    shown.push('\n');
    shown
}

/// The name taken back, as it was given.
#[must_use]
pub fn removed(spelled: &str) -> String {
    format!("removed: \"{spelled}\"\n")
}

/// Every name given and every run of episodes mapped, one to a line, the
/// names first.
///
/// A name says what it means and why it is not in force where it is not; a
/// pin says the entry's title, or that the list no longer holds the entry.
#[must_use]
pub fn maps(names: &[NameListing], pins: &[PinListing]) -> String {
    if names.is_empty() && pins.is_empty() {
        return NOTHING_MAPPED.to_owned();
    }

    let mut shown = String::new();
    for name in names {
        write!(
            shown,
            "\"{}\" to {}",
            name.spelled,
            meaning(name.anilist, &name.title)
        )
        .expect("a String accepts every write");
        if let Some(why) = &name.not_in_force {
            write!(shown, ", not in force: {why}").expect("a String accepts every write");
        }
        shown.push('\n');
    }
    for pin in pins {
        writeln!(shown, "{}", pin_words(pin)).expect("a String accepts every write");
    }
    shown
}

/// What episodes of a show mean now, and what they meant before where they
/// meant other episodes or another entry.
#[must_use]
pub fn pinned(pin: &PinListing, replaced: Option<&PinListing>) -> String {
    let mut shown = format!("mapped: {}", pin_words(pin));
    if let Some(before) = replaced {
        write!(shown, ", in place of {}", entry_run(before)).expect("a String accepts every write");
    }
    shown.push('\n');
    shown
}

/// Episodes of a show taken back, the show by the title it is filed under.
#[must_use]
pub fn unpinned(show: &str, episodes: Episodes) -> String {
    format!("removed: \"{show}\" {}\n", run_words(episodes))
}

/// That those episodes of a show were not mapped, so nothing was taken back.
#[must_use]
pub fn not_pinned(show: &str, episodes: Episodes) -> String {
    let verb = if episodes.count() == 1 { "is" } else { "are" };
    format!("\"{show}\" {} {verb} not mapped", run_words(episodes))
}

/// A pin in words: the show, its episodes, and the episodes of the entry.
fn pin_words(pin: &PinListing) -> String {
    format!(
        "\"{}\" {} to {}",
        pin.show,
        run_words(pin.episodes),
        entry_run(pin)
    )
}

/// The episodes of the entry a pin names, with the entry's title or that the
/// list no longer holds it.
fn entry_run(pin: &PinListing) -> String {
    let title = pin.title.as_ref().map_or_else(
        || "not on the list".to_owned(),
        |title| format!("\"{title}\""),
    );
    format!(
        "AniList {} {}, {title}",
        pin.anilist,
        run_words(pin.entry_episodes)
    )
}

/// A run of episodes in words: `episode 2`, `episodes 14-26`.
fn run_words(episodes: Episodes) -> String {
    if episodes.count() == 1 {
        format!("episode {}", episodes.first())
    } else {
        format!("episodes {}-{}", episodes.first(), episodes.last())
    }
}

/// An entry a name means: its number on AniList and its title, or the title
/// alone where the list holds no entry of its show.
fn meaning(anilist: Option<i64>, title: &str) -> String {
    match anilist {
        Some(number) => format!("AniList {number}, \"{title}\""),
        None => format!("\"{title}\""),
    }
}

/// What sending every operation that waits would write, under the size of
/// the list it is decided by and over a count of each outcome.
///
/// An operation takes two lines: what was watched and the episode of the
/// entry it goes to, then what would be written, why nothing would be, or
/// why it cannot be sent. A write says the entry before and after it, or
/// that it adds an entry no list holds, and of the dates and the rewatches
/// only what it changes.
#[must_use]
pub fn dry_run(of: &DryRun) -> String {
    let mut shown = format!("AniList, list as last read: {}\n\n", entries(of.entries));
    if of.operations.is_empty() {
        shown.push_str(NOTHING_WAITS);
        return shown;
    }

    for operation in &of.operations {
        writeln!(
            shown,
            "{}\n    {}",
            rehearsed(operation),
            outcome(operation)
        )
        .expect("a String accepts every write");
    }
    writeln!(shown, "\n{}", tally(&of.operations)).expect("a String accepts every write");
    shown
}

/// What an operation says was watched, and the episode of the entry it goes
/// to and what decided it, where it goes to one.
fn rehearsed(of: &Rehearsed) -> String {
    let Some(watched) = &of.watched else {
        return format!("#{} cannot be read", of.id);
    };
    let mut shown = format!(
        "#{} {} \"{}\" {}",
        of.id,
        day(watched.day),
        watched.title,
        watched.episode.map_or_else(
            || "no episode".to_owned(),
            |episode| format!("episode {episode}")
        )
    );
    let (to, off_the_list) = match &of.action {
        Action::Write { to, .. } | Action::Nothing { to, .. } => (Some(to), !to.listed),
        Action::CannotSend { to, .. } => (to.as_ref(), false),
    };
    if let Some(to) = to {
        write!(shown, " -> AniList {}", to.entry.0).expect("a String accepts every write");
        if let Some(title) = &to.title {
            write!(shown, " \"{title}\"").expect("a String accepts every write");
        }
        write!(shown, " episode {}", to.episode).expect("a String accepts every write");
        shown.push_str(match to.by {
            By::Entry => "",
            By::Rule => ", by anime-relations",
            By::Pin => ", by benshi map",
        });
        if off_the_list {
            shown.push_str(", not on the list");
        }
    }
    shown
}

/// What would be written, why nothing would be, or why the operation cannot
/// be sent.
fn outcome(of: &Rehearsed) -> String {
    match &of.action {
        Action::Write {
            to,
            before,
            after,
            undated,
        } => written(to, before.as_ref(), after, *undated),
        Action::Nothing { to, why } => format!("nothing to write: {}", nothing(to, *why)),
        Action::CannotSend { why, .. } => format!(
            "cannot send: {}",
            unsent(why, of.watched.as_ref().and_then(|watched| watched.episode))
        ),
    }
}

/// The entry before a write and after it, and the dates and the rewatches
/// the write changes.
fn written(to: &Destination, before: Option<&Standing>, after: &Standing, undated: bool) -> String {
    let mut shown = match before {
        Some(before) => format!(
            "{} -> {}",
            standing(before, to.episodes),
            standing(after, to.episodes)
        ),
        None => format!("adds it: {}", standing(after, to.episodes)),
    };
    let (started, completed, rewatches) = before.map_or((None, None, 0), |before| {
        (before.started, before.completed, before.rewatches)
    });
    if after.started != started
        && let Some(begun) = after.started
    {
        write!(shown, ", started {}", day(begun)).expect("a String accepts every write");
        if undated {
            shown.push_str(" if list has none");
        }
    }
    if after.completed != completed
        && let Some(ended) = after.completed
    {
        write!(shown, ", completed {}", day(ended)).expect("a String accepts every write");
    }
    if after.rewatches != rewatches {
        write!(shown, ", rewatches {rewatches} -> {}", after.rewatches)
            .expect("a String accepts every write");
    }
    shown
}

/// Why a viewing writes nothing to the entry it goes to.
fn nothing(to: &Destination, why: Why) -> String {
    match why {
        Why::Conflict { progress } => {
            format!("list already at {progress}/{}", count(to.episodes))
        }
        Why::Completed => "completed, rewatch begins at episode 1".to_owned(),
        Why::PastCount { episodes } => format!("entry has {}", episodes_of(episodes)),
        Why::EpisodeZero => "progress counts from episode 1".to_owned(),
    }
}

/// Why an operation cannot be sent, with the episode it says was watched.
fn unsent(why: &Unsent, episode: Option<u32>) -> String {
    let number = episode.map_or_else(|| "no episode".to_owned(), |episode| episode.to_string());
    match why {
        Unsent::Unresolved(Unresolved::NotListed) => "not on the list".to_owned(),
        Unsent::Unresolved(Unresolved::NoEpisode { episodes }) => format!(
            "name spells no episode, entry has {}",
            episodes.map_or_else(|| "no episode count".to_owned(), episodes_of)
        ),
        Unsent::Unresolved(Unresolved::Outside {
            episodes: Some(episodes),
        }) => format!(
            "entry has {}, no rule takes episode {number}",
            episodes_of(*episodes)
        ),
        Unsent::Unresolved(Unresolved::Outside { episodes: None }) => {
            format!("no rule takes episode {number}")
        }
        Unsent::Unresolved(Unresolved::LaterSeason { season, episode }) => format!(
            "{number} reads as season {season} episode {episode}; benshi map can name its entry"
        ),
        Unsent::Unresolved(Unresolved::Unnumbered) => {
            format!("anime-relations gives no number for episode {number}")
        }
        Unsent::Departed => "no longer on the list, though it was".to_owned(),
        Unsent::Unread(why) => why.clone(),
    }
}

/// How many operations would write, how many would write nothing and how
/// many cannot be sent, each where there are any.
fn tally(operations: &[Rehearsed]) -> String {
    let (mut writes, mut nothing, mut unsent) = (0, 0, 0);
    for operation in operations {
        match operation.action {
            Action::Write { .. } => writes += 1,
            Action::Nothing { .. } => nothing += 1,
            Action::CannotSend { .. } => unsent += 1,
        }
    }

    let mut counted = Vec::new();
    if writes > 0 {
        counted.push(counted_as(writes, "write", "writes"));
    }
    if nothing > 0 {
        counted.push(format!("{nothing} nothing to write"));
    }
    if unsent > 0 {
        counted.push(format!("{unsent} cannot send"));
    }
    format!(
        "{}: {}; dry run, nothing sent",
        counted_as(operations.len(), "operation", "operations"),
        counted.join(", ")
    )
}

/// Where the account stands on an entry: the status, and the progress out
/// of the count, or out of `?` where the list holds no count.
fn standing(of: &Standing, episodes: Option<u32>) -> String {
    let status = match of.status {
        Status::Planned => "planned",
        Status::Watching => "watching",
        Status::Rewatching => "rewatching",
        Status::Completed => "completed",
        Status::OnHold => "on hold",
        Status::Dropped => "dropped",
    };
    format!("{status} {}/{}", of.progress, count(episodes))
}

/// A count of episodes, or `?` where the list holds none.
fn count(episodes: Option<u32>) -> String {
    episodes.map_or_else(|| "?".to_owned(), |episodes| episodes.to_string())
}

/// How many episodes, in words.
fn episodes_of(episodes: u32) -> String {
    counted_as(episodes, "episode", "episodes")
}

/// How many entries the list holds, in words.
fn entries(of: usize) -> String {
    if of == 0 {
        "no entries".to_owned()
    } else {
        counted_as(of, "entry", "entries")
    }
}

/// A count and the word for what it counts, in the singular for one.
fn counted_as<T: Copy + std::fmt::Display + PartialEq + From<u8>>(
    count: T,
    one: &str,
    several: &str,
) -> String {
    let word = if count == T::from(1) { one } else { several };
    format!("{count} {word}")
}

/// A day as RFC 3339 writes a date.
fn day(of: Day) -> String {
    format!("{:04}-{:02}-{:02}", of.year, of.month, of.day)
}

#[cfg(test)]
mod tests {
    use super::{
        capabilities, dry_run, event, explanation, listing, logged_in, mapped, maps, moment,
        not_pinned, pinned, removed, state, unpinned, why,
    };
    use benshi_core::clock::Timestamp;
    use benshi_core::list::{EntryId, Status};
    use benshi_core::path::RawPath;
    use benshi_core::pin::Episodes;
    use benshi_core::policy::Policy;
    use benshi_core::recognize::altname::Assignment;
    use benshi_core::recognize::parse::{Episode, Kind};
    use benshi_core::settle::Fact;
    use benshi_core::{
        AppName, Capabilities, Known, MediaRef, PlayState, PlayerId, PlayerSnapshot, ShowId,
    };
    use benshi_daemon::bus::BusEvent;
    use benshi_daemon::delivery::{Action, Destination, DryRun, Rehearsed, Unsent, Watched};
    use benshi_daemon::protocol::{
        DecidedBy, Explanation, NameListing, Outcome, PinListing, SourceListing,
    };
    use benshi_daemon::recognition::Recognizer;
    use benshi_sync::plan::{Day, Standing, Why};
    use benshi_sync::resolve::{By, Unresolved};
    // The reason a failure carries comes from here in the daemon, so the tests
    // that check how one reads take it from here too rather than writing one.
    use benshi_detect::WatchError;
    use std::time::Duration;

    /// A filename in Shift-JIS, which is not valid UTF-8.
    const SHIFT_JIS_NAME: &[u8] = b"/anime/\x83\x5c\x83\x8c\x83\x62\x83\x5e - 03.mkv";

    fn everything() -> Capabilities {
        Capabilities {
            position: true,
            duration: true,
            paused: true,
            location: true,
        }
    }

    fn nothing() -> Capabilities {
        Capabilities {
            position: false,
            duration: false,
            paused: false,
            location: false,
        }
    }

    fn a_listing(identity: &str, app: &str, policy: Policy, of: Capabilities) -> SourceListing {
        SourceListing {
            player: PlayerId(identity.to_owned()),
            app: AppName(app.to_owned()),
            policy,
            capabilities: of,
            state: PlayState::Playing,
        }
    }

    #[test]
    fn a_source_that_reports_everything_names_all_four() {
        assert_eq!(capabilities(everything()), "pos+dur+pause+loc");
    }

    #[test]
    fn a_source_that_cannot_name_what_it_opened_says_so_beside_the_rest() {
        // The column answers two questions: where playback has reached, and
        // what is playing. A source with no location can still report progress,
        // and a user deciding whether to allow it needs to see both.
        let streaming = Capabilities {
            location: false,
            ..everything()
        };

        assert_eq!(capabilities(streaming), "pos+dur+pause, title only");
    }

    #[test]
    fn a_source_that_declares_nothing_says_only_that_it_has_a_title() {
        // No leading comma and no empty column. An empty cell reads as a bug in
        // the renderer rather than as a fact about the source.
        assert_eq!(capabilities(nothing()), "title only");
    }

    #[test]
    fn a_source_that_cannot_report_a_position_does_not_claim_one() {
        let no_position = Capabilities {
            position: false,
            ..everything()
        };

        let shown = capabilities(no_position);

        assert!(!shown.contains("pos"), "claimed a position: {shown}");
        assert_eq!(shown, "dur+pause+loc");
    }

    #[test]
    fn a_source_that_names_what_it_opened_is_never_title_only() {
        let named = Capabilities {
            position: false,
            duration: false,
            paused: false,
            location: true,
        };

        assert_eq!(capabilities(named), "loc");
    }

    #[test]
    fn nothing_open_reads_as_idle_rather_than_stopped() {
        // The state answers what the source is doing. "Stopped" reads as
        // playback having been stopped; the fact is that nothing is open.
        assert_eq!(state(PlayState::Stopped), "idle");
        assert_eq!(state(PlayState::Playing), "playing");
        assert_eq!(state(PlayState::Paused), "paused");
    }

    #[test]
    fn a_listing_shows_the_identity_and_the_application_as_two_columns() {
        // Two windows of one player are two identities and one application.
        // Policy is set on the application, so the name to type has to be on
        // screen beside the identity that distinguishes the windows.
        let shown = listing(&[
            a_listing("mpv.instance1701", "mpv", Policy::Auto, everything()),
            a_listing("mpv.instance1702", "mpv", Policy::Auto, everything()),
        ]);

        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines.len(), 2, "one line per source: {shown}");
        assert!(lines[0].contains("mpv.instance1701"), "{shown}");
        assert!(lines[1].contains("mpv.instance1702"), "{shown}");
        for line in lines {
            assert!(line.contains("mpv "), "no application column: {line}");
        }
    }

    #[test]
    fn a_listing_shows_the_policy_in_force_for_each_source() {
        let shown = listing(&[
            a_listing("mpv", "mpv", Policy::Auto, everything()),
            a_listing("firefox.instance30062", "firefox", Policy::Deny, nothing()),
            a_listing("vlc", "vlc", Policy::Allow, everything()),
        ]);

        assert!(shown.contains("auto"), "{shown}");
        assert!(shown.contains("deny"), "{shown}");
        assert!(shown.contains("allow"), "{shown}");
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_printing_nothing() {
        // Nothing on screen cannot be told from a command that failed quietly.
        let shown = listing(&[]);

        assert!(!shown.trim().is_empty(), "an empty listing printed nothing");
        assert!(shown.contains("no source"), "got {shown:?}");
    }

    #[test]
    fn a_reported_moment_reads_as_minutes_and_seconds() {
        assert_eq!(moment(Known::Value(Duration::from_secs(0))), "00:00");
        assert_eq!(moment(Known::Value(Duration::from_secs(5))), "00:05");
        assert_eq!(moment(Known::Value(Duration::from_secs(1_383))), "23:03");
        assert_eq!(moment(Known::Value(Duration::from_secs(3_661))), "61:01");
    }

    #[test]
    fn a_moment_that_was_not_reported_reads_differently_from_one_that_cannot_be() {
        // The whole reason the domain carries three values rather than two. A
        // renderer that collapses them here throws the distinction away at the
        // last possible moment, where nobody downstream can recover it.
        let not_reported = moment(Known::NotReported);
        let unsupported = moment(Known::Unsupported);

        assert_ne!(not_reported, unsupported);
        assert_ne!(not_reported, "00:00", "an absence rendered as the start");
        assert_ne!(unsupported, "00:00", "an absence rendered as the start");
    }

    #[test]
    fn a_reading_shows_where_playback_is_and_what_is_playing() {
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(b"/anime/ep 03.mkv".to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::Value(Duration::from_mins(23)),
            observed_at: Timestamp::epoch(),
        };

        let shown = event(&BusEvent::Snapshot(snapshot));

        assert!(shown.contains("mpv"), "{shown}");
        assert!(shown.contains("00:05"), "{shown}");
        assert!(shown.contains("23:00"), "{shown}");
        assert!(shown.contains("/anime/ep 03.mkv"), "{shown}");
    }

    #[test]
    fn a_filename_that_is_not_utf8_is_shown_rather_than_refused() {
        // A name out of a Japanese archive unpacked with the wrong encoding is
        // exactly what a user watching this stream is trying to diagnose.
        // Refusing it, or printing a row of replacement characters, hides the
        // one thing they came to see.
        let snapshot = PlayerSnapshot {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(RawPath::from_bytes(SHIFT_JIS_NAME.to_vec())),
            state: PlayState::Playing,
            position: Known::Value(Duration::from_secs(5)),
            duration: Known::NotReported,
            observed_at: Timestamp::epoch(),
        };

        let shown = event(&BusEvent::Snapshot(snapshot));

        assert!(shown.contains(" - 03.mkv"), "the name was lost: {shown}");
        assert!(
            !shown.contains('\u{fffd}'),
            "the name was replaced rather than read: {shown}"
        );
    }

    #[test]
    fn a_membership_change_names_the_sources_rather_than_counting_them() {
        let shown = event(&BusEvent::SourcesChanged {
            sources: vec![PlayerId("mpv".to_owned()), PlayerId("Feishin".to_owned())],
        });

        assert!(shown.contains("mpv"), "{shown}");
        assert!(shown.contains("Feishin"), "{shown}");
    }

    #[test]
    fn a_membership_that_went_empty_says_so() {
        // An event whose list is empty must not render as an event with no
        // content: every player closing is a thing that happened.
        let shown = event(&BusEvent::SourcesChanged {
            sources: Vec::new(),
        });

        assert!(
            !shown.trim().is_empty(),
            "an empty membership printed nothing"
        );
        assert!(shown.contains("no source"), "got {shown:?}");
    }

    #[test]
    fn a_failure_names_the_source_and_the_reason() {
        let shown = event(&BusEvent::SourceFailed {
            player: PlayerId("vlc".to_owned()),
            reason: "did not answer within 500ms".to_owned(),
        });

        assert!(shown.contains("vlc"), "{shown}");
        assert!(shown.contains("did not answer within 500ms"), "{shown}");
    }

    #[test]
    fn a_failure_line_built_from_a_real_error_reads_as_one_sentence() {
        // The test above supplies a reason of its own, and a test that does
        // cannot see a reason that names its source. With such a reason
        // `benshi watch` prints a line like:
        //
        //   mpv.instance-X  failed: PlayerId("mpv.instance-X") did not answer within 500ms
        //
        // The identity belongs to the event and not to the reason:
        // `BusEvent::SourceFailed` carries both so that this function lays them
        // out, and a reason that repeats the subject prints it twice wherever a
        // person reads it. Both cases below take the reason from a real error.
        //
        // The whole line is pinned rather than picked at. Two weaker checks
        // pass for a line that is still wrong: counting the identity passes for
        // a bare fragment, and looking for the absence of `PlayerId` passes for
        // a reason that names the source in any other form. One sentence, read
        // end to end, is the thing being claimed.
        let player = PlayerId("mpv.instance-NvZEKsqR".to_owned());
        let cases = [
            (
                WatchError::Timeout {
                    player: player.clone(),
                    deadline: Duration::from_millis(500),
                },
                "mpv.instance-NvZEKsqR  failed: did not answer within 500ms",
            ),
            (
                WatchError::Unavailable(player.clone()),
                "mpv.instance-NvZEKsqR  failed: is no longer present",
            ),
        ];

        for (failure, expected) in cases {
            let shown = event(&BusEvent::SourceFailed {
                player: player.clone(),
                reason: failure.to_string(),
            });

            assert_eq!(shown, expected);
        }
    }

    /// An explanation of the file at this path, answered this way.
    ///
    /// The name is parsed the way a round parses it, so what is shown is what
    /// the daemon would have decided about.
    fn explained(path: &[u8], outcome: Outcome) -> Explanation {
        let path = RawPath::from_bytes(path.to_vec());
        let (spelled, _refused) = Recognizer::empty().decide(&path);

        Explanation {
            player: PlayerId("mpv".to_owned()),
            media: MediaRef::LocalFile(path),
            spelled,
            outcome,
        }
    }

    /// A match of this title by this stage, at episode three.
    fn found(title: &str, stage: DecidedBy) -> Outcome {
        Outcome::Recognized {
            title: title.to_owned(),
            episode: Episode::Only(3),
            stage,
            refused: None,
        }
    }

    /// The line of an explanation that begins this way.
    fn line_of<'a>(shown: &'a str, beginning: &str) -> &'a str {
        shown
            .lines()
            .find(|line| line.starts_with(beginning))
            .unwrap_or_else(|| panic!("no line begins with {beginning:?}: {shown:?}"))
    }

    #[test]
    fn nothing_decided_says_so_rather_than_printing_nothing() {
        // Nothing on screen cannot be told from a command that failed quietly,
        // and a client asking before the first reading is the ordinary case.
        let shown = why(None);

        assert!(shown.contains("nothing has been decided"), "got {shown:?}");
    }

    #[test]
    fn an_explanation_names_the_player_and_the_file_first() {
        let shown = explanation(&explained(
            b"/anime/[Group] Show Title - 03.mkv",
            Outcome::Unrecognized { best: None },
        ));

        let first = shown.lines().next().expect("a first line");
        assert!(first.contains("mpv"), "{first}");
        assert!(
            first.contains("/anime/[Group] Show Title - 03.mkv"),
            "{first}"
        );
    }

    #[test]
    fn an_explanation_shows_what_the_name_spelled() {
        // In words, and the season among them: a user comparing the file that
        // matched with the one that did not is looking for exactly the thing
        // the parser cut out of the title.
        let shown = explanation(&explained(
            b"[Group] Show Title S02E03 [1080p].mkv",
            Outcome::Unrecognized { best: None },
        ));

        let spelled = line_of(&shown, "spelled:");
        assert!(spelled.contains("\"Show Title\""), "{spelled}");
        assert!(spelled.contains("season 2"), "{spelled}");
        assert!(spelled.contains("episode 3"), "{spelled}");
        assert!(spelled.contains("\"Group\""), "{spelled}");
    }

    #[test]
    fn a_title_read_from_a_directory_says_so() {
        let shown = explanation(&explained(
            b"/video/Show.Title.Season.1.1999.DVDRip-Group/01 Quiet Harbor.mkv",
            Outcome::Unrecognized { best: None },
        ));

        let spelled = line_of(&shown, "spelled:");
        assert!(
            spelled.starts_with("spelled: title \"Show Title\" from directory, "),
            "{spelled}"
        );

        let shown = explanation(&explained(
            b"/anime/[Group] Show Title - 03.mkv",
            Outcome::Unrecognized { best: None },
        ));

        assert!(
            !line_of(&shown, "spelled:").contains("directory"),
            "{shown}"
        );
    }

    #[test]
    fn a_name_that_spelled_no_title_says_so() {
        let shown = explanation(&explained(
            b"/anime/ep 03.mkv",
            Outcome::Unrecognized { best: None },
        ));

        let spelled = line_of(&shown, "spelled:");
        assert!(spelled.contains("no title"), "{spelled}");
        assert!(spelled.contains("episode 3"), "{spelled}");
    }

    #[test]
    fn a_name_that_was_not_utf8_is_shown_read_and_said_to_be_inferred() {
        // The confidence travels with the parse so that this line can say it:
        // a title recovered by inference is a weaker input than one that could
        // not have been anything else, and the user is the one who can tell
        // whether the guess was right.
        let shown = explanation(&explained(
            SHIFT_JIS_NAME,
            Outcome::Unrecognized { best: None },
        ));

        assert!(shown.contains("ソレッタ"), "the name was not read: {shown}");
        assert!(
            !shown.contains('\u{fffd}'),
            "the name was replaced rather than read: {shown}"
        );
        assert!(line_of(&shown, "spelled:").contains("inferred"), "{shown}");
    }

    #[test]
    fn a_match_by_an_assignment_shows_the_text_the_user_gave_and_the_season_it_was_bound_to() {
        // What a user whose first-season file did not follow their assignment
        // needs to see: the text they typed, and the season the assignment
        // was bound to, which the text itself does not spell.
        let shown = explanation(&explained(
            b"Kaguya-sama.Love.is.War.S03E03.1080p.WEB-DL.mkv",
            found(
                "Kaguya-sama: Love is War -Ultra Romantic-",
                DecidedBy::Altname(Assignment {
                    spelled: "Kaguya-sama Love is War".to_owned(),
                    season: Some(3),
                    part: None,
                    show: ShowId(2),
                    title: "Kaguya-sama: Love is War -Ultra Romantic-".to_owned(),
                }),
            ),
        ));

        assert!(
            line_of(&shown, "recognized:").contains("Kaguya-sama: Love is War -Ultra Romantic-"),
            "{shown}"
        );
        let stage = line_of(&shown, "stage:");
        assert!(stage.contains("\"Kaguya-sama Love is War\""), "{stage}");
        assert!(stage.contains("season 3"), "{stage}");
    }

    #[test]
    fn a_stage_is_named_rather_than_numbered() {
        // The criterion speaks of "stage one" and the type names its stages,
        // because a number computed from a position moves when a stage is
        // inserted. What is printed is the name.
        let cases = [
            (
                DecidedBy::Altname(Assignment {
                    spelled: "SnK".to_owned(),
                    season: None,
                    part: None,
                    show: ShowId(1),
                    title: "Show Title".to_owned(),
                }),
                "stage: altname",
            ),
            (DecidedBy::Key, "stage: key"),
            (DecidedBy::Scored(0.83), "stage: score"),
        ];

        for (stage, expected) in cases {
            let shown = explanation(&explained(
                b"[Group] Show Title - 03.mkv",
                found("Show Title", stage),
            ));

            assert!(
                line_of(&shown, "stage:").starts_with(expected),
                "expected {expected:?} in {shown:?}"
            );
        }
    }

    #[test]
    fn a_name_the_list_does_not_hold_says_so_in_place_of_a_stage() {
        let shown = explanation(&explained(
            b"[Group] Some Other Show - 03.mkv",
            found("Some Other Show", DecidedBy::Unlisted),
        ));

        assert_eq!(
            line_of(&shown, "stage:"),
            "stage: not on list, kept as show of its own"
        );
    }

    #[test]
    fn a_match_by_score_shows_the_score_it_won_with() {
        let shown = explanation(&explained(
            b"[Group] Show Title - 03.mkv",
            found("Show Title", DecidedBy::Scored(0.83)),
        ));

        assert!(line_of(&shown, "stage:").contains("0.83"), "{shown}");
    }

    #[test]
    fn a_file_a_fact_refused_names_the_fact_after_the_stage() {
        // The line after the stage, one for each fact, with what the name or
        // the file says beside what the entry holds where the fact compares
        // the two.
        let cases = [
            (Fact::NotReleased, "refused: entry not released yet"),
            (
                Fact::Year {
                    spelled: 2019,
                    began: 2021,
                },
                "refused: name spells year 2019, entry began in 2021",
            ),
            (
                Fact::Episodes(2),
                "refused: name spells no episode, entry has 2 episodes",
            ),
            (
                Fact::Length {
                    file: Duration::from_secs(114),
                    episode: Duration::from_mins(40),
                },
                "refused: file is 01:54 long, under half of entry's 40:00 episode",
            ),
            (
                Fact::PastCount {
                    episode: 5,
                    episodes: 1,
                },
                "refused: name spells episode 5, entry has 1 episode",
            ),
            (
                Fact::Kind(Kind::Extra),
                "refused: name spells OVA or special, entry is TV series",
            ),
            (
                Fact::Kind(Kind::Film),
                "refused: name spells film, entry is TV series",
            ),
            (
                Fact::PastCount {
                    episode: 13,
                    episodes: 12,
                },
                "refused: name spells episode 13, entry has 12 episodes",
            ),
        ];

        for (fact, line) in cases {
            let shown = explanation(&explained(
                b"[Group] Show Title [1080p].mkv",
                Outcome::Recognized {
                    title: "Show Title".to_owned(),
                    episode: Episode::Absent,
                    stage: DecidedBy::Key,
                    refused: Some(fact),
                },
            ));

            let lines: Vec<&str> = shown.lines().collect();
            assert_eq!(lines.len(), 5, "{shown:?}");
            assert_eq!(lines[3], "stage: key", "{shown:?}");
            assert_eq!(lines[4], line, "{shown:?}");
        }
    }

    #[test]
    fn a_file_the_facts_let_stand_has_no_line_of_them() {
        let shown = explanation(&explained(
            b"[Group] Show Title - 03.mkv",
            found("Show Title", DecidedBy::Key),
        ));

        assert_eq!(shown.lines().count(), 4, "{shown:?}");
        assert!(!shown.contains("refused"), "{shown:?}");
    }

    #[test]
    fn an_ambiguity_names_every_candidate() {
        let shown = explanation(&explained(
            b"Fruits Basket - 01.mkv",
            Outcome::Ambiguous {
                candidates: vec![
                    "Fruits Basket".to_owned(),
                    "Fruits Basket (2019)".to_owned(),
                ],
                refused: Vec::new(),
                chosen: None,
            },
        ));

        let ambiguous = line_of(&shown, "ambiguous:");
        assert!(ambiguous.contains("\"Fruits Basket\""), "{ambiguous}");
        assert!(
            ambiguous.contains("\"Fruits Basket (2019)\""),
            "{ambiguous}"
        );
    }

    #[test]
    fn an_ambiguity_the_facts_settled_names_each_refusal_and_what_they_chose() {
        let shown = explanation(&explained(
            b"Show Title (1988).mkv",
            Outcome::Ambiguous {
                candidates: vec!["Show Title".to_owned(), "Show Title".to_owned()],
                refused: vec![("Show Title".to_owned(), Fact::NotReleased)],
                chosen: Some(("Show Title".to_owned(), Episode::Absent)),
            },
        ));

        let lines: Vec<&str> = shown.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                "ambiguous: \"Show Title\", \"Show Title\"",
                "refused: \"Show Title\", entry not released yet",
                "chosen: \"Show Title\", no episode",
            ],
            "{shown:?}"
        );
    }

    #[test]
    fn an_ambiguity_the_facts_left_undecided_names_each_refusal_and_nothing_chosen() {
        let shown = explanation(&explained(
            b"[Group] Show Title - 05.mkv",
            Outcome::Ambiguous {
                candidates: vec![
                    "Show Title".to_owned(),
                    "Show Title (2019)".to_owned(),
                    "Show Title (2027)".to_owned(),
                ],
                refused: vec![
                    (
                        "Show Title".to_owned(),
                        Fact::Year {
                            spelled: 2019,
                            began: 2001,
                        },
                    ),
                    ("Show Title (2027)".to_owned(), Fact::Episodes(2)),
                ],
                chosen: None,
            },
        ));

        let lines: Vec<&str> = shown.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                "ambiguous: \"Show Title\", \"Show Title (2019)\", \"Show Title (2027)\"",
                "refused: \"Show Title\", name spells year 2019, entry began in 2001",
                "refused: \"Show Title (2027)\", name spells no episode, entry has 2 episodes",
            ],
            "{shown:?}"
        );
    }

    #[test]
    fn a_refusal_says_how_close_the_best_came_or_that_nothing_was_scored() {
        // Absent and low are different answers and lead different places: a
        // score says the list was searched and the name is the problem, an
        // absence says the list is.
        let close = explanation(&explained(
            b"Some Other Show - 03.mkv",
            Outcome::Unrecognized { best: Some(0.4) },
        ));
        let nothing = explanation(&explained(
            b"Some Other Show - 03.mkv",
            Outcome::Unrecognized { best: None },
        ));

        assert_eq!(
            line_of(&close, "unrecognized:"),
            "unrecognized: closest candidate scored 0.40"
        );
        let unscored = line_of(&nothing, "unrecognized:");
        assert!(unscored.contains("nothing"), "{unscored}");
        assert!(
            !unscored.contains("0.00"),
            "an absence rendered as a score of zero: {unscored}"
        );
    }

    #[test]
    fn an_episode_that_is_not_one_number_is_said_in_words() {
        // Several, a half, and none are three answers a consumer must not
        // read as a number, so none of them is printed as one.
        let cases = [
            (Episode::Several, "several episodes"),
            (Episode::NotWhole, "not a whole number"),
            (Episode::Absent, "no episode"),
        ];

        for (episode, words) in cases {
            let shown = explanation(&explained(
                b"[Group] Show Title - 03.mkv",
                Outcome::Recognized {
                    title: "Show Title".to_owned(),
                    episode,
                    stage: DecidedBy::Key,
                    refused: None,
                },
            ));

            let recognized = line_of(&shown, "recognized:");
            assert!(recognized.contains(words), "{recognized}");
            assert!(
                !recognized.contains("episode 0"),
                "an absence rendered as a number: {recognized}"
            );
        }
    }

    #[test]
    fn a_login_says_whose_the_token_is_and_until_when_in_utc() {
        assert_eq!(
            logged_in("someone", Some(1_900_000_000)),
            "logged in to AniList as someone, token valid until 2030-03-17 17:46 UTC\n"
        );
    }

    #[test]
    fn a_token_that_does_not_say_when_it_runs_out_is_said_so() {
        assert_eq!(
            logged_in("someone", None),
            "logged in to AniList as someone; token does not say when it runs out\n"
        );
    }

    #[test]
    fn a_time_past_the_last_date_written_is_said_as_none() {
        // The last second of the year 9999 is the last time a date is
        // written for.
        assert_eq!(
            logged_in("someone", Some(253_402_300_799)),
            "logged in to AniList as someone, token valid until 9999-12-31 23:59 UTC\n"
        );
        for past in [253_402_300_800, u64::MAX] {
            assert_eq!(
                logged_in("someone", Some(past)),
                "logged in to AniList as someone; token does not say when it runs out\n",
                "{past}"
            );
        }
    }

    /// A name given, as the daemon lists it.
    fn a_name(spelled: &str, anilist: Option<i64>, title: &str) -> NameListing {
        NameListing {
            spelled: spelled.to_owned(),
            anilist,
            title: title.to_owned(),
            not_in_force: None,
        }
    }

    #[test]
    fn a_name_mapped_says_which_entry_it_means() {
        assert_eq!(
            mapped("Show Title", 12345, "Show Title Season 2", None),
            "mapped: \"Show Title\" to AniList 12345, \"Show Title Season 2\"\n"
        );
    }

    #[test]
    fn a_name_mapped_again_says_what_it_meant_before() {
        // The entry it leaves is the one a user would have to put back.
        let before = a_name("show title", Some(678), "Show Title");

        assert_eq!(
            mapped("Show Title", 12345, "Show Title Season 2", Some(&before)),
            "mapped: \"Show Title\" to AniList 12345, \"Show Title Season 2\", \
             in place of AniList 678, \"Show Title\"\n"
        );
    }

    #[test]
    fn an_entry_that_left_the_list_is_said_by_its_title_alone() {
        let before = a_name("Show Title", None, "Show Title");

        assert_eq!(
            mapped("Show Title", 12345, "Show Title Season 2", Some(&before)),
            "mapped: \"Show Title\" to AniList 12345, \"Show Title Season 2\", \
             in place of \"Show Title\"\n"
        );
    }

    #[test]
    fn a_name_taken_back_is_said_as_it_was_given() {
        assert_eq!(removed("Show Title"), "removed: \"Show Title\"\n");
    }

    #[test]
    fn the_names_given_are_listed_one_to_a_line_in_the_order_given() {
        let left = NameListing {
            not_in_force: Some("its entry is not on the list".to_owned()),
            ..a_name("Old Title", None, "A Show That Left")
        };

        assert_eq!(
            maps(
                &[
                    a_name("Show Title", Some(12345), "Show Title Season 2"),
                    left,
                    a_name("Other Title", Some(678), "Other Title"),
                ],
                &[]
            ),
            "\"Show Title\" to AniList 12345, \"Show Title Season 2\"\n\
             \"Old Title\" to \"A Show That Left\", not in force: its entry is not on the list\n\
             \"Other Title\" to AniList 678, \"Other Title\"\n"
        );
    }

    #[test]
    fn nothing_mapped_says_so_rather_than_printing_nothing() {
        assert_eq!(maps(&[], &[]), "nothing is mapped\n");
    }

    fn run(first: u32, last: u32) -> Episodes {
        Episodes::new(first, last).expect("a run from its first episode to its last")
    }

    /// Episodes 14 to 26 of a show, which are an entry's 1 to 13.
    fn a_pin() -> PinListing {
        PinListing {
            show: "Show Title".to_owned(),
            episodes: run(14, 26),
            anilist: 12345,
            entry_episodes: run(1, 13),
            title: Some("Show Title Season 2".to_owned()),
        }
    }

    #[test]
    fn episodes_mapped_say_which_episodes_of_which_entry_they_are() {
        assert_eq!(
            pinned(&a_pin(), None),
            "mapped: \"Show Title\" episodes 14-26 to AniList 12345 episodes 1-13, \
             \"Show Title Season 2\"\n"
        );
    }

    #[test]
    fn one_episode_mapped_is_said_as_one() {
        let one = PinListing {
            episodes: run(2, 2),
            anilist: 678,
            entry_episodes: run(1, 1),
            title: Some("Show Title Film".to_owned()),
            ..a_pin()
        };

        assert_eq!(
            pinned(&one, None),
            "mapped: \"Show Title\" episode 2 to AniList 678 episode 1, \"Show Title Film\"\n"
        );
    }

    #[test]
    fn episodes_mapped_again_say_what_they_meant_before() {
        // The pin they leave is the one a user would have to put back.
        let before = PinListing {
            anilist: 678,
            entry_episodes: run(14, 26),
            title: Some("Show Title".to_owned()),
            ..a_pin()
        };

        assert_eq!(
            pinned(&a_pin(), Some(&before)),
            "mapped: \"Show Title\" episodes 14-26 to AniList 12345 episodes 1-13, \
             \"Show Title Season 2\", in place of AniList 678 episodes 14-26, \"Show Title\"\n"
        );
    }

    #[test]
    fn episodes_taken_back_are_said_by_the_title_the_show_is_filed_under() {
        assert_eq!(
            unpinned("Show Title", run(14, 26)),
            "removed: \"Show Title\" episodes 14-26\n"
        );
    }

    #[test]
    fn episodes_never_mapped_are_said_to_be_so_in_one_number_or_more() {
        assert_eq!(
            not_pinned("Show Title", run(14, 26)),
            "\"Show Title\" episodes 14-26 are not mapped"
        );
        assert_eq!(
            not_pinned("Show Title", run(2, 2)),
            "\"Show Title\" episode 2 is not mapped"
        );
    }

    #[test]
    fn episodes_mapped_are_listed_where_no_name_is_given() {
        assert_eq!(
            maps(&[], &[a_pin()]),
            "\"Show Title\" episodes 14-26 to AniList 12345 episodes 1-13, \"Show Title Season 2\"\n"
        );
    }

    #[test]
    fn names_and_episodes_mapped_are_listed_the_names_first() {
        let left = PinListing {
            show: "Other Title".to_owned(),
            episodes: run(2, 2),
            anilist: 678,
            entry_episodes: run(1, 1),
            title: None,
        };

        assert_eq!(
            maps(
                &[a_name("Show Title", Some(12345), "Show Title Season 2")],
                &[a_pin(), left]
            ),
            "\"Show Title\" to AniList 12345, \"Show Title Season 2\"\n\
             \"Show Title\" episodes 14-26 to AniList 12345 episodes 1-13, \"Show Title Season 2\"\n\
             \"Other Title\" episode 2 to AniList 678 episode 1, not on the list\n"
        );
    }

    const fn november(day: u8) -> Day {
        Day {
            year: 2026,
            month: 11,
            day,
        }
    }

    const fn standing(
        status: Status,
        progress: u32,
        rewatches: u32,
        started: Option<Day>,
        completed: Option<Day>,
    ) -> Standing {
        Standing {
            status,
            progress,
            rewatches,
            started,
            completed,
        }
    }

    /// An operation of `Show Title` on this day of November.
    fn rehearsed(id: i64, episode: Option<u32>, day: u8, action: Action) -> Rehearsed {
        Rehearsed {
            id,
            watched: Some(Watched {
                day: november(day),
                title: "Show Title".to_owned(),
                episode,
            }),
            action,
        }
    }

    /// This episode of AniList 12345, a show's own entry of twelve episodes.
    fn of_its_entry(episode: u32) -> Destination {
        Destination {
            entry: EntryId(12345),
            episode,
            by: By::Entry,
            title: None,
            episodes: Some(12),
            listed: true,
        }
    }

    /// A dry run of these operations over a list of 410 entries.
    fn over_the_list(operations: Vec<Rehearsed>) -> String {
        dry_run(&DryRun {
            entries: 410,
            operations,
        })
    }

    /// The lines a dry run of one operation says of it.
    fn said_of(operation: Rehearsed) -> Vec<String> {
        over_the_list(vec![operation])
            .lines()
            .skip(2)
            .take(2)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn a_dry_run_says_each_operation_under_the_list_it_reads_and_over_a_count() {
        let said = over_the_list(vec![
            rehearsed(
                41,
                Some(3),
                1,
                Action::Write {
                    to: of_its_entry(3),
                    before: Some(standing(Status::Planned, 0, 0, None, None)),
                    after: standing(Status::Watching, 3, 0, Some(november(1)), None),
                    undated: true,
                },
            ),
            rehearsed(
                42,
                Some(3),
                2,
                Action::Nothing {
                    to: of_its_entry(3),
                    why: Why::Conflict { progress: 3 },
                },
            ),
            rehearsed(
                43,
                Some(12),
                2,
                Action::Write {
                    to: of_its_entry(12),
                    before: Some(standing(Status::Watching, 3, 0, Some(november(1)), None)),
                    after: standing(
                        Status::Completed,
                        12,
                        0,
                        Some(november(1)),
                        Some(november(2)),
                    ),
                    undated: false,
                },
            ),
            rehearsed(
                44,
                Some(14),
                3,
                Action::Write {
                    to: Destination {
                        entry: EntryId(900_001),
                        episode: 1,
                        by: By::Rule,
                        title: None,
                        episodes: None,
                        listed: false,
                    },
                    before: None,
                    after: standing(Status::Watching, 1, 0, Some(november(3)), None),
                    undated: false,
                },
            ),
            Rehearsed {
                id: 45,
                watched: Some(Watched {
                    day: november(4),
                    title: "Unlisted Show".to_owned(),
                    episode: Some(2),
                }),
                action: Action::CannotSend {
                    to: None,
                    why: Unsent::Unresolved(Unresolved::NotListed),
                },
            },
        ]);

        assert_eq!(
            said,
            [
                "AniList, list as last read: 410 entries",
                "",
                "#41 2026-11-01 \"Show Title\" episode 3 -> AniList 12345 episode 3",
                "    planned 0/12 -> watching 3/12, started 2026-11-01 if list has none",
                "#42 2026-11-02 \"Show Title\" episode 3 -> AniList 12345 episode 3",
                "    nothing to write: list already at 3/12",
                "#43 2026-11-02 \"Show Title\" episode 12 -> AniList 12345 episode 12",
                "    watching 3/12 -> completed 12/12, completed 2026-11-02",
                "#44 2026-11-03 \"Show Title\" episode 14 -> AniList 900001 episode 1, \
                 by anime-relations, not on the list",
                "    adds it: watching 1/?, started 2026-11-03",
                "#45 2026-11-04 \"Unlisted Show\" episode 2",
                "    cannot send: not on the list",
                "",
                "5 operations: 3 writes, 1 nothing to write, 1 cannot send; dry run, nothing sent",
            ]
            .map(|line| format!("{line}\n"))
            .concat()
        );
    }

    #[test]
    fn a_dry_run_with_nothing_past_the_cursor_says_nothing_waits() {
        assert_eq!(
            over_the_list(Vec::new()),
            "AniList, list as last read: 410 entries\n\nnothing waits to be sent\n"
        );
    }

    #[test]
    fn a_list_of_one_entry_or_none_is_counted_in_words() {
        for (entries, header) in [
            (0, "AniList, list as last read: no entries"),
            (1, "AniList, list as last read: 1 entry"),
        ] {
            let said = dry_run(&DryRun {
                entries,
                operations: Vec::new(),
            });

            assert_eq!(said.lines().next(), Some(header));
        }
    }

    #[test]
    fn a_count_of_one_is_said_in_the_singular_and_a_count_of_none_not_at_all() {
        let said = over_the_list(vec![rehearsed(
            41,
            Some(3),
            1,
            Action::Write {
                to: of_its_entry(3),
                before: Some(standing(Status::Watching, 2, 0, Some(november(1)), None)),
                after: standing(Status::Watching, 3, 0, Some(november(1)), None),
                undated: false,
            },
        )]);

        assert_eq!(
            said.lines().last(),
            Some("1 operation: 1 write; dry run, nothing sent")
        );
    }

    #[test]
    fn every_write_says_what_changes_and_no_more() {
        let film = Destination {
            episodes: Some(1),
            ..of_its_entry(1)
        };
        for (to, before, after, written) in [
            (
                of_its_entry(1),
                standing(
                    Status::Completed,
                    12,
                    0,
                    Some(november(1)),
                    Some(november(1)),
                ),
                standing(
                    Status::Rewatching,
                    1,
                    0,
                    Some(november(1)),
                    Some(november(1)),
                ),
                "completed 12/12 -> rewatching 1/12",
            ),
            (
                of_its_entry(12),
                standing(Status::Rewatching, 11, 1, None, None),
                standing(Status::Completed, 12, 2, None, None),
                "rewatching 11/12 -> completed 12/12, rewatches 1 -> 2",
            ),
            (
                film,
                standing(Status::Completed, 1, 0, None, None),
                standing(Status::Completed, 1, 1, None, None),
                "completed 1/1 -> completed 1/1, rewatches 0 -> 1",
            ),
            (
                of_its_entry(5),
                standing(Status::OnHold, 4, 0, None, None),
                standing(Status::Watching, 5, 0, Some(november(1)), None),
                "on hold 4/12 -> watching 5/12, started 2026-11-01 if list has none",
            ),
            (
                of_its_entry(12),
                standing(Status::Dropped, 4, 0, None, None),
                standing(
                    Status::Completed,
                    12,
                    0,
                    Some(november(1)),
                    Some(november(1)),
                ),
                "dropped 4/12 -> completed 12/12, started 2026-11-01 if list has none, \
                 completed 2026-11-01",
            ),
        ] {
            let said = said_of(rehearsed(
                41,
                Some(to.episode),
                1,
                Action::Write {
                    to,
                    before: Some(before),
                    after,
                    undated: true,
                },
            ));

            assert_eq!(said[1], format!("    {written}"));
        }
    }

    #[test]
    fn every_reason_to_write_nothing_is_said() {
        for (why, said) in [
            (
                Why::Completed,
                "nothing to write: completed, rewatch begins at episode 1",
            ),
            (
                Why::PastCount { episodes: 13 },
                "nothing to write: entry has 13 episodes",
            ),
            (
                Why::PastCount { episodes: 1 },
                "nothing to write: entry has 1 episode",
            ),
            (
                Why::EpisodeZero,
                "nothing to write: progress counts from episode 1",
            ),
        ] {
            let lines = said_of(rehearsed(
                41,
                Some(14),
                1,
                Action::Nothing {
                    to: of_its_entry(14),
                    why,
                },
            ));

            assert_eq!(lines[1], format!("    {said}"));
        }
    }

    #[test]
    fn every_reason_an_operation_cannot_be_sent_is_said() {
        for (episode, why, said) in [
            (
                Some(4),
                Unsent::Departed,
                "cannot send: no longer on the list, though it was",
            ),
            (
                None,
                Unsent::Unresolved(Unresolved::NoEpisode { episodes: Some(12) }),
                "cannot send: name spells no episode, entry has 12 episodes",
            ),
            (
                None,
                Unsent::Unresolved(Unresolved::NoEpisode { episodes: None }),
                "cannot send: name spells no episode, entry has no episode count",
            ),
            (
                Some(14),
                Unsent::Unresolved(Unresolved::Outside { episodes: Some(13) }),
                "cannot send: entry has 13 episodes, no rule takes episode 14",
            ),
            (
                Some(0),
                Unsent::Unresolved(Unresolved::Outside { episodes: None }),
                "cannot send: no rule takes episode 0",
            ),
            (
                Some(201),
                Unsent::Unresolved(Unresolved::LaterSeason {
                    season: 2,
                    episode: 1,
                }),
                "cannot send: 201 reads as season 2 episode 1; benshi map can name its entry",
            ),
            (
                Some(14),
                Unsent::Unresolved(Unresolved::Unnumbered),
                "cannot send: anime-relations gives no number for episode 14",
            ),
        ] {
            let lines = said_of(rehearsed(
                41,
                episode,
                1,
                Action::CannotSend { to: None, why },
            ));

            assert_eq!(lines[1], format!("    {said}"));
        }
    }

    #[test]
    fn where_an_operation_goes_is_said_with_what_decided_it() {
        let season_two = |by| Destination {
            entry: EntryId(900_002),
            episode: 3,
            by,
            title: Some("Show Title Season 2".to_owned()),
            episodes: Some(12),
            listed: true,
        };
        for (episode, action, said) in [
            (
                Some(203),
                Action::Nothing {
                    to: season_two(By::Pin),
                    why: Why::Conflict { progress: 3 },
                },
                "#41 2026-11-01 \"Show Title\" episode 203 -> AniList 900002 \
                 \"Show Title Season 2\" episode 3, by benshi map",
            ),
            (
                Some(15),
                Action::Nothing {
                    to: season_two(By::Rule),
                    why: Why::Conflict { progress: 3 },
                },
                "#41 2026-11-01 \"Show Title\" episode 15 -> AniList 900002 \
                 \"Show Title Season 2\" episode 3, by anime-relations",
            ),
            (
                None,
                Action::Nothing {
                    to: Destination {
                        entry: EntryId(678),
                        episodes: Some(1),
                        ..of_its_entry(1)
                    },
                    why: Why::Completed,
                },
                "#41 2026-11-01 \"Show Title\" no episode -> AniList 678 episode 1",
            ),
            (
                Some(4),
                Action::CannotSend {
                    to: Some(Destination {
                        episodes: None,
                        listed: false,
                        ..of_its_entry(4)
                    }),
                    why: Unsent::Departed,
                },
                "#41 2026-11-01 \"Show Title\" episode 4 -> AniList 12345 episode 4",
            ),
        ] {
            let lines = said_of(rehearsed(41, episode, 1, action));

            assert_eq!(lines[0], said);
        }
    }

    #[test]
    fn a_row_that_holds_no_operation_is_said_with_why() {
        let lines = said_of(Rehearsed {
            id: 48,
            watched: None,
            action: Action::CannotSend {
                to: None,
                why: Unsent::Unread(
                    "queued operation 48 is for a show that is not there".to_owned(),
                ),
            },
        });

        assert_eq!(
            lines,
            [
                "#48 cannot be read",
                "    cannot send: queued operation 48 is for a show that is not there",
            ]
        );
    }
}
