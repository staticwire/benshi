//! What a panic says, and that saying it waits for nobody.
//!
//! A panic hook is one to a process, so no test of the library can set one
//! beside the tests it runs with. `panic_twice` is a binary of this crate for
//! this one purpose: it supervises a task that panics at its first two
//! attempts, with its panics said through a voice on stderr, and says on
//! stdout what the supervisor told of.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a test waits for a line, or for a helper to end, before calling
/// it lost. Only ever spent on a failure.
///
/// Real time, and far above the 0.3 s the slowest helper sleeps: a busy
/// machine can hold a helper up for seconds.
const PATIENCE: Duration = Duration::from_secs(30);

/// How a helper ends whose `main` returned.
const RETURNED: Option<i32> = Some(0);

/// How a helper ends whose main thread panicked.
const PANICKED: Option<i32> = Some(101);

/// The first lines `panic_twice` says on stdout, a line to an entry: the
/// supervisor tells of each panic, and the third attempt runs.
const SHOWN: [&str; 3] = [
    "told: flaky panicked and is started again: attempt 1 panics",
    "told: flaky panicked and is started again: panic carrying no message",
    "attempt 3 runs",
];

/// Where `panic_twice` panics, without the line and the column.
const PLACE: &str = "crates/daemon/src/bin/panic_twice.rs:";

/// What is said once, at the first panic that has no backtrace.
const HOW_TO_ASK: &str = "benshi: run with RUST_BACKTRACE=1 for a backtrace";

/// `panic_twice`, with nothing of the environment of the test.
fn panic_twice() -> Command {
    let mut helper = Command::new(env!("CARGO_BIN_EXE_panic_twice"));
    helper.env_clear().stdin(Stdio::null());

    helper
}

/// A helper that is killed and waited for when the test lets go of it.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        // One that has ended already is not there to kill.
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

/// A stream that is full, and the end of it that nobody reads.
///
/// The test keeps that end for as long as the helper runs: a write to a
/// stream whose other end has gone fails, where a write to this one waits.
fn nobody_reads() -> (UnixStream, Stdio) {
    let (unread, full) = UnixStream::pair().expect("a pair of sockets can be made");
    // Filled without waiting, and handed over as a stream that waits.
    full.set_nonblocking(true)
        .expect("a socket can be kept from waiting");
    let filling = [b'.'; 4096];
    loop {
        match (&full).write(&filling) {
            Ok(_taken) => {}
            Err(waits) if waits.kind() == ErrorKind::WouldBlock => break,
            Err(other) => panic!("the stream could not be filled: {other}"),
        }
    }
    full.set_nonblocking(false)
        .expect("a socket can be made to wait");

    (unread, OwnedFd::from(full).into())
}

/// The lines a helper says on stdout, read on a thread of its own, so that a
/// helper that says nothing fails the test where it would otherwise hold it.
fn lines_of(helper: &mut Running) -> Receiver<String> {
    let stdout = helper.0.stdout.take().expect("stdout is a pipe");
    let (said, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            // Nobody waits for it once the test has failed.
            if said.send(line).is_err() {
                break;
            }
        }
    });

    lines
}

/// What a helper that ended said, and how it ended.
struct Heard {
    /// Nothing where a signal ended it.
    code: Option<i32>,
    /// On stdout.
    shown: String,
    /// On stderr.
    said: String,
}

/// Everything a stream holds, read on a thread of its own.
fn all_of(stream: impl Read + Send + 'static) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        let mut stream = stream;
        stream
            .read_to_string(&mut text)
            .expect("the helper says text");

        text
    })
}

/// Runs a helper to its end with both of its streams read, and fails where
/// it does not end.
fn heard(helper: &mut Command) -> Heard {
    let mut running = Running(
        helper
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the helper starts"),
    );
    let shown = all_of(running.0.stdout.take().expect("stdout is a pipe"));
    let said = all_of(running.0.stderr.take().expect("stderr is a pipe"));
    let waited_from = Instant::now();
    let ended = loop {
        if let Some(ended) = running.0.try_wait().expect("the helper can be waited for") {
            break ended;
        }
        assert!(
            waited_from.elapsed() < PATIENCE,
            "the helper did not end in {PATIENCE:?}"
        );
        thread::sleep(Duration::from_millis(2));
    };

    Heard {
        code: ended.code(),
        shown: shown.join().expect("stdout is read"),
        said: said.join().expect("stderr is read"),
    }
}

/// Holds that `line` is a panic of the task of `panic_twice`, said with its
/// thread, its place and this message.
fn is_the_panic_that_says(message: &str, line: Option<&str>) {
    let line = line.expect("a panic is said");
    let place = line
        .strip_prefix(&format!("benshi: thread 'runs' panicked at {PLACE}"))
        .and_then(|rest| rest.strip_suffix(&format!(": {message}")))
        .unwrap_or_else(|| panic!("the panic that says {message:?} was said as {line:?}"));
    let numbers: Vec<&str> = place.split(':').collect();

    assert!(
        numbers.len() == 2 && numbers.iter().all(|number| number.parse::<u32>().is_ok()),
        "the place ends with {place:?}, which is no line and column"
    );
}

#[test]
fn a_task_that_panics_is_started_again_while_nobody_reads() {
    // The hook a process starts with writes the panic on the thread that
    // panicked. Where nobody reads, that write waits, the task does not end
    // and the supervisor is told of nothing.
    let (_unread, stderr) = nobody_reads();
    let mut helper = Running(
        panic_twice()
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("the helper starts"),
    );
    let said = lines_of(&mut helper);

    for line in SHOWN {
        assert_eq!(
            said.recv_timeout(PATIENCE).as_deref(),
            Ok(line),
            "the task was held by a panic nobody read"
        );
    }
}

#[test]
fn a_panic_is_a_line_that_names_the_thread_the_place_and_the_message() {
    // With no backtrace asked for, and with one refused.
    for refused in [false, true] {
        let mut helper = panic_twice();
        if refused {
            helper.env("RUST_BACKTRACE", "0");
        }

        let Heard { code, shown, said } = heard(&mut helper);

        assert_eq!(code, RETURNED, "stderr was {said:?}");
        assert_eq!(
            shown.lines().take(SHOWN.len()).collect::<Vec<_>>(),
            SHOWN,
            "stderr was {said:?}"
        );
        let mut said = said.lines();
        is_the_panic_that_says("attempt 1 panics", said.next());
        // Once, at the first panic: a daemon that panics every minute does
        // not say it every minute.
        assert_eq!(said.next(), Some(HOW_TO_ASK));
        is_the_panic_that_says("no message", said.next());
        assert_eq!(said.next(), None);
    }
}

#[test]
fn a_panic_carries_its_backtrace_where_one_is_asked_for() {
    let Heard { code, said, .. } = heard(panic_twice().env("RUST_BACKTRACE", "1"));

    assert_eq!(code, RETURNED, "stderr was {said:?}");
    let mut said = said.lines();
    is_the_panic_that_says("attempt 1 panics", said.next());
    assert_eq!(said.next(), Some("stack backtrace:"));
    let traced: Vec<&str> = said.collect();
    // The frames of the first panic end at the line of the second, which
    // names the helper in its place.
    let mut frames = traced
        .iter()
        .take_while(|line| !line.starts_with("benshi: "));
    assert!(
        frames.any(|frame| frame.contains("panic_twice")),
        "the backtrace does not reach the helper: {traced:?}"
    );
    assert!(
        !traced.contains(&HOW_TO_ASK),
        "a backtrace was offered where one was given: {traced:?}"
    );
}

#[test]
fn a_panic_of_the_voice_is_written_by_the_hook_that_was_there() {
    // The thread that writes ends with its panic, and a line said to the
    // voice from then on is written by nobody. Its panic is left to the hook
    // the process started with, which writes it where it stands.
    let Heard { code, shown, said } = heard(panic_twice().arg("voice"));

    assert_eq!((code, shown.as_str()), (RETURNED, "the voice has gone\n"));
    assert!(
        said.trim_start().starts_with("thread 'voice'"),
        "stderr was {said:?}"
    );
    assert!(said.contains("the sink panics"), "stderr was {said:?}");
}

#[test]
fn a_panic_once_the_thread_of_the_voice_has_ended_is_written_as_well() {
    // A line handed to a voice whose thread has ended is written by nobody,
    // so a panic after that is left to the hook that was there too.
    let Heard { code, said, .. } = heard(panic_twice().arg("after"));

    assert_eq!(code, PANICKED, "stderr was {said:?}");
    assert!(
        said.contains("the main thread panics once the voice has ended"),
        "stderr was {said:?}"
    );
}

#[test]
fn a_voice_let_go_of_last_by_its_own_thread_goes() {
    // The hook holds the voice while it says a panic of the thread that
    // writes. Where everything else lets go of the voice in that time, that
    // thread lets go of it last, and it cannot wait for itself to end.
    let Heard { code, shown, said } = heard(panic_twice().arg("own"));

    assert_eq!(
        (code, shown.as_str(), said.as_str()),
        (RETURNED, "the voice has gone\n", "")
    );
}

#[test]
fn a_voice_that_panics_are_said_through_still_goes_with_its_last_line() {
    // A daemon that ends over a failure says why in its last line, and the
    // process ends once the voice has gone. A hook that kept the voice would
    // keep it from going, and the process would end ahead of the line.
    let Heard { code, said, .. } = heard(panic_twice().arg("last"));

    assert_eq!(code, RETURNED);
    assert_eq!(said, "benshi: one\nbenshi: two\nbenshi: three\n");
}
