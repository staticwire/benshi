//! The MPRIS adapter against the shared contract.
//!
//! The suite is written without naming a platform so that it can be run against
//! one. This is that run: the same fifteen clauses, the same code, a real
//! session bus instead of a fake.

// The adapter under test exists only on Linux, so on any other target this
// binary has nothing to compile rather than nothing to run.
#![cfg(target_os = "linux")]

mod contract;

use std::collections::BTreeSet;

use benshi_core::PlayerId;
use benshi_core::clock::SystemClock;
use benshi_detect::PlayerWatcher;
use benshi_detect::mpris::{MprisWatcher, SOURCE_DEADLINE};

/// Every clause of the contract, against whatever is playing on this machine.
///
/// Ignored by default, and this is the only ignored test the project may carry.
/// It needs a session bus with at least one MPRIS player on it, which no
/// continuous integration runner has; a test that cannot pass there would be a
/// permanently red build rather than a check. Run it by hand on a desktop
/// session, with something open:
///
/// ```sh
/// cargo test -p benshi-detect --test mpris_contract -- --ignored --nocapture
/// ```
///
/// A failure here is a bug in the adapter. Do not relax a clause to
/// accommodate it without writing down why the clause was wrong.
#[tokio::test]
#[ignore = "requires a session bus with a live MPRIS player"]
async fn the_mpris_adapter_satisfies_the_contract() {
    let mut watcher = MprisWatcher::connect(SystemClock::new(), SOURCE_DEADLINE)
        .await
        .expect("a session bus");

    // Most clauses only bite on a source, and several only on a reading, so an
    // empty bus would satisfy the whole suite while proving nothing. A run with
    // nothing to check fails here rather than reporting success.
    let sources = watcher.sources().await.expect("a listing");
    assert!(
        !sources.is_empty(),
        "no MPRIS player is on this bus, so this run would prove nothing. \
         Open a player and try again."
    );

    // The failures are named rather than dropped. A source that accepted the
    // call and never answered produces no snapshot either, and reporting that
    // as "nothing is open" would misdiagnose the one case a deadline exists for.
    let outcome = watcher.poll().await.expect("a reading");
    assert!(
        !outcome.snapshots.is_empty(),
        "{} source(s) listed, none produced a reading, {} failed: {:?}. \
         With no failures, open something in a player and try again. With \
         failures, they are the bug and this run can check nothing.",
        sources.len(),
        outcome.failures.len(),
        outcome.failures
    );

    // A source with nothing open produces no reading and must still be
    // described, or a player that merely stopped would look to the daemon like
    // a player that closed. The contract checks that every reading was
    // described; the reverse it cannot check, because a player that closed
    // between the listing and the round would fail a correct implementation.
    // That risk is taken here instead, where a re-run settles it.
    //
    // Exercising this needs a player sitting idle: with every player busy,
    // every listed source produces a reading and this passes without proving
    // anything.
    let described: BTreeSet<&PlayerId> = outcome
        .sources
        .iter()
        .map(|source| &source.player)
        .collect();
    let failed: BTreeSet<&PlayerId> = outcome
        .failures
        .iter()
        .map(|(player, _reason)| player)
        .collect();

    for source in &sources {
        assert!(
            described.contains(&source.player) || failed.contains(&source.player),
            "{:?} was listed, and the round that followed neither described nor \
             failed it. A player closing between the two calls does this too, \
             so re-run before believing it.",
            source.player
        );
    }

    contract::verify(contract::Subject {
        watcher,
        deadline: SOURCE_DEADLINE,
    })
    .await;
}
