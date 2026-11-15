//! Which file a path names, by its device and inode.
//!
//! The kernel names a file a process holds open by the path it reached, with
//! every symbolic link on the way resolved, and a player names the path it was
//! given. One file opened through a link, or through a directory that is one,
//! has two paths and one device and inode.
//!
//! Paths are looked up on a thread of their own, under the deadline of the
//! reading that asked, and each answer is kept while every round of readings
//! asks for its path. Looks that outlive the deadline are left to finish and
//! keep their answer for a later reading, and no second look at a path starts
//! while one runs, so a path on a mount that has stopped answering holds one
//! thread and no more.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::panic;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use benshi_core::path::RawPath;
use futures_util::future::join;
use tokio::time::{Instant, timeout_at};

/// The device and inode of a file.
type Inode = (u64, u64);

/// What is known of the file one path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// The look has started and has not finished.
    Running,
    /// The look has finished, with the file's device and inode or none.
    Done(Option<Inode>),
}

/// Every look, and the paths asked for since the round before ended.
#[derive(Debug, Default)]
struct Looks {
    known: HashMap<RawPath, Look>,
    asked: HashSet<RawPath>,
}

/// The files paths name, each looked up once while readings ask for it.
#[derive(Debug, Default)]
pub(crate) struct Inodes {
    looks: Arc<Mutex<Looks>>,
}

impl Inodes {
    /// Whether `one` and `other` name one file: nothing while a look at either
    /// path runs or past `until`, and false where either names no file.
    pub(crate) async fn same(
        &self,
        one: &RawPath,
        other: &RawPath,
        until: Instant,
    ) -> Option<bool> {
        let (one, other) = join(
            self.of_with(one, until, inode_of),
            self.of_with(other, until, inode_of),
        )
        .await;

        match (one?, other?) {
            (Some(one), Some(other)) => Some(one == other),
            _ => Some(false),
        }
    }

    /// The device and inode `path` names, looked up by `look` where no look
    /// at it has started: nothing while a look runs or past `until`.
    async fn of_with<F>(&self, path: &RawPath, until: Instant, look: F) -> Option<Option<Inode>>
    where
        F: FnOnce(&Path) -> Option<Inode> + Send + 'static,
    {
        {
            let mut looks = self.looks();
            looks.asked.insert(path.clone());
            match looks.known.entry(path.clone()) {
                Entry::Occupied(known) => {
                    return match known.get() {
                        Look::Done(inode) => Some(*inode),
                        Look::Running => None,
                    };
                }
                Entry::Vacant(unknown) => {
                    unknown.insert(Look::Running);
                }
            }
        }

        let looks = Arc::clone(&self.looks);
        let path = path.clone();
        let task = tokio::task::spawn_blocking(move || {
            let inode = look(Path::new(OsStr::from_bytes(path.as_bytes())));
            lock(&looks).known.insert(path, Look::Done(inode));
            inode
        });

        match timeout_at(until, task).await {
            Ok(Ok(inode)) => Some(inode),
            Ok(Err(failed)) if failed.is_panic() => panic::resume_unwind(failed.into_panic()),
            // The runtime is shutting down, or the deadline has passed. Looks
            // left behind keep their answer when they end.
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// Forget every path no reading asked for since the round before ended,
    /// except one whose look still runs.
    pub(crate) fn end_round(&self) {
        let mut looks = self.looks();
        let Looks { known, asked } = &mut *looks;
        known.retain(|path, look| *look == Look::Running || asked.contains(path));
        asked.clear();
    }

    fn looks(&self) -> MutexGuard<'_, Looks> {
        lock(&self.looks)
    }
}

/// Lock the looks. Each change to them is a single insert, retain or clear,
/// so a thread that panicked holding the lock leaves them as they were or as
/// it meant them to be, and they are taken as they stand.
fn lock(looks: &Mutex<Looks>) -> MutexGuard<'_, Looks> {
    looks.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The device and inode of the file `path` names, through every link on the way.
///
/// Nothing here panics, and `Inodes` relies on that: looks that panic record no
/// answer, and no later look at their path starts.
fn inode_of(path: &Path) -> Option<Inode> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    use benshi_core::path::RawPath;
    use tokio::time::Instant;

    use super::{Inode, Inodes};

    fn raw(path: &Path) -> RawPath {
        RawPath::from_bytes(path.as_os_str().as_bytes().to_vec())
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// Write nothing to a file at `name` in `directory`, and name it.
    fn a_file(directory: &Path, name: &str) -> RawPath {
        let path = directory.join(name);
        fs::write(&path, b"").expect("the file written");
        raw(&path)
    }

    #[tokio::test]
    async fn a_link_names_the_file_it_points_at() {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let file = a_file(directory.path(), "Show Title - 02.mkv");
        let link = directory.path().join("link.mkv");
        symlink(directory.path().join("Show Title - 02.mkv"), &link).expect("the link made");

        let same = Inodes::default().same(&raw(&link), &file, soon()).await;

        assert_eq!(same, Some(true));
    }

    #[tokio::test]
    async fn a_file_reached_through_a_linked_directory_is_one_file() {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let real = directory.path().join("real");
        fs::create_dir(&real).expect("the directory made");
        let file = a_file(&real, "Show Title - 02.mkv");
        let linked = directory.path().join("linked");
        symlink(&real, &linked).expect("the link made");

        let same = Inodes::default()
            .same(&raw(&linked.join("Show Title - 02.mkv")), &file, soon())
            .await;

        assert_eq!(same, Some(true));
    }

    #[tokio::test]
    async fn two_files_are_two() {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let first = a_file(directory.path(), "Show Title - 01.mkv");
        let second = a_file(directory.path(), "Show Title - 02.mkv");

        assert_eq!(
            Inodes::default().same(&first, &second, soon()).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn a_path_that_names_no_file_is_not_the_file_held() {
        let directory = tempfile::tempdir().expect("a directory of our own");
        let held = a_file(directory.path(), "Show Title - 02.mkv");
        let gone = raw(&directory.path().join("Show Title - 01.mkv"));

        assert_eq!(
            Inodes::default().same(&gone, &held, soon()).await,
            Some(false)
        );
    }

    /// Look at a path a second time, which must never happen.
    fn no_second_look(_: &Path) -> Option<Inode> {
        unreachable!("a second look at one path")
    }

    #[tokio::test]
    async fn two_paths_have_no_answer_while_a_look_at_one_of_them_runs() {
        let inodes = Inodes::default();
        let directory = tempfile::tempdir().expect("a directory of our own");
        let file = a_file(directory.path(), "Show Title - 02.mkv");
        let stalled = RawPath::from_bytes(b"/media/video/stalled.mkv".to_vec());
        let (release, held) = mpsc::channel::<()>();
        let asked = inodes.of_with(
            &stalled,
            Instant::now() + Duration::from_millis(20),
            move |_| {
                held.recv().expect("the test lets the look go");
                None
            },
        );
        tokio::time::timeout(Duration::from_secs(5), asked)
            .await
            .expect("the look is given up on at its deadline");

        let same = inodes.same(&stalled, &file, soon()).await;
        release.send(()).expect("the look still waits");

        assert_eq!(same, None);
    }

    #[tokio::test]
    async fn a_look_past_the_deadline_is_no_answer_and_keeps_its_answer() {
        let inodes = Inodes::default();
        let path = RawPath::from_bytes(b"/media/video/stalled.mkv".to_vec());
        let (release, held) = mpsc::channel::<()>();
        let asked = inodes.of_with(
            &path,
            Instant::now() + Duration::from_millis(20),
            move |_| {
                held.recv().expect("the test lets the look go");
                Some((1, 2))
            },
        );
        let in_time = tokio::time::timeout(Duration::from_secs(5), asked)
            .await
            .expect("the look is given up on at its deadline");
        assert_eq!(in_time, None);
        release.send(()).expect("the look still waits");

        let kept = async {
            loop {
                if let Some(inode) = inodes.of_with(&path, soon(), no_second_look).await {
                    return inode;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), kept)
                .await
                .ok(),
            Some(Some((1, 2)))
        );
    }

    #[tokio::test]
    async fn a_path_a_round_asks_for_is_looked_up_once() {
        let inodes = Inodes::default();
        let path = RawPath::from_bytes(b"/media/video/episode.mkv".to_vec());
        assert_eq!(
            inodes.of_with(&path, soon(), |_| Some((1, 2))).await,
            Some(Some((1, 2)))
        );

        inodes.end_round();

        assert_eq!(
            inodes.of_with(&path, soon(), no_second_look).await,
            Some(Some((1, 2)))
        );
    }

    #[tokio::test]
    async fn a_path_no_reading_asked_for_in_a_round_is_looked_up_again() {
        let inodes = Inodes::default();
        let path = RawPath::from_bytes(b"/media/video/episode.mkv".to_vec());
        assert_eq!(
            inodes.of_with(&path, soon(), |_| Some((1, 2))).await,
            Some(Some((1, 2)))
        );

        inodes.end_round();
        inodes.end_round();

        assert_eq!(
            inodes.of_with(&path, soon(), |_| Some((3, 4))).await,
            Some(Some((3, 4)))
        );
    }
}
