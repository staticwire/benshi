//! A list kept in memory.
//!
//! The backend the contract was written against. Made to fail, it stands in
//! for a service that fails in any of the ways [`Fault`] names, without a
//! network. It keeps a list and the works the service has beside it, and
//! takes a write as a service does.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use benshi_core::list::{Entry, EntryId, Write};

use crate::{Error, ListBackend, within};

/// How a backend in memory fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Accepts every call and never answers.
    Silent,
    /// Cannot be reached.
    Unreachable,
    /// Asks to be asked again after this long.
    Wait(Duration),
    /// Refuses every call.
    Refused,
    /// Answers with what cannot be read.
    Unreadable,
}

/// A list kept in memory.
#[derive(Debug)]
pub struct Memory {
    name: String,
    deadline: Duration,
    fault: Option<Fault>,
    works: Mutex<Works>,
}

/// What the service has: the works on the list, and the others.
#[derive(Debug)]
struct Works {
    listed: BTreeMap<EntryId, Entry>,
    unlisted: BTreeMap<EntryId, Entry>,
}

impl Memory {
    /// A backend named `name` whose list holds `listed`, and whose service
    /// also has `unlisted`.
    ///
    /// Where the account stands on a work of `unlisted` is not read: a write
    /// that adds the work says it, and a work added has no score.
    #[must_use]
    pub fn new(
        name: &str,
        deadline: Duration,
        listed: impl IntoIterator<Item = Entry>,
        unlisted: impl IntoIterator<Item = Entry>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            deadline,
            fault: None,
            works: Mutex::new(Works {
                listed: by_work(listed),
                unlisted: by_work(unlisted),
            }),
        }
    }

    /// The same backend, failing every call this way.
    #[must_use]
    pub fn failing(self, fault: Fault) -> Self {
        Self {
            fault: Some(fault),
            ..self
        }
    }

    /// Answer a call as the service would, within the deadline.
    async fn answer<T>(
        &self,
        answer: impl FnOnce(&mut Works) -> Result<T, Error>,
    ) -> Result<T, Error> {
        within(self.deadline, async {
            match self.fault {
                None => {
                    let mut works = self.works.lock().unwrap_or_else(PoisonError::into_inner);
                    answer(&mut works)
                }
                Some(Fault::Silent) => std::future::pending().await,
                Some(Fault::Unreachable) => Err(Error::Unreachable("connection refused".into())),
                Some(Fault::Wait(after)) => Err(Error::Wait { after }),
                Some(Fault::Refused) => Err(Error::Refused("invalid token".to_owned())),
                Some(Fault::Unreadable) => Err(Error::Unreadable(
                    "expected value at line 1 column 1".into(),
                )),
            }
        })
        .await
    }
}

/// The entries, by the work each is for.
fn by_work(entries: impl IntoIterator<Item = Entry>) -> BTreeMap<EntryId, Entry> {
    entries.into_iter().map(|entry| (entry.id, entry)).collect()
}

impl ListBackend for Memory {
    fn name(&self) -> &str {
        &self.name
    }

    async fn list(&self) -> Result<Vec<Entry>, Error> {
        self.answer(|works| Ok(works.listed.values().cloned().collect()))
            .await
    }

    async fn entry(&self, id: EntryId) -> Result<Option<Entry>, Error> {
        self.answer(|works| Ok(works.listed.get(&id).cloned()))
            .await
    }

    async fn write(&self, write: &Write) -> Result<(), Error> {
        self.answer(|works| {
            let entry = match works.unlisted.remove(&write.id) {
                Some(work) => works.listed.entry(write.id).or_insert(Entry {
                    score: None,
                    ..work
                }),
                None => works
                    .listed
                    .get_mut(&write.id)
                    .ok_or(Error::NoWork { id: write.id })?,
            };
            entry.status = write.status;
            entry.progress = write.progress;
            entry.rewatches = write.rewatches;

            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Memory;
    use crate::ListBackend;

    #[test]
    fn a_backend_in_memory_is_named_as_it_was_made() {
        let memory = Memory::new("second", Duration::from_secs(15), [], []);

        assert_eq!(memory.name(), "second");
    }
}
