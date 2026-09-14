//! What benshi does with a source once the platform has reported it.
//!
//! Policy is decided here, as pure logic, and applied by the daemon between the
//! adapter and the event bus. An adapter never consults it: an adapter
//! translates and emits, and a denied source is still listed so that
//! `benshi sources` can show it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::AppName;

/// Browsers, denied by default.
///
/// Not out of prejudice: a browser publishes a page title rather than a file,
/// and recognising a page title is a structurally different problem from
/// recognising a filename. Browser streaming is a legitimate later feature and
/// pretending it works now would be worse than omitting it.
///
/// This is a default, not a prohibition. A user who wants to experiment sets
/// one to [`Policy::Allow`] and gets title-only recognition with capabilities
/// reported honestly.
const DEFAULT_DENYLIST: &[&str] = &[
    "firefox",
    "librewolf",
    "waterfox",
    "zen",
    "chromium",
    "chromium-browser",
    "chrome",
    "google-chrome",
    "brave",
    "brave-browser",
    "vivaldi",
    "opera",
    "epiphany",
    "falkon",
    "midori",
];

/// What benshi does with readings from an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Policy {
    /// Accept the source when what it has open parses as anime.
    ///
    /// The default, and it is why adding support for a new local player is
    /// usually nothing: which player opened the file is not interesting.
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
#[derive(Debug, Clone, Default)]
pub struct PolicyTable {
    entries: BTreeMap<String, Policy>,
}

impl PolicyTable {
    /// A table in which the browsers of the default denylist are denied and
    /// every other application is [`Policy::Auto`].
    #[must_use]
    pub fn with_default_denylist() -> Self {
        Self {
            entries: DEFAULT_DENYLIST
                .iter()
                .map(|&browser| (browser.to_owned(), Policy::Deny))
                .collect(),
        }
    }

    /// The policy in force for an application, [`Policy::Auto`] if none is set.
    #[must_use]
    pub fn policy_for(&self, app: &AppName) -> Policy {
        self.entries
            .get(&app.0.to_lowercase())
            .copied()
            .unwrap_or(Policy::Auto)
    }

    /// Set the policy for an application, replacing any default.
    pub fn set(&mut self, app: &AppName, policy: Policy) {
        self.entries.insert(app.0.to_lowercase(), policy);
    }
}

#[cfg(test)]
mod tests {
    use super::{Policy, PolicyTable};
    use crate::AppName;

    fn app(name: &str) -> AppName {
        AppName(name.to_owned())
    }

    #[test]
    fn an_unknown_application_is_auto() {
        let table = PolicyTable::with_default_denylist();
        assert_eq!(table.policy_for(&app("mpv")), Policy::Auto);
    }

    #[test]
    fn a_browser_is_denied_by_default() {
        let table = PolicyTable::with_default_denylist();
        assert_eq!(table.policy_for(&app("firefox")), Policy::Deny);
        assert_eq!(table.policy_for(&app("chromium")), Policy::Deny);
    }

    #[test]
    fn an_instance_qualified_source_is_denied_by_its_application_name() {
        // Observed on the live session bus on 2026-09-14: chromium publishes
        // org.mpris.MediaPlayer2.chromium.instance30062 while Feishin publishes
        // org.mpris.MediaPlayer2.Feishin. Policy is keyed on the application
        // the adapter declares, never on the unique identity - otherwise the
        // denylist silently misses every browser that qualifies its bus name.
        let table = PolicyTable::with_default_denylist();
        assert_eq!(table.policy_for(&app("chromium")), Policy::Deny);
        assert_eq!(table.policy_for(&app("Feishin")), Policy::Auto);
    }

    #[test]
    fn an_explicit_setting_outranks_the_default_denylist() {
        let mut table = PolicyTable::with_default_denylist();
        table.set(&app("firefox"), Policy::Allow);
        assert_eq!(table.policy_for(&app("firefox")), Policy::Allow);
    }

    #[test]
    fn an_application_name_is_matched_whole_and_case_insensitively() {
        // The predecessor matched a regular expression against bus names, and
        // a whole-name comparison replaced it. This is the test that holds it
        // there.
        let table = PolicyTable::with_default_denylist();
        assert_eq!(table.policy_for(&app("Firefox")), Policy::Deny);
        assert_eq!(table.policy_for(&app("firefoxy")), Policy::Auto);
        assert_eq!(table.policy_for(&app("not-firefox")), Policy::Auto);
    }

    #[test]
    fn deny_suppresses_readings_and_the_others_do_not() {
        assert!(!Policy::Deny.admits());
        assert!(Policy::Auto.admits());
        assert!(Policy::Allow.admits());
    }
}
