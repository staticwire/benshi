//! The contract rejects a backend that breaks it.
//!
//! Every clause gets a backend that breaks it and nothing else, and a test
//! that asserts the suite notices. A check no backend can fail is a
//! description rather than a check, and the only way to tell the difference is
//! to break it on purpose.
//!
//! The `expected` string ties each test to the clause it proves, so renaming a
//! clause without its proof fails the test.

mod contract;
mod fakes;

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use benshi_core::list::{Entry, EntryId, Rating, Write};
use benshi_sync::memory::Memory;
use benshi_sync::{Error, ListBackend};

use contract::Failure;
use fakes::{DEADLINE, LISTED, UNLISTED, a_film, a_series};

/// The one clause a backend breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Break {
    /// Answers an entry for a work that is off the list.
    InventsEntries,
    /// Takes a write and keeps nothing of it.
    ForgetsWrites,
    /// Takes the score away from a work it writes.
    ClearsTheScore,
    /// Renames a work it writes.
    RenamesTheWork,
    /// Counts one more rewatch for a write made again.
    CountsAWriteMadeAgain,
    /// Takes a write to a work off the list and adds nothing.
    WritesOnlyTheList,
    /// Scores a work a write adds.
    ScoresAWorkItAdds,
    /// Lists what it was given, whatever was written since.
    KeepsTheListItWasGiven,
    /// Lists every work twice.
    ListsTwice,
    /// Lists a work that is off the list.
    ListsAWorkOffIt,
    /// Fails a call to a service that answers.
    FailsAnAnsweringService,
    /// Takes a write to a number the service has no work under.
    TakesAWriteToNoWork,
    /// Waits on a silent service for ever.
    NeverGivesUpOnSilence,
    /// Gives up on a silent service half a deadline late.
    GivesUpOnSilenceLate,
    /// Answers a silent service as one that cannot be reached.
    TakesSilenceForUnreachable,
    /// Answers a silent service with a deadline other than its own.
    MisstatesTheDeadline,
    /// Answers a service that cannot be reached as a silent one.
    TakesUnreachableForSilence,
    /// Answers a wait with twice the wait the service asked for.
    DoublesTheWait,
    /// Answers a refusal as a service that cannot be reached.
    TakesARefusalForUnreachable,
    /// Answers what cannot be read as a refusal.
    TakesTheUnreadableForARefusal,
}

/// The backend in memory with one clause broken.
struct Broken {
    memory: Memory,
    failure: Option<Failure>,
    breaking: Break,
    /// The last write to each work, and how many times it was made again.
    written: Mutex<BTreeMap<EntryId, (Write, u32)>>,
}

impl Broken {
    fn new(memory: Memory, failure: Option<Failure>, breaking: Break) -> Self {
        Self {
            memory,
            failure,
            breaking,
            written: Mutex::new(BTreeMap::new()),
        }
    }

    /// The answer a break gives in place of the failure the backend stands
    /// in for, where it gives one.
    async fn misanswered(&self) -> Option<Error> {
        match (self.breaking, self.failure?) {
            (Break::NeverGivesUpOnSilence, Failure::Silent) => std::future::pending().await,
            (Break::GivesUpOnSilenceLate, Failure::Silent) => {
                tokio::time::sleep(DEADLINE * 3 / 2).await;
                Some(Error::Timeout { deadline: DEADLINE })
            }
            (Break::TakesSilenceForUnreachable, Failure::Silent)
            | (Break::TakesARefusalForUnreachable, Failure::Refused) => {
                Some(Error::Unreachable("connection refused".into()))
            }
            (Break::MisstatesTheDeadline, Failure::Silent) => Some(Error::Timeout {
                deadline: DEADLINE * 2,
            }),
            (Break::TakesUnreachableForSilence, Failure::Unreachable) => {
                Some(Error::Timeout { deadline: DEADLINE })
            }
            (Break::DoublesTheWait, Failure::Wait(after)) => Some(Error::Wait { after: after * 2 }),
            (Break::TakesTheUnreadableForARefusal, Failure::Unreadable) => {
                Some(Error::Refused("invalid token".to_owned()))
            }
            _ => None,
        }
    }

    /// An entry as this backend answers it.
    fn broken(&self, mut entry: Entry) -> Entry {
        let written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(&(_, again)) = written.get(&entry.id) {
            match self.breaking {
                Break::ClearsTheScore => entry.score = None,
                Break::RenamesTheWork => "Another Title".clone_into(&mut entry.title),
                Break::CountsAWriteMadeAgain => entry.rewatches += again,
                Break::ScoresAWorkItAdds if entry.id == UNLISTED => entry.score = Rating::new(50),
                _ => {}
            }
        }
        entry
    }
}

impl ListBackend for Broken {
    fn name(&self) -> &str {
        self.memory.name()
    }

    async fn list(&self) -> Result<Vec<Entry>, Error> {
        if let Some(error) = self.misanswered().await {
            return Err(error);
        }
        let list = match self.breaking {
            Break::KeepsTheListItWasGiven => vec![a_series(LISTED)],
            _ => self.memory.list().await?,
        };
        let list = match self.breaking {
            Break::ListsTwice => [list.clone(), list].concat(),
            Break::ListsAWorkOffIt if list.iter().all(|entry| entry.id != UNLISTED) => {
                [list, vec![a_film(UNLISTED)]].concat()
            }
            _ => list,
        };

        Ok(list.into_iter().map(|entry| self.broken(entry)).collect())
    }

    async fn entry(&self, id: EntryId) -> Result<Option<Entry>, Error> {
        if let Some(error) = self.misanswered().await {
            return Err(error);
        }
        if self.breaking == Break::FailsAnAnsweringService && self.failure.is_none() {
            return Err(Error::Unreachable("connection refused".into()));
        }
        let entry = match (self.breaking, self.memory.entry(id).await?) {
            (Break::InventsEntries, None) => Some(a_film(id)),
            (_, entry) => entry,
        };

        Ok(entry.map(|entry| self.broken(entry)))
    }

    async fn write(&self, write: &Write) -> Result<(), Error> {
        if let Some(error) = self.misanswered().await {
            return Err(error);
        }
        match self.breaking {
            Break::ForgetsWrites => return Ok(()),
            Break::WritesOnlyTheList if self.memory.entry(write.id).await?.is_none() => {
                return Ok(());
            }
            _ => {}
        }
        match self.memory.write(write).await {
            Err(Error::NoWork { .. }) if self.breaking == Break::TakesAWriteToNoWork => {}
            answer => answer?,
        }
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        let again = match written.get(&write.id) {
            Some(&(last, again)) if last == *write => again + 1,
            _ => 0,
        };
        written.insert(write.id, (*write, again));

        Ok(())
    }
}

/// Run the suite against the backend in memory with one clause broken.
async fn verify_broken(breaking: Break) {
    contract::verify(fakes::subject(move |memory, failure| {
        Broken::new(memory, failure, breaking)
    }))
    .await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a work off the list is answered as none")]
async fn an_entry_invented_for_a_work_off_the_list_fails_the_contract() {
    verify_broken(Break::InventsEntries).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write is read back as it was written")]
async fn a_write_forgotten_fails_the_contract() {
    verify_broken(Break::ForgetsWrites).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write leaves the score as it was")]
async fn a_score_taken_away_by_a_write_fails_the_contract() {
    verify_broken(Break::ClearsTheScore).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write changes nothing else of the entry")]
async fn a_work_renamed_by_a_write_fails_the_contract() {
    verify_broken(Break::RenamesTheWork).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write made twice leaves what one left")]
async fn a_write_made_again_counted_again_fails_the_contract() {
    verify_broken(Break::CountsAWriteMadeAgain).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write adds a work that is off the list")]
async fn a_write_that_adds_nothing_fails_the_contract() {
    verify_broken(Break::WritesOnlyTheList).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a work a write adds has no score")]
async fn a_work_added_with_a_score_fails_the_contract() {
    verify_broken(Break::ScoresAWorkItAdds).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "the list holds what was written")]
async fn a_list_that_keeps_what_it_was_given_fails_the_contract() {
    verify_broken(Break::KeepsTheListItWasGiven).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a work off the list is not listed")]
async fn a_list_that_holds_a_work_off_it_fails_the_contract() {
    verify_broken(Break::ListsAWorkOffIt).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a call to a service that answers succeeds")]
async fn a_call_failed_while_the_service_answers_fails_the_contract() {
    verify_broken(Break::FailsAnAnsweringService).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a work is on the list once")]
async fn a_list_that_holds_a_work_twice_fails_the_contract() {
    verify_broken(Break::ListsTwice).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "a write to a work the service does not have is answered as no work")]
async fn a_write_to_no_work_taken_fails_the_contract() {
    verify_broken(Break::TakesAWriteToNoWork).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "every call ends within its deadline")]
async fn a_call_that_waits_on_silence_for_ever_fails_the_contract() {
    verify_broken(Break::NeverGivesUpOnSilence).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "every call ends within its deadline")]
async fn a_call_that_gives_up_late_fails_the_contract() {
    verify_broken(Break::GivesUpOnSilenceLate).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Silent")]
async fn silence_taken_for_a_service_out_of_reach_fails_the_contract() {
    verify_broken(Break::TakesSilenceForUnreachable).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Silent")]
async fn a_deadline_misstated_fails_the_contract() {
    verify_broken(Break::MisstatesTheDeadline).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Unreachable")]
async fn a_service_out_of_reach_taken_for_silence_fails_the_contract() {
    verify_broken(Break::TakesUnreachableForSilence).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Wait")]
async fn a_wait_doubled_fails_the_contract() {
    verify_broken(Break::DoublesTheWait).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Refused")]
async fn a_refusal_taken_for_a_service_out_of_reach_fails_the_contract() {
    verify_broken(Break::TakesARefusalForUnreachable).await;
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "each failure is answered as its class, and Unreadable")]
async fn an_unreadable_answer_taken_for_a_refusal_fails_the_contract() {
    verify_broken(Break::TakesTheUnreadableForARefusal).await;
}
