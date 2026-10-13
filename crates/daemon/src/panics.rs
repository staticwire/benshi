//! What a panic says, said through the daemon's voice.
//!
//! The hook a process starts with writes on stderr, on the thread that
//! panicked and before the panic unwinds. With a stderr that is full and that
//! nobody reads, that write waits: the task that panicked does not end, the
//! supervisor is told of nothing, and the task is not started again. A panic
//! said through a [`Voice`] waits for nobody.
//!
//! A panic is said as one line: the thread, the place and the message. A
//! backtrace stands under it where the environment asks for one, and the
//! first panic that has none is followed by how to ask.
//!
//! Three panics are left to the hook that was there, which writes them where
//! they stand. One is a panic of the thread that writes for the voice: that
//! thread ends with its panic. Another is a panic after that thread has
//! ended, since a line said to the voice from then on is written by nobody.
//! The last is a panic once the voice has gone. Where a process makes more
//! than one voice, the one made last says its panics for as long as it is
//! there.

use std::backtrace::Backtrace;
use std::env;
use std::fmt::{self, Display};
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::voice::Voice;

/// What stands for the message of a panic that carries none a person can
/// read.
const NO_MESSAGE: &str = "no message";

/// What is said once, at the first panic, where no backtrace is asked for.
const HOW_TO_ASK: &str = "run with RUST_BACKTRACE=1 for a backtrace";

/// Has the panics of the process said through `voice`, but for the three
/// the text of this module leaves to the hook that was there.
///
/// A backtrace is asked for as the hook that was there reads it:
/// `RUST_BACKTRACE` is set, to anything but `0`.
///
/// The hook holds the voice weakly, and takes hold of it for as long as it
/// says one panic.
pub(crate) fn are_said_through(voice: &Arc<Voice>) {
    let voice = Arc::downgrade(voice);
    let found = panic::take_hook();
    let traced = env::var_os("RUST_BACKTRACE").is_some_and(|asked| asked != "0");
    let asked_how = AtomicBool::new(false);

    panic::set_hook(Box::new(move |panic| {
        let voice = match voice.upgrade() {
            Some(voice) if voice.writes_for_this_thread() => voice,
            _ => return found(panic),
        };
        let backtrace = traced.then(Backtrace::force_capture);

        voice.say(format_args!(
            "{}",
            Said {
                thread: thread::current().name(),
                place: panic.location(),
                message: panic.payload_as_str().unwrap_or(NO_MESSAGE),
                backtrace: backtrace.as_ref(),
            }
        ));
        if !traced && !asked_how.swap(true, Ordering::Relaxed) {
            voice.say(format_args!("{HOW_TO_ASK}"));
        }
    }));
}

/// A panic as it is said: one line, and a backtrace under it where there is
/// one.
struct Said<'a, P, B> {
    thread: Option<&'a str>,
    place: Option<P>,
    message: &'a str,
    backtrace: Option<B>,
}

impl<P: Display, B: Display> Display for Said<'_, P, B> {
    fn fmt(&self, into: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            into,
            "thread '{}' panicked",
            self.thread.unwrap_or("<unnamed>")
        )?;
        if let Some(place) = &self.place {
            write!(into, " at {place}")?;
        }
        write!(into, ": {}", self.message)?;
        if let Some(backtrace) = &self.backtrace {
            write!(into, "\nstack backtrace:\n{backtrace}")?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Said;

    #[test]
    fn a_panic_is_said_with_its_thread_its_place_and_its_message() {
        let said = Said::<_, &str> {
            thread: Some("worker"),
            place: Some("src/lib.rs:7:9"),
            message: "index out of bounds",
            backtrace: None,
        };

        assert_eq!(
            said.to_string(),
            "thread 'worker' panicked at src/lib.rs:7:9: index out of bounds"
        );
    }

    #[test]
    fn a_thread_with_no_name_is_said_as_one() {
        let said = Said::<&str, &str> {
            thread: None,
            place: None,
            message: "index out of bounds",
            backtrace: None,
        };

        assert_eq!(
            said.to_string(),
            "thread '<unnamed>' panicked: index out of bounds"
        );
    }

    #[test]
    fn a_backtrace_stands_under_the_line_of_its_panic() {
        let said = Said {
            thread: Some("worker"),
            place: Some("src/lib.rs:7:9"),
            message: "index out of bounds",
            backtrace: Some("   0: one\n   1: two\n"),
        };

        assert_eq!(
            said.to_string(),
            "thread 'worker' panicked at src/lib.rs:7:9: index out of bounds\n\
             stack backtrace:\n   0: one\n   1: two\n"
        );
    }
}
