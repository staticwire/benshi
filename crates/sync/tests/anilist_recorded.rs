//! The AniList backend over answers recorded from the test account.
//!
//! `fixtures/anilist/list.json` was recorded by `anilist_record.rs`, with
//! three works filed on the account's list. The replay fails a test where
//! the backend asks anything other than what was recorded.

mod recorded;

use std::time::Duration;

use benshi_core::list::{Entry, EntryId, Format, Rating, Status};
use benshi_sync::anilist::AniList;

/// An entry with the facts the service gives a work, and no spellings,
/// which a test of their own checks.
// One argument for each fact of an entry, which is what the table below is.
#[allow(clippy::too_many_arguments)]
fn an_entry(
    id: i64,
    title: &str,
    format: Format,
    episodes: u32,
    minutes: u64,
    year: u16,
    status: Status,
    progress: u32,
    score: Option<Rating>,
) -> Entry {
    Entry {
        id: EntryId(id),
        title: title.to_owned(),
        spellings: Vec::new(),
        format: Some(format),
        episodes: Some(episodes),
        episode_length: Some(Duration::from_mins(minutes)),
        year: Some(year),
        released: true,
        status,
        progress,
        rewatches: 0,
        score,
    }
}

#[tokio::test(start_paused = true)]
async fn the_list_of_the_test_account_is_read_from_its_recorded_answers() {
    let anilist = AniList::new(recorded::Replay::of("list.json"));

    let list = anilist.list().await.expect("the list is read");

    let without_spellings: Vec<Entry> = list
        .iter()
        .map(|entry| Entry {
            spellings: Vec::new(),
            ..entry.clone()
        })
        .collect();
    assert_eq!(
        without_spellings,
        [
            an_entry(
                16_498,
                "Shingeki no Kyojin",
                Format::Tv,
                25,
                24,
                2013,
                Status::OnHold,
                3,
                None
            ),
            an_entry(
                227,
                "FLCL",
                Format::Ova,
                6,
                25,
                2000,
                Status::Watching,
                1,
                None
            ),
            an_entry(
                112_151,
                "Kimetsu no Yaiba: Mugen Ressha-hen",
                Format::Film,
                1,
                117,
                2020,
                Status::Completed,
                1,
                Rating::new(80),
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_spellings_recorded_are_every_other_title_and_synonym_once() {
    let anilist = AniList::new(recorded::Replay::of("list.json"));

    let list = anilist.list().await.expect("the list is read");

    let spellings: Vec<(EntryId, usize, bool)> = list
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.spellings.len(),
                entry.spellings.contains(&entry.title),
            )
        })
        .collect();
    // The series has an English and a native title and 15 synonyms. The
    // OVA's romaji and English titles are both its title, so its native
    // title and its 2 synonyms are left. The film has an English and a
    // native title and 12.
    assert_eq!(
        spellings,
        [
            (EntryId(16_498), 17, false),
            (EntryId(227), 3, false),
            (EntryId(112_151), 14, false),
        ]
    );
    assert!(list[0].spellings.contains(&"Attack on Titan".to_owned()));
}
