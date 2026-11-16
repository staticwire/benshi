//! The AniList backend against the shared contract, over answers recorded
//! from the test account.
//!
//! `fixtures/anilist/contract.json` was recorded by `anilist_record.rs`,
//! which ran the checks of the contract that ask the service against
//! AniList. The replay fails a test where the backend asks anything other
//! than what was recorded.

mod contract;
mod recorded;
mod subject;

#[tokio::test(start_paused = true)]
async fn the_anilist_backend_satisfies_the_contract() {
    contract::verify(subject::subject(recorded::Replay::of("contract.json"))).await;
}
