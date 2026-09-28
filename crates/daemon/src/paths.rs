//! Where the daemon keeps its files.
//!
//! One directory for each [`Role`], named from what the environment holds by
//! the rules of the platform the build is for:
//!
//! | Platform | Data | Configuration |
//! | --- | --- | --- |
//! | Linux | `$XDG_DATA_HOME/benshi`, else `$HOME/.local/share/benshi` | `$XDG_CONFIG_HOME/benshi`, else `$HOME/.config/benshi` |
//! | macOS | `$HOME/Library/Application Support/benshi` | the same directory |
//! | Windows | `%LOCALAPPDATA%\benshi` | `%APPDATA%\benshi` |
//!
//! A build for any other system reads the row for Linux. On Windows the
//! database stays out of `%APPDATA%`, which is the part of a profile that
//! roams between machines.
//!
//! **`BENSHI_DATA_DIR` is the data directory itself** on every platform, with
//! nothing added beneath it, and it is the one override. The database is
//! written through a write-ahead log, which does not work over a network
//! filesystem, so a home directory mounted from one needs the data on a local
//! disk. Configuration has no override.
//!
//! **A `~` at the head of `BENSHI_DATA_DIR` stands for the home directory**,
//! which is `$HOME`, and `%USERPROFILE%` on Windows. A shell is what reads
//! `~`, so a value no shell has read, one a service manager set or one written
//! between quotes, arrives with the `~` still in it. `~name`, the home of
//! another user, is in no variable and is a relative path like any other.
//! Where the home directory is in no variable, the answer is
//! [`Nowhere::NoHome`].
//!
//! **A variable counts where it holds an absolute path.** An empty one is read
//! as one never set and a relative one is passed over, `~/data` included. The
//! XDG Base Directory Specification asks that for its own variables, and the
//! other variables a default is read from are read the same way: a relative
//! path names another directory every time the daemon is started somewhere
//! else. In `BENSHI_DATA_DIR` a relative path that is not beneath `~` is
//! refused with [`Nowhere::Relative`]. Whoever set it wants the data kept off
//! the default, and passing it over would put the data exactly there.
//!
//! Nothing here touches the filesystem. What is answered is a name, and
//! [`benshi_store::Store::open`] makes the directory of the file it is handed.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

/// The variable that holds the data directory itself.
const OVERRIDE: &str = "BENSHI_DATA_DIR";

/// What stands for the home directory at the head of the override.
const MARK: &str = "~";

/// The directory every default ends in.
const OURS: &str = "benshi";

/// What a directory is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Where the database goes.
    Data,
    /// Where configuration goes.
    Config,
}

// Written so that it reads inside the sentence an error is.
impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Data => "data",
            Self::Config => "configuration",
        })
    }
}

/// Why there is no directory to answer with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Nowhere {
    /// `BENSHI_DATA_DIR` holds a relative path that is not beneath `~`.
    #[error(
        "`{OVERRIDE}` holds `{}`, which is a relative path: the data directory is an absolute \
         path or a path beneath `{MARK}`",
        .given.display()
    )]
    Relative {
        /// What the variable holds.
        given: PathBuf,
    },
    /// `BENSHI_DATA_DIR` holds a path beneath `~` and the home directory is
    /// in no variable.
    #[error(
        "`{OVERRIDE}` holds `{}` and `{home}` holds no absolute path for `{MARK}` to stand for",
        .given.display()
    )]
    NoHome {
        /// What the variable holds.
        given: PathBuf,
        /// The variable the home directory is read from.
        home: &'static str,
    },
    /// No variable the role is read from holds an absolute path.
    #[error("no {role} directory: no absolute path in {}", listed(.looked_in))]
    NotFound {
        /// The directory that was asked for.
        role: Role,
        /// Every variable that was read, in the order they were read in.
        looked_in: Vec<&'static str>,
    },
}

/// The variables as an error names them, the last after an "or".
fn listed(variables: &[&str]) -> String {
    let named: Vec<_> = variables
        .iter()
        .map(|variable| format!("`{variable}`"))
        .collect();

    match named.as_slice() {
        [earlier @ .., last] if !earlier.is_empty() => {
            format!("{} or {last}", earlier.join(", "))
        }
        alone => alone.concat(),
    }
}

/// The directory for a role, read from the environment of this process by the
/// rules of the platform this build is for.
///
/// # Errors
///
/// Where the data directory is asked for and `BENSHI_DATA_DIR` holds a
/// relative path: [`Nowhere::NoHome`] for one beneath `~` with the home
/// directory in no variable, and [`Nowhere::Relative`] for any other.
/// [`Nowhere::NotFound`] where no variable the role is read from holds an
/// absolute path.
pub fn directory(role: Role) -> Result<PathBuf, Nowhere> {
    directory_from(role, Platform::HERE, |variable| env::var_os(variable))
}

/// Whose rules a directory is named by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    /// The XDG Base Directory Specification.
    Linux,
    /// The library in the home directory.
    MacOs,
    /// The two application data directories of a profile.
    Windows,
}

/// One place a default is read from: a variable, and the directories between
/// the path it holds and the one that is ours.
type Source = (&'static str, &'static [&'static str]);

impl Platform {
    /// The platform this build is for.
    const HERE: Self = if cfg!(target_os = "macos") {
        Self::MacOs
    } else if cfg!(windows) {
        Self::Windows
    } else {
        Self::Linux
    };

    /// The variable that holds the home directory.
    const fn home(self) -> &'static str {
        match self {
            Self::Linux | Self::MacOs => "HOME",
            Self::Windows => "USERPROFILE",
        }
    }

    /// Where the default for a role is read from, in the order it is read in.
    /// The first variable that holds an absolute path decides.
    const fn sources(self, role: Role) -> &'static [Source] {
        match (self, role) {
            (Self::Linux, Role::Data) => &[("XDG_DATA_HOME", &[]), ("HOME", &[".local", "share"])],
            (Self::Linux, Role::Config) => &[("XDG_CONFIG_HOME", &[]), ("HOME", &[".config"])],
            (Self::MacOs, Role::Data | Role::Config) => {
                &[("HOME", &["Library", "Application Support"])]
            }
            (Self::Windows, Role::Data) => &[("LOCALAPPDATA", &[])],
            (Self::Windows, Role::Config) => &[("APPDATA", &[])],
        }
    }
}

/// The directory for a role, given a platform and a way to read a variable.
fn directory_from(
    role: Role,
    platform: Platform,
    read: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, Nowhere> {
    let overriding = match role {
        Role::Data => Some(OVERRIDE),
        Role::Config => None,
    };
    if let Some(variable) = overriding
        && let Some(given) = read(variable).filter(|given| !given.is_empty())
    {
        let given = PathBuf::from(given);

        return if given.is_absolute() {
            Ok(given)
        } else if let Ok(beneath) = given.strip_prefix(MARK) {
            let home = platform.home();

            match absolute(&read, home) {
                Some(mut directory) => {
                    // By the component, so that the mark on its own leaves the
                    // home directory with no separator at its end.
                    directory.extend(beneath.components());

                    Ok(directory)
                }
                None => Err(Nowhere::NoHome { given, home }),
            }
        } else {
            Err(Nowhere::Relative { given })
        };
    }

    let sources = platform.sources(role);
    sources
        .iter()
        .find_map(|(variable, beneath)| {
            let mut directory = absolute(&read, variable)?;
            directory.extend(*beneath);
            directory.push(OURS);

            Some(directory)
        })
        .ok_or_else(|| Nowhere::NotFound {
            role,
            looked_in: overriding
                .into_iter()
                .chain(sources.iter().map(|(variable, _)| *variable))
                .collect(),
        })
}

/// The absolute path a variable holds, where it holds one. `is_absolute` is
/// false for an empty path, so an empty variable holds none.
fn absolute(read: &impl Fn(&str) -> Option<OsString>, variable: &str) -> Option<PathBuf> {
    read(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;

    use super::*;

    const EVERY_PLATFORM: [Platform; 3] = [Platform::Linux, Platform::MacOs, Platform::Windows];

    /// Set in a run of the tests that a test started, and in no other.
    #[cfg(unix)]
    const RUN_AGAIN: &str = "BENSHI_PATHS_RUN_AGAIN";

    /// A path that is absolute on the system the tests run on.
    fn rooted(beneath: &str) -> PathBuf {
        let root = if cfg!(windows) { r"C:\" } else { "/" };

        Path::new(root).join(beneath)
    }

    /// An absolute path that is not Unicode.
    #[cfg(unix)]
    fn not_text() -> PathBuf {
        use std::os::unix::ffi::OsStrExt;

        rooted("mnt").join(OsStr::from_bytes(b"disk\xff"))
    }

    /// Every variable that is read on any platform but the override, each
    /// holding a directory of its own.
    fn a_session() -> Vec<(&'static str, PathBuf)> {
        vec![
            ("XDG_DATA_HOME", rooted("xdg/data")),
            ("XDG_CONFIG_HOME", rooted("xdg/config")),
            ("HOME", rooted("home/viewer")),
            ("USERPROFILE", rooted("viewer")),
            ("LOCALAPPDATA", rooted("viewer/local")),
            ("APPDATA", rooted("viewer/roaming")),
        ]
    }

    /// The home directory of the session, as a platform reads it.
    fn home_on(platform: Platform) -> PathBuf {
        match platform {
            Platform::Linux | Platform::MacOs => rooted("home/viewer"),
            Platform::Windows => rooted("viewer"),
        }
    }

    /// The session with these variables holding these paths, whether it set
    /// them before or not.
    fn a_session_with(changed: &[(&'static str, PathBuf)]) -> Vec<(&'static str, PathBuf)> {
        let mut session = a_session_without(
            &changed
                .iter()
                .map(|(variable, _)| *variable)
                .collect::<Vec<_>>(),
        );
        session.extend_from_slice(changed);

        session
    }

    /// The session with these variables never set.
    fn a_session_without(unset: &[&str]) -> Vec<(&'static str, PathBuf)> {
        let mut session = a_session();
        session.retain(|(variable, _)| !unset.contains(variable));

        session
    }

    /// Read a variable from these and from nowhere else.
    fn holding<'set>(
        set: &'set [(&'static str, PathBuf)],
    ) -> impl Fn(&str) -> Option<OsString> + 'set {
        move |asked| {
            set.iter()
                .find(|(variable, _)| *variable == asked)
                .map(|(_, held)| held.clone().into_os_string())
        }
    }

    fn not_found(role: Role, looked_in: &[&'static str]) -> Result<PathBuf, Nowhere> {
        Err(Nowhere::NotFound {
            role,
            looked_in: looked_in.to_vec(),
        })
    }

    #[test]
    fn on_linux_data_goes_beneath_the_xdg_data_home() {
        let found = directory_from(Role::Data, Platform::Linux, holding(&a_session()));

        assert_eq!(found, Ok(rooted("xdg/data/benshi")));
    }

    #[test]
    fn on_linux_configuration_goes_beneath_the_xdg_config_home() {
        let found = directory_from(Role::Config, Platform::Linux, holding(&a_session()));

        assert_eq!(found, Ok(rooted("xdg/config/benshi")));
    }

    #[test]
    fn on_linux_data_goes_beneath_home_where_no_xdg_data_home_is_set() {
        let session = a_session_without(&["XDG_DATA_HOME"]);

        let found = directory_from(Role::Data, Platform::Linux, holding(&session));

        assert_eq!(found, Ok(rooted("home/viewer/.local/share/benshi")));
    }

    #[test]
    fn on_linux_configuration_goes_beneath_home_where_no_xdg_config_home_is_set() {
        let session = a_session_without(&["XDG_CONFIG_HOME"]);

        let found = directory_from(Role::Config, Platform::Linux, holding(&session));

        assert_eq!(found, Ok(rooted("home/viewer/.config/benshi")));
    }

    #[test]
    fn on_macos_both_roles_share_a_directory_beneath_the_library() {
        let expected = rooted("home/viewer/Library/Application Support/benshi");

        for role in [Role::Data, Role::Config] {
            let found = directory_from(role, Platform::MacOs, holding(&a_session()));

            assert_eq!(found, Ok(expected.clone()), "{role:?}");
        }
    }

    #[test]
    fn on_windows_data_goes_beneath_the_local_application_data() {
        let found = directory_from(Role::Data, Platform::Windows, holding(&a_session()));

        assert_eq!(found, Ok(rooted("viewer/local/benshi")));
    }

    #[test]
    fn on_windows_configuration_goes_beneath_the_roaming_application_data() {
        let found = directory_from(Role::Config, Platform::Windows, holding(&a_session()));

        assert_eq!(found, Ok(rooted("viewer/roaming/benshi")));
    }

    #[test]
    fn the_override_is_the_data_directory_on_every_platform() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", rooted("mnt/disk"))]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));

            assert_eq!(found, Ok(rooted("mnt/disk")), "{platform:?}");
        }
    }

    #[test]
    fn the_override_leaves_the_configuration_where_it_was() {
        for given in [
            rooted("mnt/disk"),
            PathBuf::from("data/here"),
            PathBuf::from("~/.benshi"),
        ] {
            let session = a_session_with(&[("BENSHI_DATA_DIR", given.clone())]);

            for platform in EVERY_PLATFORM {
                let found = directory_from(Role::Config, platform, holding(&session));
                let without = directory_from(Role::Config, platform, holding(&a_session()));

                assert!(found.is_ok(), "{platform:?} with {given:?}");
                assert_eq!(found, without, "{platform:?} with {given:?}");
            }
        }
    }

    #[test]
    fn an_empty_override_is_read_as_one_never_set() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", PathBuf::new())]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));
            let without = directory_from(Role::Data, platform, holding(&a_session()));

            assert!(found.is_ok(), "{platform:?}");
            assert_eq!(found, without, "{platform:?}");
        }
    }

    #[test]
    fn a_relative_override_is_refused_whatever_else_is_set() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", PathBuf::from("data/here"))]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));

            assert_eq!(
                found,
                Err(Nowhere::Relative {
                    given: PathBuf::from("data/here")
                }),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn a_mark_at_the_head_of_the_override_stands_for_the_home_directory() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", PathBuf::from("~/.benshi"))]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));

            assert_eq!(found, Ok(home_on(platform).join(".benshi")), "{platform:?}");
        }
    }

    #[test]
    fn a_mark_on_its_own_is_the_home_directory_as_it_is_held() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", PathBuf::from("~"))]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));

            // As text, because two paths are equal where one of them ends in
            // a separator.
            assert_eq!(
                found.map(PathBuf::into_os_string),
                Ok(home_on(platform).into_os_string()),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn a_mark_with_a_name_after_it_is_a_relative_path() {
        let session = a_session_with(&[("BENSHI_DATA_DIR", PathBuf::from("~viewer/data"))]);

        for platform in EVERY_PLATFORM {
            let found = directory_from(Role::Data, platform, holding(&session));

            assert_eq!(
                found,
                Err(Nowhere::Relative {
                    given: PathBuf::from("~viewer/data")
                }),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn a_mark_with_no_home_to_stand_for_is_refused() {
        let homes = [
            (Platform::Linux, "HOME"),
            (Platform::MacOs, "HOME"),
            (Platform::Windows, "USERPROFILE"),
        ];

        for (platform, home) in homes {
            for held in [None, Some(PathBuf::new()), Some(PathBuf::from("viewer"))] {
                let mut session = a_session_without(&[home]);
                session.push(("BENSHI_DATA_DIR", PathBuf::from("~/.benshi")));
                session.extend(held.clone().map(|held| (home, held)));

                let found = directory_from(Role::Data, platform, holding(&session));

                assert_eq!(
                    found,
                    Err(Nowhere::NoHome {
                        given: PathBuf::from("~/.benshi"),
                        home,
                    }),
                    "{platform:?} with {held:?}"
                );
            }
        }
    }

    #[test]
    fn a_mark_is_read_in_the_override_and_in_no_default() {
        let session = a_session_with(&[
            ("XDG_DATA_HOME", PathBuf::from("~/data")),
            ("XDG_CONFIG_HOME", PathBuf::from("~/config")),
        ]);

        let data = directory_from(Role::Data, Platform::Linux, holding(&session));
        let config = directory_from(Role::Config, Platform::Linux, holding(&session));

        assert_eq!(data, Ok(rooted("home/viewer/.local/share/benshi")));
        assert_eq!(config, Ok(rooted("home/viewer/.config/benshi")));
    }

    #[test]
    fn an_empty_xdg_directory_is_read_as_one_never_set() {
        let session = a_session_with(&[
            ("XDG_DATA_HOME", PathBuf::new()),
            ("XDG_CONFIG_HOME", PathBuf::new()),
        ]);

        let data = directory_from(Role::Data, Platform::Linux, holding(&session));
        let config = directory_from(Role::Config, Platform::Linux, holding(&session));

        assert_eq!(data, Ok(rooted("home/viewer/.local/share/benshi")));
        assert_eq!(config, Ok(rooted("home/viewer/.config/benshi")));
    }

    #[test]
    fn a_relative_xdg_directory_is_passed_over() {
        let session = a_session_with(&[
            ("XDG_DATA_HOME", PathBuf::from("xdg/data")),
            ("XDG_CONFIG_HOME", PathBuf::from("xdg/config")),
        ]);

        let data = directory_from(Role::Data, Platform::Linux, holding(&session));
        let config = directory_from(Role::Config, Platform::Linux, holding(&session));

        assert_eq!(data, Ok(rooted("home/viewer/.local/share/benshi")));
        assert_eq!(config, Ok(rooted("home/viewer/.config/benshi")));
    }

    #[test]
    fn a_home_that_is_empty_or_relative_is_no_home() {
        for home in [PathBuf::new(), PathBuf::from("home/viewer")] {
            let session = [("HOME", home.clone())];

            let found = directory_from(Role::Config, Platform::MacOs, holding(&session));

            assert_eq!(found, not_found(Role::Config, &["HOME"]), "{home:?}");
        }
    }

    #[test]
    fn on_macos_the_xdg_directories_are_not_read() {
        let session = a_session_without(&["HOME"]);

        let data = directory_from(Role::Data, Platform::MacOs, holding(&session));
        let config = directory_from(Role::Config, Platform::MacOs, holding(&session));

        assert_eq!(data, not_found(Role::Data, &["BENSHI_DATA_DIR", "HOME"]));
        assert_eq!(config, not_found(Role::Config, &["HOME"]));
    }

    #[test]
    fn on_windows_data_is_kept_out_of_the_roaming_directory() {
        let session = a_session_without(&["LOCALAPPDATA"]);

        let found = directory_from(Role::Data, Platform::Windows, holding(&session));

        assert_eq!(
            found,
            not_found(Role::Data, &["BENSHI_DATA_DIR", "LOCALAPPDATA"])
        );
    }

    #[test]
    fn on_windows_configuration_is_kept_out_of_the_local_directory() {
        let session = a_session_without(&["APPDATA"]);

        let found = directory_from(Role::Config, Platform::Windows, holding(&session));

        assert_eq!(found, not_found(Role::Config, &["APPDATA"]));
    }

    #[test]
    fn with_nothing_set_linux_names_everything_it_read() {
        let data = directory_from(Role::Data, Platform::Linux, holding(&[]));
        let config = directory_from(Role::Config, Platform::Linux, holding(&[]));

        assert_eq!(
            data,
            not_found(Role::Data, &["BENSHI_DATA_DIR", "XDG_DATA_HOME", "HOME"])
        );
        assert_eq!(
            config,
            not_found(Role::Config, &["XDG_CONFIG_HOME", "HOME"])
        );
    }

    #[test]
    fn a_directory_that_was_not_found_says_where_it_was_looked_for() {
        let said = |role, platform| {
            directory_from(role, platform, holding(&[])).map_err(|nowhere| nowhere.to_string())
        };

        assert_eq!(
            said(Role::Data, Platform::Linux),
            Err("no data directory: no absolute path in \
                 `BENSHI_DATA_DIR`, `XDG_DATA_HOME` or `HOME`"
                .to_owned())
        );
        assert_eq!(
            said(Role::Config, Platform::Linux),
            Err("no configuration directory: no absolute path in \
                 `XDG_CONFIG_HOME` or `HOME`"
                .to_owned())
        );
        assert_eq!(
            said(Role::Config, Platform::Windows),
            Err("no configuration directory: no absolute path in `APPDATA`".to_owned())
        );
    }

    #[test]
    fn a_relative_override_is_quoted_beside_the_variable_that_held_it() {
        let session = [("BENSHI_DATA_DIR", PathBuf::from("data/here"))];

        let found = directory_from(Role::Data, Platform::Linux, holding(&session));

        assert_eq!(
            found.map_err(|nowhere| nowhere.to_string()),
            Err(
                "`BENSHI_DATA_DIR` holds `data/here`, which is a relative path: \
                 the data directory is an absolute path or a path beneath `~`"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_mark_with_no_home_is_quoted_beside_the_variable_that_has_none() {
        let session = [("BENSHI_DATA_DIR", PathBuf::from("~/.benshi"))];

        let found = directory_from(Role::Data, Platform::Windows, holding(&session));

        assert_eq!(
            found.map_err(|nowhere| nowhere.to_string()),
            Err(
                "`BENSHI_DATA_DIR` holds `~/.benshi` and `USERPROFILE` holds no absolute path \
                 for `~` to stand for"
                    .to_owned()
            )
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_build_for_linux_reads_what_linux_sets() {
        assert_eq!(Platform::HERE, Platform::Linux);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_build_for_macos_reads_what_macos_sets() {
        assert_eq!(Platform::HERE, Platform::MacOs);
    }

    #[cfg(windows)]
    #[test]
    fn a_build_for_windows_reads_what_windows_sets() {
        assert_eq!(Platform::HERE, Platform::Windows);
    }

    // A process cannot set its own environment without `unsafe`, so the test
    // runs itself again in an environment it chose.
    #[cfg(unix)]
    #[test]
    fn the_directory_is_read_from_this_process_on_this_platform() {
        const NAME: &str = "paths::tests::the_directory_is_read_from_this_process_on_this_platform";
        let session = a_session_with(&[("BENSHI_DATA_DIR", not_text())]);

        if env::var_os(RUN_AGAIN).is_some() {
            for role in [Role::Data, Role::Config] {
                let expected = directory_from(role, Platform::HERE, holding(&session));

                assert!(expected.is_ok(), "{role:?}");
                assert_eq!(directory(role), expected, "{role:?}");
            }
            return;
        }

        let again = Command::new(env::current_exe().expect("the tests know what runs them"))
            .args(["--exact", NAME])
            .env_clear()
            .env(RUN_AGAIN, "yes")
            .envs(session.iter().map(|(variable, held)| (variable, held)))
            .output()
            .expect("the tests can be run again");
        let said = String::from_utf8_lossy(&again.stdout);

        assert!(again.status.success(), "{said}");
        // A name that matches no test is a run that passes.
        assert!(said.contains("1 passed"), "{said}");
    }
}
