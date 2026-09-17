//! What benshi does with a source once the platform has reported it.
//!
//! Policy is decided here, as pure logic, and applied by the daemon between the
//! adapter and the event bus. An adapter never consults it: an adapter
//! translates and emits, and a denied source is still listed so that
//! `benshi sources` can show it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::AppName;

/// Players that show video, allowed by default. Everything else is denied.
///
/// An allowlist rather than a denylist, and that direction is the decision.
/// What is *not* a video player has no end to it - every browser, every music
/// player, every podcast and audiobook app, anything that plays a notification
/// sound through MPRIS - and a denylist has to name all of them to work. Video
/// players are a small set that changes slowly.
///
/// The cost lands the right way round too. A video player missing from here is
/// denied, `benshi sources` shows it denied beside its name, and one command
/// fixes it. A music player missing from a denylist would be admitted instead,
/// and a track called `Artist - 03.flac` would be filed as episode three of
/// `Artist` with nothing anywhere to say so.
///
/// Three entries were read rather than guessed, and one of them is why this
/// holds published names rather than commands: `mpc-qt` registers `MpcQt`,
/// where `mpv` and `vlc` register the names they are called by. A lookup
/// lowercases, so case is the one thing an entry need not get right - the
/// hyphen `MpcQt` drops is not. A measured entry is written the way it is
/// published, so the list shows what was read and not what was inferred from
/// it. Measuring one costs nothing and does not need the player running:
/// `strings <binary or plugin> | grep org.mpris.MediaPlayer2`.
///
/// An entry nobody has measured is a guess that fails visibly, which is the
/// only kind of guess this list can safely hold - but a guess already believed
/// wrong is not one of those, and belongs nowhere. That is why no unmeasured
/// name here carries punctuation: the one measurement there is says
/// punctuation does not survive.
const VIDEO_PLAYERS: &[&str] = &[
    // Measured on this machine, 2026-09-17.
    "mpv",
    "vlc",
    // Written as it is published rather than lowercased on the way in, so
    // that the one entry proving the constructor lowercases is in the list
    // rather than only in a comment.
    "MpcQt",
    // Conventional names, unmeasured.
    "celluloid",
    "haruna",
    "smplayer",
    "totem",
    "dragonplayer",
    "parole",
    "qmplay2",
    "kodi",
    "jellyfinmediaplayer",
    "plexmediaplayer",
    "iina",
    "xplayer",
];

/// What benshi does with readings from an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Policy {
    /// Accept the source when what it has open parses as anime.
    ///
    /// What the default table gives the video players it names, and none of it
    /// is about the player itself: recognition reads the filename, not which
    /// program opened it. A player the table does not name is denied until
    /// `benshi policy <app> auto` says otherwise.
    Auto,

    /// Accept the source whatever it has open.
    ///
    /// Differs from [`Policy::Auto`] only once filename recognition exists:
    /// `Auto` will additionally require the filename to parse as anime, and
    /// `Allow` will not. The two are not a boolean and must not be collapsed.
    Allow,

    /// Ignore the source's readings.
    ///
    /// The source is still discovered and still listed by `benshi sources`,
    /// with its policy shown. Suppression applies to readings, never to
    /// discovery - a source nobody can see is a source nobody can diagnose.
    Deny,
}

impl Policy {
    /// Whether readings from an application under this policy reach the bus.
    ///
    /// Written as an exhaustive match rather than a test against `Deny`, so
    /// that a fourth policy has to state its own answer here instead of
    /// inheriting one.
    #[must_use]
    pub const fn admits(self) -> bool {
        match self {
            Self::Auto | Self::Allow => true,
            Self::Deny => false,
        }
    }
}

/// The policy in force for each application.
///
/// Keys are compared whole and without regard to case. The predecessor matched
/// a regular expression against bus names, compiled without `IGNORECASE`, and
/// of its four default players exactly one matched - by luck. A whole-name
/// comparison cannot half-match, which is the entire point.
#[derive(Debug, Clone)]
pub struct PolicyTable {
    entries: BTreeMap<String, Policy>,
    /// What an application this table has never heard of gets.
    unknown: Policy,
}

impl PolicyTable {
    /// A table in which a default set of video players is [`Policy::Auto`] and
    /// every other application is [`Policy::Deny`].
    ///
    /// Lowercased on the way in, because a lookup lowercases too. An entry
    /// written with a capital would otherwise be filed under a key nothing
    /// ever asks for, and would sit in the list looking like one that worked.
    #[must_use]
    pub fn allowing_video_players() -> Self {
        Self {
            entries: VIDEO_PLAYERS
                .iter()
                .map(|&player| (player.to_lowercase(), Policy::Auto))
                .collect(),
            unknown: Policy::Deny,
        }
    }

    /// The policy in force for an application, or this table's answer for one
    /// it has never heard of.
    #[must_use]
    pub fn policy_for(&self, app: &AppName) -> Policy {
        self.entries
            .get(&app.0.to_lowercase())
            .copied()
            .unwrap_or(self.unknown)
    }

    /// Set the policy for an application, replacing any default.
    pub fn set(&mut self, app: &AppName, policy: Policy) {
        self.entries.insert(app.0.to_lowercase(), policy);
    }
}

#[cfg(test)]
mod tests {
    use super::{Policy, PolicyTable, VIDEO_PLAYERS};
    use crate::AppName;

    fn app(name: &str) -> AppName {
        AppName(name.to_owned())
    }

    #[test]
    fn an_application_this_table_has_never_heard_of_is_denied() {
        // The whole inversion in one assertion. What is not a video player has
        // no end to it, so the default cannot be to admit whatever turns up.
        let table = PolicyTable::allowing_video_players();

        assert_eq!(table.policy_for(&app("some-new-thing")), Policy::Deny);
    }

    #[test]
    fn a_video_player_is_allowed_by_default() {
        let table = PolicyTable::allowing_video_players();

        assert_eq!(table.policy_for(&app("mpv")), Policy::Auto);
        assert_eq!(table.policy_for(&app("vlc")), Policy::Auto);
        assert_eq!(table.policy_for(&app("kodi")), Policy::Auto);
    }

    #[test]
    fn a_player_is_allowed_however_it_capitalises_its_own_name() {
        // Not hypothetical, and the reason the list holds published names
        // rather than commands. Read out of the shipped binary on 2026-09-17,
        // mpc-qt registers org.mpris.MediaPlayer2.MpcQt - neither the command
        // a user types nor the spelling written in the list. A lookup
        // lowercases, which is the only reason the published name and the
        // entry meet; the command spelling never does, hyphen and all.
        let table = PolicyTable::allowing_video_players();

        assert_eq!(table.policy_for(&app("MpcQt")), Policy::Auto);
        assert_eq!(table.policy_for(&app("MPV")), Policy::Auto);
    }

    #[test]
    fn everything_that_is_not_a_video_player_is_denied() {
        // Browsers and music players are denied here by never being named,
        // which is the point: this list would not have to grow to cover a
        // podcast app, an audiobook reader, or whatever publishes MPRIS next.
        let table = PolicyTable::allowing_video_players();

        for other in [
            "firefox",
            "chromium",
            "Feishin",
            "DeaDBeeF",
            "spotify",
            "rhythmbox",
        ] {
            assert_eq!(table.policy_for(&app(other)), Policy::Deny, "{other}");
        }
    }

    #[test]
    fn every_default_entry_allows_the_name_it_is_written_as() {
        // Every entry looked up exactly as the list spells it. `MpcQt` is what
        // carries this: written as the player publishes it, it is filed by a
        // constructor that lowercases and found by a lookup that lowercases,
        // and dropping either would file it under a key nothing ever asks for.
        // Measured by removing the constructor's `to_lowercase`, which fails
        // this and nothing else.
        let table = PolicyTable::allowing_video_players();

        for name in VIDEO_PLAYERS {
            assert_eq!(table.policy_for(&app(name)), Policy::Auto, "{name}");
        }
    }

    #[test]
    fn an_instance_qualified_source_is_answered_by_its_application_name() {
        // Observed on the live session bus: chromium published
        // org.mpris.MediaPlayer2.chromium.instance30062 on 2026-09-14, mpv
        // org.mpris.MediaPlayer2.mpv unqualified on 2026-09-17. Policy is
        // keyed on the application the adapter declares, never on the unique
        // identity - otherwise a second window of an allowed player would be
        // answered as an unknown one.
        let table = PolicyTable::allowing_video_players();

        assert_eq!(table.policy_for(&app("mpv")), Policy::Auto);
        assert_eq!(table.policy_for(&app("chromium")), Policy::Deny);
    }

    #[test]
    fn an_explicit_setting_outranks_the_default_in_both_directions() {
        // The escape hatch, and it has to work both ways: a browser the user
        // wants tried, and a video player they want left alone.
        let mut table = PolicyTable::allowing_video_players();

        table.set(&app("firefox"), Policy::Allow);
        table.set(&app("mpv"), Policy::Deny);

        assert_eq!(table.policy_for(&app("firefox")), Policy::Allow);
        assert_eq!(table.policy_for(&app("mpv")), Policy::Deny);
    }

    #[test]
    fn an_application_name_is_matched_whole_and_case_insensitively() {
        // The predecessor matched a regular expression against bus names, and
        // a whole-name comparison replaced it. This is the test that holds it
        // there. A loose comparison now admits a stranger rather than
        // silencing a player: the quieter failure, and the worse one.
        let table = PolicyTable::allowing_video_players();

        assert_eq!(table.policy_for(&app("MPV")), Policy::Auto);
        assert_eq!(table.policy_for(&app("mpvy")), Policy::Deny);
        assert_eq!(table.policy_for(&app("not-mpv")), Policy::Deny);
    }

    #[test]
    fn deny_suppresses_readings_and_the_others_do_not() {
        assert!(!Policy::Deny.admits());
        assert!(Policy::Auto.admits());
        assert!(Policy::Allow.admits());
    }
}
