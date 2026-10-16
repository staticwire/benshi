//! A line a person pastes at the terminal, read without being shown.

use std::io;
use std::os::fd::BorrowedFd;

use benshi_core::token::Token;
use rustix::io::Errno;
use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr,
};

/// The byte Ctrl-C sends.
const CTRL_C: u8 = 0x03;

/// The byte Ctrl-D sends.
const CTRL_D: u8 = 0x04;

/// The bytes Backspace sends, by the terminal's choice.
const BACKSPACE: [u8; 2] = [0x7f, 0x08];

/// What was typed at a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Typed {
    /// A line, without its end. Where the input ended first, what came
    /// before the end. A token is what a prompt reads, so it is held as one.
    Line(Token),
    /// Ctrl-C or Ctrl-D, at a terminal.
    Canceled,
}

/// Read one line from `input`, which a terminal does not show.
///
/// A terminal is read a byte at a time with its echo, its editing of lines
/// and its signals off. Ctrl-C and Ctrl-D cancel at once, Backspace takes
/// back the byte before it, and a line longer than the 4095 bytes Linux keeps
/// of a line it edits is read whole. The terminal is put back as it was
/// whatever the read returns with. Any other input is read as it is, to the
/// end of a line or of the input.
///
/// # Errors
///
/// What reading `input`, or setting the terminal, fails with.
pub fn hidden(input: BorrowedFd<'_>) -> io::Result<Typed> {
    let Ok(was) = tcgetattr(input) else {
        return line(input, false);
    };
    let mut quiet = was.clone();
    quiet
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG | LocalModes::IEXTEN);
    quiet.special_codes[SpecialCodeIndex::VMIN] = 1;
    quiet.special_codes[SpecialCodeIndex::VTIME] = 0;
    tcsetattr(input, OptionalActions::Now, &quiet)?;
    let _put_back = PutBack {
        terminal: input,
        was,
    };

    line(input, true)
}

/// Read to the end of a line, where the bytes of keys are read as keys at a
/// terminal and as text anywhere else.
fn line(input: BorrowedFd<'_>, at_a_terminal: bool) -> io::Result<Typed> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0];
        match rustix::io::read(input, &mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(failed) => return Err(failed.into()),
        }
        match byte[0] {
            b'\n' => break,
            b'\r' if at_a_terminal => break,
            CTRL_C | CTRL_D if at_a_terminal => return Ok(Typed::Canceled),
            key if at_a_terminal && BACKSPACE.contains(&key) => {
                line.pop();
            }
            other => line.push(other),
        }
    }

    Ok(Typed::Line(Token::new(String::from_utf8_lossy(&line))))
}

/// A terminal to set back to what it was, once it is dropped.
struct PutBack<'fd> {
    terminal: BorrowedFd<'fd>,
    was: Termios,
}

impl Drop for PutBack<'_> {
    fn drop(&mut self) {
        // The line is read or given up on, and a terminal that cannot be
        // set back leaves nothing more to do.
        tcsetattr(self.terminal, OptionalActions::Now, &self.was).ok();
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use benshi_core::token::Token;
    use rustix::fs::{Mode, OFlags};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{
        InputModes, LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr,
    };

    use super::{Typed, hidden};

    /// A bound on a read that ends on its own, so that one which does not
    /// ends the test.
    const BOUND: Duration = Duration::from_secs(5);

    /// What `hidden` reads from a pipe that carries `written` and ends.
    fn read_from_a_pipe(written: &[u8]) -> Typed {
        let (reading, mut writing) = std::io::pipe().expect("a pipe");
        writing.write_all(written).expect("the pipe takes it");
        drop(writing);

        hidden(reading.as_fd()).expect("a pipe is read")
    }

    #[test]
    fn a_line_from_a_pipe_is_read_to_its_end() {
        assert_eq!(
            read_from_a_pipe(b"a token\nwhat follows"),
            Typed::Line(Token::new("a token"))
        );
    }

    #[test]
    fn a_pipe_that_ends_first_gives_what_came_before_its_end() {
        assert_eq!(
            read_from_a_pipe(b"a token"),
            Typed::Line(Token::new("a token"))
        );
        assert_eq!(read_from_a_pipe(b""), Typed::Line(Token::new("")));
    }

    #[test]
    fn a_pipe_is_read_as_it_is() {
        // The bytes a terminal reads as keys are text in a pipe.
        assert_eq!(
            read_from_a_pipe(b"a\x03b\x04c\x7fd\re\n"),
            Typed::Line(Token::new("a\u{3}b\u{4}c\u{7f}d\re"))
        );
    }

    /// A terminal of the test's own: the side a person types at, and the
    /// side a program reads. It is no process's controlling terminal, so a
    /// key typed at it signals nobody.
    struct Terminal {
        typed_at: File,
        read_at: File,
    }

    impl Terminal {
        fn new() -> Self {
            let typed_at = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("a terminal");
            grantpt(&typed_at).expect("the terminal is granted");
            unlockpt(&typed_at).expect("the terminal is unlocked");
            let name = ptsname(&typed_at, Vec::new()).expect("the terminal has a name");
            let read_at = rustix::fs::open(
                name.as_c_str(),
                OFlags::RDWR | OFlags::NOCTTY,
                Mode::empty(),
            )
            .expect("the terminal opens");

            Self {
                typed_at: File::from(typed_at),
                read_at: File::from(read_at),
            }
        }

        fn modes(&self) -> LocalModes {
            tcgetattr(&self.read_at).expect("a terminal").local_modes
        }

        /// What `hidden` reads while `typed` is typed, once the terminal no
        /// longer shows what is typed at it.
        fn typing(&self, typed: &[u8]) -> Typed {
            let read_at = self.read_at.try_clone().expect("the terminal is shared");
            let (answer, answered) = mpsc::channel();
            thread::spawn(move || answer.send(hidden(read_at.as_fd())));
            let began = Instant::now();
            while self.modes().contains(LocalModes::ECHO) {
                assert!(
                    began.elapsed() < BOUND,
                    "the terminal went on showing what is typed"
                );
                thread::sleep(Duration::from_millis(1));
            }
            (&self.typed_at)
                .write_all(typed)
                .expect("the terminal takes keys");

            answered
                .recv_timeout(BOUND)
                .expect("the line is read within five seconds")
                .expect("the terminal is read")
        }

        /// What the terminal shows from now until the program writes `mark`.
        fn shown_until(&self, mark: &str) -> String {
            (&self.read_at)
                .write_all(mark.as_bytes())
                .expect("the program writes to the terminal");
            let mut typed_at = self.typed_at.try_clone().expect("the terminal is shared");
            let (answer, answered) = mpsc::channel();
            let mark = mark.to_owned();
            thread::spawn(move || {
                let mut shown = Vec::new();
                let mut chunk = [0; 4096];
                while !String::from_utf8_lossy(&shown).ends_with(&mark) {
                    let read = typed_at.read(&mut chunk).expect("the terminal shows");
                    shown.extend_from_slice(&chunk[..read]);
                }
                answer.send(String::from_utf8_lossy(&shown).into_owned())
            });

            answered
                .recv_timeout(BOUND)
                .expect("the mark is shown within five seconds")
        }
    }

    #[test]
    fn a_line_pasted_at_a_terminal_is_read_whole_and_shown_nowhere() {
        // Longer than the 4095 bytes a terminal keeps of a line it edits.
        let pasted = "t".repeat(5000);
        let terminal = Terminal::new();
        let modes = terminal.modes();

        let typed = terminal.typing(format!("{pasted}\r").as_bytes());

        assert_eq!(typed, Typed::Line(Token::new(pasted)));
        assert_eq!(terminal.shown_until("END"), "END");
        assert_eq!(terminal.modes(), modes, "the terminal was not put back");
    }

    #[test]
    fn ctrl_c_at_a_terminal_cancels_at_once() {
        let terminal = Terminal::new();
        let modes = terminal.modes();

        assert_eq!(terminal.typing(b"part of a token\x03"), Typed::Canceled);
        assert_eq!(terminal.modes(), modes, "the terminal was not put back");
    }

    #[test]
    fn ctrl_d_at_a_terminal_cancels_at_once() {
        let terminal = Terminal::new();
        let modes = terminal.modes();

        assert_eq!(terminal.typing(b"\x04"), Typed::Canceled);
        assert_eq!(terminal.modes(), modes, "the terminal was not put back");
    }

    #[test]
    fn enter_ends_the_line_where_the_terminal_passes_it_as_a_return() {
        // A terminal that does not turn the return Enter sends into a new
        // line, as one a program before left in another state.
        let terminal = Terminal::new();
        let mut modes = tcgetattr(&terminal.read_at).expect("a terminal");
        modes.input_modes.remove(InputModes::ICRNL);
        tcsetattr(&terminal.read_at, OptionalActions::Now, &modes).expect("the terminal is set");

        assert_eq!(terminal.typing(b"abc\r"), Typed::Line(Token::new("abc")));
    }

    #[test]
    fn a_read_waits_for_a_key_where_the_terminal_was_left_not_to() {
        // A terminal a program before left returning from a read at once.
        let terminal = Terminal::new();
        let mut modes = tcgetattr(&terminal.read_at).expect("a terminal");
        modes.special_codes[SpecialCodeIndex::VMIN] = 0;
        modes.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(&terminal.read_at, OptionalActions::Now, &modes).expect("the terminal is set");

        assert_eq!(terminal.typing(b"abc\r"), Typed::Line(Token::new("abc")));
    }

    #[test]
    fn backspace_at_a_terminal_takes_back_what_was_typed_last() {
        let terminal = Terminal::new();

        assert_eq!(
            terminal.typing(b"abx\x7fc\x08d\n"),
            Typed::Line(Token::new("abd"))
        );
    }
}
