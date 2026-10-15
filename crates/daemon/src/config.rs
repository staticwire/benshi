//! The settings a user writes, in `config.toml` in the configuration
//! directory [`crate::paths`] names.
//!
//! ```
//! # let settings: benshi_daemon::config::Settings = toml::from_str(r##"
//! [send]
//! when = "each"     # or "close", "interval", "manual"
//! interval = "15m"  # how often "interval" sends
//! at_start = true
//! at_stop = false
//!
//! [list]
//! refresh = "1h"    # or "off"
//! at_start = true
//! on_miss = "5m"    # or "off"
//! # "##).unwrap();
//! # assert_eq!(settings, benshi_daemon::config::Settings::default());
//! ```
//!
//! That is the file of every default. A key or a table it leaves out has its
//! default, and so does a file that is not there or a configuration directory
//! that is not named. A length of time is written as `humantime` reads it,
//! `15m`, `1h` or `1h 30m`, and is a minute at the least.
//!
//! **A file that cannot be used stops the daemon**: one that cannot be read
//! for any reason but that it is not there, TOML that does not parse, a key or
//! a table this build does not know, and a value a key does not take. What the
//! daemon says names the line and the column where `toml` gives a place.
//! Running on the defaults in its place would set aside what the file says,
//! `when = "manual"` among it. A key spelt wrong is refused for the same
//! reason, where it would be read as one left out.

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

use crate::paths::Nowhere;

/// The name of the file in the configuration directory.
pub const FILE: &str = "config.toml";

/// The shortest length of time a key takes.
const SHORTEST: Duration = Duration::from_mins(1);

/// What a key that can be turned off takes in place of a length of time.
const OFF: &str = "off";

/// A length of time, as a message names what it expected.
const A_LENGTH: &str = "a length of time, such as `15m` or `1h`";

/// What the file says, and the default of whatever it leaves out.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `[send]`.
    pub send: Sending,
    /// `[list]`.
    pub list: Reading,
}

/// `[send]`: when what was seen is sent to the list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sending {
    /// `when`: `"each"` where the file does not say.
    pub when: When,
    /// `interval`, which `"interval"` sends at the end of: 15 minutes where
    /// the file does not say.
    #[serde(deserialize_with = "length")]
    pub interval: Duration,
    /// `at_start`, which asks for what waits to be sent as the daemon
    /// starts: true where the file does not say.
    pub at_start: bool,
    /// `at_stop`, which asks for what waits to be sent as the daemon stops:
    /// false where the file does not say.
    pub at_stop: bool,
}

impl Default for Sending {
    fn default() -> Self {
        Self {
            when: When::Each,
            interval: Duration::from_mins(15),
            at_start: true,
            at_stop: false,
        }
    }
}

/// `when` of `[send]`: what a viewing is sent at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// `"each"`: each viewing, as it is recorded.
    Each,
    /// `"close"`: the player closing.
    Close,
    /// `"interval"`: the end of each `interval`.
    Interval,
    /// `"manual"`: a user asking.
    Manual,
}

/// `[list]`: when the list is read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reading {
    /// `refresh`, which asks for the list to be read this often: an hour
    /// where the file does not say.
    #[serde(deserialize_with = "length_or_off")]
    pub refresh: Period,
    /// `at_start`, which asks for the list to be read as the daemon starts:
    /// true where the file does not say.
    pub at_start: bool,
    /// `on_miss`, for reading the list again where a name misses it: 5
    /// minutes where the file does not say.
    #[serde(deserialize_with = "length_or_off")]
    pub on_miss: Period,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            refresh: Period::Every(Duration::from_hours(1)),
            at_start: true,
            on_miss: Period::Every(Duration::from_mins(5)),
        }
    }
}

/// A length of time, or `"off"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// A length of time.
    Every(Duration),
    /// `"off"`.
    Off,
}

/// Where the settings were read from, as the daemon says it.
#[derive(Debug, PartialEq, Eq)]
pub enum Origin {
    /// The file at this path.
    File(PathBuf),
    /// No file at this path, so the defaults.
    NoFile(PathBuf),
    /// No configuration directory, so the defaults.
    NoDirectory(Nowhere),
}

impl fmt::Display for Origin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(file) => write!(formatter, "settings from {}", file.display()),
            Self::NoFile(file) => write!(formatter, "default settings: no {}", file.display()),
            Self::NoDirectory(nowhere) => write!(formatter, "default settings: {nowhere}"),
        }
    }
}

/// A file that cannot be used.
#[derive(Debug, thiserror::Error)]
#[error("{} cannot be used: {cause}", .file.display())]
pub struct Unusable {
    /// The file.
    pub file: PathBuf,
    /// Why it cannot be used.
    pub cause: Cause,
}

/// Why a file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum Cause {
    /// It cannot be read.
    #[error("{0}")]
    Unread(io::Error),
    /// What it holds is not settings, at a place in it.
    #[error("line {line}, column {column}: {message}")]
    Misread {
        /// The line, the first being 1.
        line: usize,
        /// The column on that line, in characters, the first being 1.
        column: usize,
        /// What is wrong there.
        message: String,
    },
    /// What it holds is not settings, at no place `toml` names.
    #[error("{message}")]
    Unplaced {
        /// What is wrong.
        message: String,
    },
}

/// The settings in [`FILE`] in `directory`, and where they were read from.
///
/// # Errors
///
/// [`Unusable`] where reading the file fails for any reason but that it is not
/// there, or what it holds is not settings.
pub fn load(directory: Result<PathBuf, Nowhere>) -> Result<(Settings, Origin), Unusable> {
    let file = match directory {
        Ok(directory) => directory.join(FILE),
        Err(nowhere) => return Ok((Settings::default(), Origin::NoDirectory(nowhere))),
    };
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(unread) if unread.kind() == io::ErrorKind::NotFound => {
            return Ok((Settings::default(), Origin::NoFile(file)));
        }
        Err(unread) => {
            return Err(Unusable {
                file,
                cause: Cause::Unread(unread),
            });
        }
    };

    match toml::from_str(&text) {
        Ok(settings) => Ok((settings, Origin::File(file))),
        Err(misread) => Err(Unusable {
            file,
            cause: placed(&text, &misread),
        }),
    }
}

/// Why `text` is not settings, at the place in it `misread` names.
fn placed(text: &str, misread: &toml::de::Error) -> Cause {
    let message = misread.message().to_owned();
    let Some(span) = misread.span() else {
        return Cause::Unplaced { message };
    };
    let before = &text.as_bytes()[..span.start];
    let on_its_line = match before.iter().rposition(|&byte| byte == b'\n') {
        Some(newline) => &before[newline + 1..],
        None => before,
    };

    Cause::Misread {
        // One piece more than there are newlines before the place.
        line: before.split(|&byte| byte == b'\n').count(),
        // In characters, counted by the bytes that begin one.
        column: on_its_line
            .iter()
            .filter(|&&byte| byte & 0b1100_0000 != 0b1000_0000)
            .count()
            + 1,
        message,
    }
}

/// A length of time, a minute at the least.
fn length<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    deserializer.deserialize_str(Length)
}

/// A length of time, a minute at the least, or `"off"`.
fn length_or_off<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Period, D::Error> {
    deserializer.deserialize_str(LengthOrOff)
}

struct Length;

impl Visitor<'_> for Length {
    type Value = Duration;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(A_LENGTH)
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Duration, E> {
        length_in(text, &self).map_err(E::custom)
    }
}

struct LengthOrOff;

impl Visitor<'_> for LengthOrOff {
    type Value = Period;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{A_LENGTH}, or `{OFF}`")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Period, E> {
        if text == OFF {
            return Ok(Period::Off);
        }

        length_in(text, &self).map(Period::Every).map_err(E::custom)
    }
}

/// The length of time `text` says, where it says one of a minute or more. A
/// text that says none is told what the key takes, which is `expected`.
fn length_in(text: &str, expected: &dyn de::Expected) -> Result<Duration, String> {
    let length =
        humantime::parse_duration(text).map_err(|_unread| format!("`{text}` is not {expected}"))?;
    if length < SHORTEST {
        return Err(format!("`{text}` is less than a minute"));
    }

    Ok(length)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::paths::Role;

    /// What a directory whose file holds `text` is read as: the settings, or
    /// what the daemon says of the file after its path.
    fn read(text: impl AsRef<[u8]>) -> Result<Settings, String> {
        let directory = tempfile::tempdir().expect("a directory");
        let file = directory.path().join(FILE);
        fs::write(&file, text).expect("the file is written");

        match load(Ok(directory.path().to_owned())) {
            Ok((settings, origin)) => {
                assert_eq!(origin, Origin::File(file));
                Ok(settings)
            }
            Err(unusable) => Err(cause_of(&unusable, &file)),
        }
    }

    /// What the daemon says of a file that cannot be used, with the path at
    /// its head taken off.
    fn cause_of(unusable: &Unusable, file: &Path) -> String {
        let line = unusable.to_string();
        let head = format!("{} cannot be used: ", file.display());

        line.strip_prefix(&head)
            .unwrap_or_else(|| panic!("{line:?} does not begin with {head:?}"))
            .to_owned()
    }

    fn refused(cause: &str) -> Result<Settings, String> {
        Err(cause.to_owned())
    }

    #[test]
    fn each_default_is_pinned() {
        assert_eq!(
            Settings::default(),
            Settings {
                send: Sending {
                    when: When::Each,
                    interval: Duration::from_mins(15),
                    at_start: true,
                    at_stop: false,
                },
                list: Reading {
                    refresh: Period::Every(Duration::from_hours(1)),
                    at_start: true,
                    on_miss: Period::Every(Duration::from_mins(5)),
                },
            }
        );
    }

    #[test]
    fn no_file_is_the_defaults_and_its_path_is_said() {
        let directory = tempfile::tempdir().expect("a directory");
        let file = directory.path().join(FILE);

        let (settings, origin) =
            load(Ok(directory.path().to_owned())).expect("no file is the defaults");

        assert_eq!(settings, Settings::default());
        assert_eq!(
            origin.to_string(),
            format!("default settings: no {}", file.display())
        );
    }

    #[test]
    fn no_configuration_directory_is_the_defaults_and_why_is_said() {
        let nowhere = Nowhere::NotFound {
            role: Role::Config,
            looked_in: vec!["XDG_CONFIG_HOME", "HOME"],
        };

        let (settings, origin) = load(Err(nowhere)).expect("no directory is the defaults");

        assert_eq!(settings, Settings::default());
        assert_eq!(
            origin.to_string(),
            "default settings: no configuration directory: no absolute path in \
             `XDG_CONFIG_HOME` or `HOME`"
        );
    }

    #[test]
    fn a_file_read_is_said_by_its_path() {
        let file = PathBuf::from("/home/viewer/.config/benshi/config.toml");

        assert_eq!(
            Origin::File(file).to_string(),
            "settings from /home/viewer/.config/benshi/config.toml"
        );
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(read(""), Ok(Settings::default()));
    }

    #[test]
    fn every_key_is_read() {
        let file = "[send]\nwhen = \"interval\"\ninterval = \"30m\"\nat_start = false\n\
                    at_stop = true\n\n[list]\nrefresh = \"off\"\nat_start = false\n\
                    on_miss = \"10m\"\n";

        assert_eq!(
            read(file),
            Ok(Settings {
                send: Sending {
                    when: When::Interval,
                    interval: Duration::from_mins(30),
                    at_start: false,
                    at_stop: true,
                },
                list: Reading {
                    refresh: Period::Off,
                    at_start: false,
                    on_miss: Period::Every(Duration::from_mins(10)),
                },
            })
        );
    }

    #[test]
    fn a_key_left_out_keeps_its_default() {
        assert_eq!(
            read("[send]\nat_stop = true\n"),
            Ok(Settings {
                send: Sending {
                    at_stop: true,
                    ..Sending::default()
                },
                ..Settings::default()
            })
        );
        assert_eq!(
            read("[list]\nat_start = false\n"),
            Ok(Settings {
                list: Reading {
                    at_start: false,
                    ..Reading::default()
                },
                ..Settings::default()
            })
        );
    }

    #[test]
    fn each_word_when_takes_is_read() {
        for (word, when) in [
            ("each", When::Each),
            ("close", When::Close),
            ("interval", When::Interval),
            ("manual", When::Manual),
        ] {
            let read = read(format!("[send]\nwhen = \"{word}\"\n"));

            assert_eq!(read.map(|settings| settings.send.when), Ok(when), "{word}");
        }
    }

    #[test]
    fn a_miss_can_be_set_to_read_nothing() {
        let read = read("[list]\non_miss = \"off\"\n");

        assert_eq!(read.map(|settings| settings.list.on_miss), Ok(Period::Off));
    }

    #[test]
    fn a_length_is_read_as_humantime_reads_it() {
        for written in ["1h 30m", "1h30m", "90min", "5400s"] {
            let read = read(format!("[list]\nrefresh = \"{written}\"\n"));

            assert_eq!(
                read.map(|settings| settings.list.refresh),
                Ok(Period::Every(Duration::from_mins(90))),
                "{written}"
            );
        }
    }

    #[test]
    fn a_minute_is_taken_by_each_length() {
        let minute = Duration::from_mins(1);

        let interval = read("[send]\ninterval = \"1m\"\n");
        let refresh = read("[list]\nrefresh = \"1m\"\n");
        let on_miss = read("[list]\non_miss = \"1m\"\n");

        assert_eq!(interval.map(|settings| settings.send.interval), Ok(minute));
        assert_eq!(
            refresh.map(|settings| settings.list.refresh),
            Ok(Period::Every(minute))
        );
        assert_eq!(
            on_miss.map(|settings| settings.list.on_miss),
            Ok(Period::Every(minute))
        );
    }

    #[test]
    fn a_length_under_a_minute_is_refused() {
        assert_eq!(
            read("[send]\ninterval = \"59s\"\n"),
            refused("line 2, column 12: `59s` is less than a minute")
        );
        assert_eq!(
            read("[list]\nrefresh = \"59s\"\n"),
            refused("line 2, column 11: `59s` is less than a minute")
        );
        assert_eq!(
            read("[list]\non_miss = \"0s\"\n"),
            refused("line 2, column 11: `0s` is less than a minute")
        );
    }

    #[test]
    fn text_that_is_not_a_length_is_refused() {
        assert_eq!(
            read("[send]\ninterval = \"fifteen\"\n"),
            refused("line 2, column 12: `fifteen` is not a length of time, such as `15m` or `1h`")
        );
        assert_eq!(
            read("[send]\ninterval = \"off\"\n"),
            refused("line 2, column 12: `off` is not a length of time, such as `15m` or `1h`")
        );
        assert_eq!(
            read("[list]\non_miss = \"never\"\n"),
            refused(
                "line 2, column 11: `never` is not a length of time, such as `15m` or `1h`, \
                 or `off`"
            )
        );
    }

    #[test]
    fn a_number_for_a_length_is_refused_with_what_was_expected() {
        assert_eq!(
            read("[send]\ninterval = 900\n"),
            refused(
                "line 2, column 12: invalid type: integer `900`, expected a length of time, \
                 such as `15m` or `1h`"
            )
        );
        assert_eq!(
            read("[list]\nrefresh = 3600\n"),
            refused(
                "line 2, column 11: invalid type: integer `3600`, expected a length of time, \
                 such as `15m` or `1h`, or `off`"
            )
        );
    }

    #[test]
    fn a_key_this_build_does_not_know_is_refused() {
        assert_eq!(
            read("[list]\nrefesh = \"1h\"\n"),
            refused(
                "line 2, column 1: unknown field `refesh`, expected one of `refresh`, \
                 `at_start`, `on_miss`"
            )
        );
        assert_eq!(
            read("[send]\nat_end = true\n"),
            refused(
                "line 2, column 1: unknown field `at_end`, expected one of `when`, \
                 `interval`, `at_start`, `at_stop`"
            )
        );
    }

    #[test]
    fn a_table_this_build_does_not_know_is_refused() {
        assert_eq!(
            read("[lists]\nrefresh = \"1h\"\n"),
            refused("line 1, column 2: unknown field `lists`, expected `send` or `list`")
        );
    }

    #[test]
    fn a_word_when_does_not_take_is_refused() {
        assert_eq!(
            read("[send]\nwhen = \"sometimes\"\n"),
            refused(
                "line 2, column 8: unknown variant `sometimes`, expected one of `each`, \
                 `close`, `interval`, `manual`"
            )
        );
    }

    #[test]
    fn true_or_false_written_as_text_is_refused() {
        assert_eq!(
            read("[list]\nat_start = \"yes\"\n"),
            refused("line 2, column 12: invalid type: string \"yes\", expected a boolean")
        );
    }

    #[test]
    fn toml_that_does_not_parse_is_refused_at_its_place() {
        assert_eq!(
            read("[list\nrefresh = \"1h\"\n"),
            refused("line 1, column 6: unclosed table, expected `]`")
        );
    }

    #[test]
    fn a_place_is_counted_in_characters() {
        let cause = read("# 送る\n[send]\nwhen = \"毎回\\q\"\n")
            .expect_err("an escape that TOML does not know");

        // The `q` the escape is missing at is the twelfth character of its
        // line and its sixteenth byte.
        assert!(cause.starts_with("line 3, column 12: "), "{cause}");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_refused_with_why() {
        let directory = tempfile::tempdir().expect("a directory");
        let file = directory.path().join(FILE);
        fs::create_dir(&file).expect("a directory where the file goes");
        let why = fs::read_to_string(&file)
            .expect_err("a directory is not read as text")
            .to_string();

        let unusable =
            load(Ok(directory.path().to_owned())).expect_err("a directory is no file of settings");

        assert_eq!(cause_of(&unusable, &file), why);
    }

    #[test]
    fn a_file_that_is_not_utf_8_is_refused_with_why() {
        assert_eq!(read(b"\xff"), refused("stream did not contain valid UTF-8"));
    }

    #[test]
    fn a_cause_toml_gives_no_place_is_said_with_none() {
        let unplaced = <toml::de::Error as de::Error>::custom("not settings");

        assert_eq!(placed("when = 1\n", &unplaced).to_string(), "not settings");
    }
}
