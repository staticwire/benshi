//! The file a process has open, as its descriptors in `/proc` name it.
//!
//! A player that plays a file without naming its path still holds that file
//! open. Totem 43.2 publishes a length, a state and a position over MPRIS, and
//! no `xesam:url` and no `xesam:title`. A player that publishes only a title
//! names no file either. The kernel lists every descriptor of a process under
//! `/proc/<pid>/fd` as a link. The target of the link is the path of the open
//! file, or a type and an inode number for a pipe or a socket, such as
//! `pipe:[48214]`.
//!
//! The links are read, and the files they point at never are. `readlink` on
//! such a link is answered from the kernel's own record of the open file. A
//! `stat` follows the link to the file, and on a network or FUSE mount that
//! has stopped answering it waits until that mount answers.
//!
//! Only a video file counts, and only where there is one. A player holds
//! other files as well, its own metadata and configuration among them, and of
//! two video files nothing here can tell which one plays.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::panic;
use std::path::{Path, PathBuf};

use benshi_core::path::RawPath;
use tokio::time::{Instant, timeout_at};

/// What the kernel puts after the path of a file deleted while it is open.
///
/// Nothing tells it apart from a name that ends the same way, so a link that
/// ends in it is never taken as the file, trimmed or as it stands.
const DELETED: &[u8] = b" (deleted)";

/// The extensions of the containers video files come in, lowercase.
const VIDEO_EXTENSIONS: &[&[u8]] = &[
    b"3gp", b"avi", b"flv", b"m2ts", b"m4v", b"mkv", b"mov", b"mp4", b"mpeg", b"mpg", b"mts",
    b"ogm", b"ogv", b"ts", b"webm", b"wmv",
];

/// The one video file a process holds open, looked for until `until`.
///
/// The descriptors are read on a thread of their own, so that the deadline
/// holds over that read. None where the look runs out of time.
pub(crate) async fn video_held_by(process: u32, until: Instant) -> Option<RawPath> {
    let descriptors = PathBuf::from(format!("/proc/{process}/fd"));
    let look = tokio::task::spawn_blocking(move || the_video_open_in(&descriptors));

    match timeout_at(until, look).await {
        Ok(Ok(held)) => held,
        Ok(Err(failed)) if failed.is_panic() => panic::resume_unwind(failed.into_panic()),
        // The runtime is shutting down, or the deadline has passed. A read
        // left behind ends on its own, since it reads nothing but `/proc`.
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The one video file a process has open, read from the directory of its
/// descriptors, `/proc/<pid>/fd`.
///
/// None where that directory cannot be read, as for a process that has ended
/// or that this user may not inspect.
fn the_video_open_in(descriptors: &Path) -> Option<RawPath> {
    let links = fs::read_dir(descriptors).ok()?;

    the_one_video(
        links
            // A descriptor closed while the directory is read names nothing
            // any more, so a link that cannot be read is passed over.
            .flatten()
            .filter_map(|link| fs::read_link(link.path()).ok())
            .map(|target| target.into_os_string().into_vec()),
    )
}

/// The video file among the targets of a process's descriptors, where they
/// name exactly one.
///
/// A file open on two descriptors is one file. A file deleted while open
/// still counts, so that it and another video file name neither.
fn the_one_video(targets: impl IntoIterator<Item = Vec<u8>>) -> Option<RawPath> {
    let videos: BTreeSet<Vec<u8>> = targets
        .into_iter()
        .filter(|target| is_video(target.strip_suffix(DELETED).unwrap_or(target)))
        .collect();

    let mut videos = videos.into_iter();
    match (videos.next(), videos.next()) {
        (Some(only), None) if !only.ends_with(DELETED) => Some(RawPath::from_bytes(only)),
        _ => None,
    }
}

/// Whether a path ends in the extension of a video file, in any case.
///
/// The extension is what follows the last dot, and one that holds a slash
/// belongs to a directory and matches nothing here.
fn is_video(path: &[u8]) -> bool {
    let mut parts = path.rsplitn(2, |&byte| byte == b'.');
    let (Some(extension), Some(_)) = (parts.next(), parts.next()) else {
        return false;
    };

    VIDEO_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs::{self, File};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::time::Duration;

    use benshi_core::path::RawPath;
    use tempfile::TempDir;
    use tokio::time::Instant;

    use super::{the_one_video, the_video_open_in, video_held_by};

    /// The file the player in these tests plays.
    const PLAYED: &str = "/media/video/[Group] Show Title - 03 [1080p].mkv";

    /// What a player's descriptors hold beside the file it plays.
    const BESIDE: [&str; 5] = [
        "socket:[48213]",
        "pipe:[48214]",
        "anon_inode:[eventfd]",
        "/dev/dri/renderD128",
        "/srv/user/.local/share/gvfs-metadata/home",
    ];

    fn targets(paths: &[&str]) -> Vec<Vec<u8>> {
        paths.iter().map(|path| path.as_bytes().to_vec()).collect()
    }

    fn a_path(text: &str) -> RawPath {
        RawPath::from_bytes(text.as_bytes().to_vec())
    }

    /// A directory of links, as `/proc/<pid>/fd` is: each named by its
    /// descriptor and pointing at what that descriptor names.
    fn descriptors(targets: &[&[u8]]) -> TempDir {
        let directory = tempfile::tempdir().expect("a directory of our own");
        for (descriptor, target) in targets.iter().enumerate() {
            symlink(
                OsStr::from_bytes(target),
                directory.path().join(descriptor.to_string()),
            )
            .expect("a link");
        }
        directory
    }

    #[test]
    fn the_one_video_file_a_process_holds_is_the_file_it_plays() {
        let held = [&BESIDE[..], &[PLAYED]].concat();

        assert_eq!(the_one_video(targets(&held)), Some(a_path(PLAYED)));
    }

    #[test]
    fn two_video_files_name_neither() {
        let held = [PLAYED, "/media/video/[Group] Show Title - 04 [1080p].mkv"];

        assert_eq!(the_one_video(targets(&held)), None);
    }

    #[test]
    fn a_file_open_on_two_descriptors_is_one_file() {
        assert_eq!(
            the_one_video(targets(&[PLAYED, PLAYED])),
            Some(a_path(PLAYED))
        );
    }

    #[test]
    fn a_file_deleted_while_open_names_nothing() {
        let deleted = format!("{PLAYED} (deleted)");

        assert_eq!(the_one_video(targets(&[&deleted])), None);
    }

    #[test]
    fn a_deleted_video_beside_another_leaves_neither() {
        let deleted = "/media/video/[Group] Show Title - 02 [1080p].mkv (deleted)";

        assert_eq!(the_one_video(targets(&[deleted, PLAYED])), None);
    }

    #[test]
    fn a_process_holding_no_video_file_names_nothing() {
        assert_eq!(the_one_video(targets(&BESIDE)), None);
    }

    #[test]
    fn an_extension_is_read_in_any_case() {
        let shouted = "/media/video/[Group] Show Title - 03 [1080p].MKV";

        assert_eq!(the_one_video(targets(&[shouted])), Some(a_path(shouted)));
    }

    #[test]
    fn a_directory_named_like_a_video_is_not_one() {
        let inside = "/media/video/Show Title.mkv/notes";

        assert_eq!(the_one_video(targets(&[inside])), None);
    }

    #[test]
    fn the_links_are_read_and_the_files_they_point_at_never_are() {
        // Nothing these links point at exists. A scan that looked at the files
        // rather than at the links would find none of them.
        assert!(!Path::new(PLAYED).exists(), "{PLAYED} exists here");
        let held = descriptors(&[
            b"socket:[48213]",
            PLAYED.as_bytes(),
            b"/srv/user/.local/share/gvfs-metadata/home",
        ]);

        assert_eq!(the_video_open_in(held.path()), Some(a_path(PLAYED)));
    }

    #[test]
    fn a_name_that_is_not_utf8_survives_the_link_byte_for_byte() {
        // A Shift-JIS name, which is not valid UTF-8.
        let name: &[u8] = b"/media/video/\x83\x56\x83\x87\x81\x5b - 03.mkv";
        let held = descriptors(&[name]);

        assert_eq!(
            the_video_open_in(held.path()),
            Some(RawPath::from_bytes(name.to_vec()))
        );
    }

    #[test]
    fn descriptors_that_cannot_be_read_name_nothing() {
        let directory = tempfile::tempdir().expect("a directory of our own");

        assert_eq!(the_video_open_in(&directory.path().join("fd")), None);
    }

    #[tokio::test]
    async fn the_video_a_process_holds_is_read_from_its_descriptors() {
        // The process of this test holds the file, and no other test here
        // holds a video file open.
        let directory = tempfile::tempdir().expect("a directory of our own");
        let played = fs::canonicalize(directory.path())
            .expect("the directory's own path")
            .join("[Group] Show Title - 03 [1080p].mkv");
        let _held = File::create(&played).expect("a file to hold");

        let found =
            video_held_by(std::process::id(), Instant::now() + Duration::from_secs(5)).await;

        assert_eq!(
            found,
            Some(RawPath::from_bytes(played.into_os_string().into_vec()))
        );
    }

    #[tokio::test]
    async fn a_process_that_is_not_there_holds_nothing() {
        // Above the largest process ID Linux hands out.
        let found = video_held_by(u32::MAX, Instant::now() + Duration::from_secs(5)).await;

        assert_eq!(found, None);
    }
}
