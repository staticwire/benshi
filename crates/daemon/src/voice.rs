//! What the daemon says, written by a thread of its own.
//!
//! The daemon says things on stderr: where it listens, where it records, and
//! what happens to each of its tasks. A pipe, a socket and a terminal each
//! hold so much, and a write to one that is full waits until somebody reads
//! it. The supervisor tells of a task before it deals with the next one that
//! ended, so a daemon that wrote as it told would start no task again while
//! nobody read. One held at a line it says as it starts would serve nobody on
//! the socket it had bound.
//!
//! So a [`Voice`] takes a line and returns, and a thread of its own writes
//! the lines in the order they were said.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::mem;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

/// How many lines a [`Voice`] holds that wait to be written, beside the one
/// being written.
pub const HELD: usize = 256;

/// What the daemon says, a line at a time, under the name of the program.
///
/// Saying a line hands it over and waits for nobody to read it. The voice
/// holds [`HELD`] lines that wait to be written, beside the one being
/// written, and one more pushes out the line it has held longest: what a task
/// said last is what it is doing now. How many were pushed out is written where they would have
/// stood, so that a daemon that dropped a hundred lines does not read as one
/// that had nothing to say.
///
/// A voice that goes has written every line it still held, or has failed to:
/// dropping it waits for the thread that writes. A daemon that ends over a
/// failure says why in its last line, and the process must not end ahead of
/// that line. So a daemon that ends while nobody reads waits to be read, as
/// it would had it written the line itself.
pub struct Voice {
    unwritten: Arc<Unwritten>,
    held: usize,
    writer: Option<JoinHandle<()>>,
}

/// The lines that are said and not written, as the thread that says and the
/// thread that writes share them.
struct Unwritten {
    lines: Mutex<Lines>,
    /// Told of a line that is said and of a voice that goes.
    changed: Condvar,
}

struct Lines {
    /// In the order they were said.
    said: VecDeque<String>,
    /// How many were pushed out since a line was last taken to be written.
    dropped: u64,
    /// The voice has gone, and nothing more is said.
    over: bool,
}

impl Unwritten {
    fn lines(&self) -> MutexGuard<'_, Lines> {
        // A line is added or taken in one step and no step is left half
        // made, so lines a panic left held are as sound as any.
        self.lines.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next line to write and how many were pushed out ahead of it,
    /// waited for. Nothing once the voice has gone and every line is taken.
    fn next(&self) -> Option<(u64, String)> {
        let mut lines = self.lines();
        loop {
            if let Some(line) = lines.said.pop_front() {
                return Some((mem::take(&mut lines.dropped), line));
            }
            if lines.over {
                return None;
            }
            lines = self
                .changed
                .wait(lines)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// The line that stands where this many lines were pushed out.
fn dropped_here(lines: u64) -> String {
    if lines == 1 {
        "benshi: 1 line was dropped here while nobody was reading\n".to_owned()
    } else {
        format!("benshi: {lines} lines were dropped here while nobody was reading\n")
    }
}

/// Writes every line that is said, until the voice has gone.
fn write(unwritten: &Unwritten, mut sink: impl Write) {
    while let Some((dropped, line)) = unwritten.next() {
        // A line that cannot be written is passed over: the next may be one
        // that can be written.
        if dropped > 0 {
            drop(sink.write_all(dropped_here(dropped).as_bytes()));
        }
        drop(sink.write_all(line.as_bytes()));
        drop(sink.flush());
    }
}

impl Voice {
    /// A voice that writes what it is handed to `sink`, on a thread it
    /// starts.
    ///
    /// # Errors
    ///
    /// Returns what the operating system answered where the thread cannot be
    /// started.
    pub fn on(sink: impl Write + Send + 'static) -> io::Result<Self> {
        Self::holding(sink, HELD)
    }

    /// A voice that holds this many lines that are said and not written.
    fn holding(sink: impl Write + Send + 'static, held: usize) -> io::Result<Self> {
        let unwritten = Arc::new(Unwritten {
            lines: Mutex::new(Lines {
                said: VecDeque::new(),
                dropped: 0,
                over: false,
            }),
            changed: Condvar::new(),
        });
        let writer = thread::Builder::new().name("voice".to_owned()).spawn({
            let unwritten = Arc::clone(&unwritten);
            move || write(&unwritten, sink)
        })?;

        Ok(Self {
            unwritten,
            held,
            writer: Some(writer),
        })
    }

    /// Says a line, and does not wait for it to be written.
    ///
    /// A line is one write. A pipe takes a write of up to `PIPE_BUF` bytes
    /// whole, so a line no longer than that stays apart from what another
    /// program writes to the same pipe. A line that cannot be written is
    /// passed over.
    pub fn say(&self, line: fmt::Arguments<'_>) {
        let line = format!("benshi: {line}\n");
        let mut lines = self.unwritten.lines();
        if lines.said.len() >= self.held && lines.said.pop_front().is_some() {
            lines.dropped += 1;
        }
        lines.said.push_back(line);
        drop(lines);
        self.unwritten.changed.notify_one();
    }
}

impl Drop for Voice {
    fn drop(&mut self) {
        self.unwritten.lines().over = true;
        self.unwritten.changed.notify_one();
        if let Some(writer) = self.writer.take() {
            // A writer that panicked has written what it could, and a voice
            // that goes has nowhere left to say so.
            drop(writer.join());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HELD, Voice};
    use std::io::{self, Write};
    use std::ops::RangeInclusive;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
    use std::thread;
    use std::time::Duration;

    /// How long a test waits for a thread before calling it lost. Only ever
    /// spent on a failure.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// One thing a voice did to what it writes to.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Done {
        /// One write, with what it wrote.
        Wrote(String),
        Flushed,
    }

    fn wrote(line: &str) -> Done {
        Done::Wrote(line.to_owned())
    }

    /// What a voice writes to in a test.
    #[derive(Clone, Default)]
    struct Sink(Arc<(Mutex<Taken>, Condvar)>);

    #[derive(Default)]
    struct Taken {
        done: Vec<Done>,
        /// A write waits for as long as this holds, as one does to a stream
        /// nobody reads.
        shut: bool,
        /// How many writes have come, the ones that wait among them.
        come: usize,
        /// How many writes fail before one is taken.
        refused: usize,
        /// How long a write takes.
        slow: Duration,
    }

    impl Sink {
        fn shut() -> Self {
            let sink = Self::default();
            sink.taken().shut = true;

            sink
        }

        fn slow(by: Duration) -> Self {
            let sink = Self::default();
            sink.taken().slow = by;

            sink
        }

        fn refusing(writes: usize) -> Self {
            let sink = Self::default();
            sink.taken().refused = writes;

            sink
        }

        fn taken(&self) -> MutexGuard<'_, Taken> {
            self.0.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn open(&self) {
            self.taken().shut = false;
            self.0.1.notify_all();
        }

        /// Waits until a write has come, which a sink that is shut keeps
        /// waiting.
        fn until_a_write_comes(&self) {
            self.until_writes_come(1);
        }

        /// Waits until this many writes have come.
        fn until_writes_come(&self, count: usize) {
            let (taken, timed_out) = self
                .0
                .1
                .wait_timeout_while(self.taken(), PATIENCE, |taken| taken.come < count)
                .unwrap_or_else(PoisonError::into_inner);

            assert!(
                !timed_out.timed_out(),
                "{} writes came where {count} were waited for, and {:?} was done",
                taken.come,
                taken.done
            );
        }

        fn done(&self) -> Vec<Done> {
            self.taken().done.clone()
        }

        /// Everything that was written, as it reads.
        fn written(&self) -> String {
            self.done()
                .into_iter()
                .filter_map(|done| match done {
                    Done::Wrote(written) => Some(written),
                    Done::Flushed => None,
                })
                .collect()
        }
    }

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let slow = {
                let mut taken = self.taken();
                taken.come += 1;
                self.0.1.notify_all();
                while taken.shut {
                    taken = self.0.1.wait(taken).unwrap_or_else(PoisonError::into_inner);
                }

                taken.slow
            };
            // Slept for with the sink let go of, so that a test that reads
            // what was written does not wait for a write that is under way.
            thread::sleep(slow);

            let mut taken = self.taken();
            if taken.refused > 0 {
                taken.refused -= 1;
                return Err(io::Error::other("the sink takes nothing"));
            }
            taken
                .done
                .push(Done::Wrote(String::from_utf8_lossy(bytes).into_owned()));

            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.taken().done.push(Done::Flushed);

            Ok(())
        }
    }

    /// Says these lines on a thread of its own, which hands the voice back
    /// once it has said them. A voice that waits to say then fails the test
    /// where it would otherwise hold it for ever.
    fn saying(voice: Voice, lines: RangeInclusive<u32>) -> Receiver<Voice> {
        let (said, all_said) = mpsc::channel();
        thread::spawn(move || {
            for line in lines {
                voice.say(format_args!("{line}"));
            }
            // Nobody waits for it once the test has failed.
            drop(said.send(voice));
        });

        all_said
    }

    fn said(voice: Voice, lines: RangeInclusive<u32>) -> Voice {
        saying(voice, lines)
            .recv_timeout(PATIENCE)
            .expect("saying waited for a sink that takes nothing")
    }

    /// Lets the voice go on a thread of its own, so that a voice that never
    /// goes, or panics as it goes, fails the test as well.
    fn gone(voice: Voice) {
        let (going, has_gone) = mpsc::channel::<()>();
        let dropping = thread::spawn(move || {
            drop(voice);
            // Let go of once the voice has gone, which is what the test is
            // told of.
            drop(going);
        });

        assert_eq!(
            has_gone.recv_timeout(PATIENCE),
            Err(RecvTimeoutError::Disconnected),
            "the voice did not go"
        );
        // A panic as the voice goes lets go of `going` as well.
        assert!(dropping.join().is_ok(), "the voice panicked as it went");
    }

    #[test]
    fn what_is_said_is_written_in_the_order_it_was_said() {
        let sink = Sink::default();
        let voice = Voice::on(sink.clone()).expect("a thread starts");

        voice.say(format_args!("listening on {}", "a socket"));
        voice.say(format_args!("recording in a file"));
        gone(voice);

        assert_eq!(
            sink.written(),
            "benshi: listening on a socket\nbenshi: recording in a file\n"
        );
    }

    #[test]
    fn a_line_is_one_write_with_a_flush_after_it() {
        // Two writers to one pipe keep their lines apart where a line is one
        // write, and a sink that gathers what it is handed would keep a line
        // from whoever reads.
        let sink = Sink::default();
        let voice = Voice::on(sink.clone()).expect("a thread starts");

        voice.say(format_args!("one"));
        voice.say(format_args!("two"));
        gone(voice);

        assert_eq!(
            sink.done(),
            [
                wrote("benshi: one\n"),
                Done::Flushed,
                wrote("benshi: two\n"),
                Done::Flushed,
            ]
        );
    }

    #[test]
    fn a_voice_that_goes_has_written_what_it_was_handed() {
        // The process ends once the voice has gone, and a daemon that ends
        // over a failure says why in its last line.
        let sink = Sink::slow(Duration::from_millis(20));
        let voice = Voice::on(sink.clone()).expect("a thread starts");

        let voice = said(voice, 1..=5);
        gone(voice);

        assert_eq!(
            sink.written(),
            "benshi: 1\nbenshi: 2\nbenshi: 3\nbenshi: 4\nbenshi: 5\n"
        );
    }

    #[test]
    fn saying_does_not_wait_for_what_is_written() {
        let sink = Sink::shut();
        let voice = Voice::holding(sink.clone(), 2).expect("a thread starts");
        voice.say(format_args!("the one being written"));
        sink.until_a_write_comes();

        // Said while the thread that writes waits in a write, as it does
        // where nobody reads.
        let said = saying(voice, 1..=10).recv_timeout(PATIENCE);

        sink.open();
        let voice = said.expect("saying waited for a sink that takes nothing");
        gone(voice);
    }

    #[test]
    fn a_voice_that_is_full_lets_go_of_the_lines_it_held_longest() {
        let sink = Sink::shut();
        let voice = Voice::on(sink.clone()).expect("a thread starts");
        voice.say(format_args!("the one being written"));
        sink.until_a_write_comes();
        let held = u32::try_from(HELD).expect("a voice holds fewer lines than that");

        let voice = said(voice, 1..=held + 2);
        sink.open();
        gone(voice);

        let written = sink.written();
        let mut written = written.lines();
        assert_eq!(written.next(), Some("benshi: the one being written"));
        assert_eq!(
            written.next(),
            Some("benshi: 2 lines were dropped here while nobody was reading")
        );
        let kept: Vec<String> = (3..=held + 2)
            .map(|line| format!("benshi: {line}"))
            .collect();
        assert_eq!(written.collect::<Vec<_>>(), kept);
    }

    #[test]
    fn one_line_let_go_of_is_said_as_one() {
        let sink = Sink::shut();
        let voice = Voice::holding(sink.clone(), 3).expect("a thread starts");
        voice.say(format_args!("the one being written"));
        sink.until_a_write_comes();

        let voice = said(voice, 1..=4);
        sink.open();
        gone(voice);

        assert_eq!(
            sink.written(),
            "benshi: the one being written\n\
             benshi: 1 line was dropped here while nobody was reading\n\
             benshi: 2\n\
             benshi: 3\n\
             benshi: 4\n"
        );
    }

    #[test]
    fn a_line_said_to_a_voice_that_waits_for_one_is_written() {
        let sink = Sink::default();
        let voice = Voice::on(sink.clone()).expect("a thread starts");
        voice.say(format_args!("said as it starts"));
        sink.until_a_write_comes();
        // Long enough for the thread that writes to have gone back to waiting
        // for a line, as it is for most of the time a daemon runs.
        thread::sleep(Duration::from_millis(50));

        voice.say(format_args!("said a while after"));
        sink.until_writes_come(2);

        gone(voice);
        assert_eq!(
            sink.written(),
            "benshi: said as it starts\nbenshi: said a while after\n"
        );
    }

    #[test]
    fn a_voice_with_nothing_left_to_write_goes() {
        let sink = Sink::default();
        let voice = Voice::on(sink.clone()).expect("a thread starts");
        voice.say(format_args!("the only line"));
        sink.until_a_write_comes();
        // Long enough for the thread that writes to have gone back to waiting
        // for a line, which is where a voice that goes has to reach it.
        thread::sleep(Duration::from_millis(50));

        gone(voice);

        assert_eq!(sink.written(), "benshi: the only line\n");
    }

    #[test]
    fn a_line_that_cannot_be_written_is_passed_over() {
        // Nobody is there to read it, and the next line may be one that can
        // be written.
        let sink = Sink::refusing(1);
        let voice = Voice::on(sink.clone()).expect("a thread starts");

        voice.say(format_args!("to a terminal that has gone"));
        voice.say(format_args!("to one that is back"));
        gone(voice);

        assert_eq!(sink.written(), "benshi: to one that is back\n");
    }
}
