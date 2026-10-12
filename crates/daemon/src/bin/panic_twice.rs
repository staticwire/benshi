//! Supervise a task that panics at its first two attempts, with every panic
//! of the process said through a voice on stderr, and say on stdout what the
//! supervisor told of and what the task did.
//!
//! Its one argument asks for something else. With `voice`, the thread of the
//! voice panics in place of a task, at the first line it writes. With
//! `after`, the main thread panics once that thread has ended. With `own`,
//! the thread of the voice is the last to let go of it. With `last`, a
//! voice that writes slowly is handed three lines and let go of.
//!
//! Exists for the tests of what a panic says, because a panic hook is one to
//! a process. It is not a command anybody runs by hand.

// `println!` panics where the line cannot be written. A test reads the
// lines, and fails where this ends without them.
#![allow(clippy::print_stdout)]

use std::io::{self, Write};
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use benshi_daemon::panics;
use benshi_daemon::supervisor::Supervisor;
use benshi_daemon::voice::Voice;

/// A sink that panics at its first write, as no stream of the system does.
struct Panics {
    /// Let go of as the thread that writes unwinds, which is after the hook
    /// has said the panic.
    _unwound: Sender<()>,
}

impl Write for Panics {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        panic!("the sink panics");
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A sink that lets go of the last hold on the voice it writes for, at its
/// first write.
struct LetsGo {
    /// The voice, handed over once nothing else holds it.
    held: Receiver<Arc<Voice>>,
    /// Let go of as the thread that writes ends.
    _ended: Sender<()>,
}

impl Write for LetsGo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // The voice goes here, on the thread that writes for it.
        drop(self.held.recv());

        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Stderr, a while after it is asked: a process that ends without waiting
/// for the voice ends ahead of the first line.
struct Slow;

impl Write for Slow {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        thread::sleep(Duration::from_millis(100));

        io::stderr().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        // On a thread with a name of its own, which is what a panic is said
        // with.
        None => thread::Builder::new()
            .name("runs".to_owned())
            .spawn(a_task_panics)
            .expect("a thread starts")
            .join()
            .expect("the thread ends"),
        Some("voice") => drop(the_voice_panics()),
        Some("after") => {
            // Kept, so that the hook finds a voice that is still there.
            let _voice = the_voice_panics();
            panic!("the main thread panics once the voice has ended");
        }
        Some("own") => the_voice_is_let_go_of_by_its_own_thread(),
        Some("last") => the_voice_goes(),
        Some(other) => panic!("{other} is no mode of this helper"),
    }
}

fn a_task_panics() {
    let voice = Arc::new(Voice::on(io::stderr()).expect("a thread starts"));
    panics::are_said_through(&voice);

    let mut supervisor = Supervisor::telling(|notice| println!("told: {notice}"));
    let attempts = Arc::new(AtomicU32::new(0));
    supervisor.supervise("flaky", move || {
        let attempts = Arc::clone(&attempts);
        async move {
            match attempts.fetch_add(1, Ordering::SeqCst) + 1 {
                1 => panic!("attempt 1 panics"),
                // A panic that carries nothing a person can read.
                2 => panic::panic_any(2_u32),
                attempt => println!("attempt {attempt} runs"),
            }

            Ok(())
        }
    });

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime starts")
        .block_on(supervisor.run());
    println!("every task has ended");
}

/// A voice whose thread has panicked and unwound, at the one line it was
/// handed.
fn the_voice_panics() -> Arc<Voice> {
    let (unwound, has_unwound) = mpsc::channel();
    let voice = Arc::new(Voice::on(Panics { _unwound: unwound }).expect("a thread starts"));
    panics::are_said_through(&voice);

    voice.say(format_args!("a line the sink panics at"));
    // Kept until the thread has unwound, so that the hook finds a voice
    // that is still there.
    has_unwound
        .recv()
        .expect_err("the sink is dropped, and sends nothing");
    println!("the voice has gone");

    voice
}

fn the_voice_is_let_go_of_by_its_own_thread() {
    let (hand_over, held) = mpsc::channel();
    let (ended, has_ended) = mpsc::channel();
    let voice = Arc::new(
        Voice::on(LetsGo {
            held,
            _ended: ended,
        })
        .expect("a thread starts"),
    );
    panics::are_said_through(&voice);

    voice.say(format_args!("a line the sink lets go of the voice at"));
    // The one hold on the voice, which the sink lets go of as it writes.
    hand_over.send(voice).expect("the sink waits for the voice");
    has_ended
        .recv()
        .expect_err("the sink is dropped, and sends nothing");
    println!("the voice has gone");
}

fn the_voice_goes() {
    let voice = Arc::new(Voice::on(Slow).expect("a thread starts"));
    panics::are_said_through(&voice);

    for line in ["one", "two", "three"] {
        voice.say(format_args!("{line}"));
    }
}
