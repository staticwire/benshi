//! Speaking to a running daemon over its local socket.
//!
//! One request per line, one answer per line, exactly as the daemon writes
//! them. Nothing here decides anything or renders anything: it carries
//! messages, and `commands` does the rest.
//!
//! A connection waits for its first answer until [`DEADLINE`] from the moment
//! it began to connect, and no longer. What follows the first answer of a
//! `watch` is a stream, which waits for as long as the user does.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use benshi_daemon::protocol::{Request, Response};
use humantime::format_duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::time::{Instant, timeout_at};

/// How long a connection waits for its first answer, counted from the moment
/// it began to connect.
///
/// A daemon that listens answers in a few milliseconds. One that has bound its
/// socket and is still opening its database answers once it listens, and that
/// is within a second and some: the wait inside SQLite is a second, and a
/// connection made before the daemon listens waits in the socket's backlog
/// until it does. Five seconds is several times that, and is what a person at
/// a terminal is kept waiting before being told that the daemon does not
/// answer.
pub const DEADLINE: Duration = Duration::from_secs(5);

/// A connection to a running daemon.
#[derive(Debug)]
pub struct Daemon {
    socket: PathBuf,
    /// When the first answer is due.
    answer_by: Instant,
    writer: WriteHalf<UnixStream>,
    lines: Lines<BufReader<ReadHalf<UnixStream>>>,
}

impl Daemon {
    /// Connect to the daemon listening on `socket`.
    ///
    /// # Errors
    ///
    /// Says where it looked when nothing is listening there. A client that
    /// reported only that it had failed would leave the user guessing between
    /// a daemon that is not running and one listening somewhere else, which
    /// are fixed differently. Says that the daemon is not accepting
    /// connections where the socket's backlog has no room for one more: the
    /// connect is answered `EAGAIN` there, and something holds that socket.
    /// Says that the daemon did not answer where the connection is not made
    /// within [`DEADLINE`] of its start.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let answer_by = Instant::now() + DEADLINE;
        let connected = timeout_at(answer_by, UnixStream::connect(socket))
            .await
            .map_err(|_elapsed| did_not_answer(socket))?;
        let stream = match connected {
            Err(full) if full.kind() == io::ErrorKind::WouldBlock => Err(full).with_context(|| {
                format!(
                    "daemon at {} is not accepting connections",
                    socket.display()
                )
            }),
            connected => connected.with_context(|| format!("no daemon at {}", socket.display())),
        }?;
        let (reader, writer) = tokio::io::split(stream);

        Ok(Self {
            socket: socket.to_path_buf(),
            answer_by,
            writer,
            lines: BufReader::new(reader).lines(),
        })
    }

    /// Send one request.
    ///
    /// A request longer than the socket's send buffer is sent only as the
    /// daemon reads it, so the send waits under the deadline as well.
    ///
    /// # Errors
    ///
    /// Returns whatever the socket did, and that the daemon did not answer
    /// where the request is not sent within [`DEADLINE`] of the start of the
    /// connection.
    pub async fn ask(&mut self, request: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(request).context("request could not be written")?;
        line.push(b'\n');

        let sent = timeout_at(self.answer_by, async {
            self.writer.write_all(&line).await?;
            self.writer.flush().await
        })
        .await
        .map_err(|_elapsed| did_not_answer(&self.socket))?;

        sent.context("request could not be sent")
    }

    /// The next answer, or nothing once the daemon has closed the connection.
    ///
    /// Waits for as long as it takes: this is how a stream is read, and a
    /// stream is quiet for as long as the desktop is.
    ///
    /// # Errors
    ///
    /// Returns a failure on the socket, and on a line the daemon sent that is
    /// not a response. The second is reported rather than skipped: a client
    /// that quietly ignored what it could not read would turn a protocol
    /// mismatch into a stream that is merely short.
    pub async fn answer(&mut self) -> Result<Option<Response>> {
        let Some(line) = self
            .lines
            .next_line()
            .await
            .context("daemon stopped mid-answer")?
        else {
            return Ok(None);
        };

        serde_json::from_str(&line)
            .map(Some)
            .with_context(|| format!("daemon sent a line this client cannot read: {line}"))
    }

    /// The first answer, failing when the daemon closed instead of answering
    /// and when it gave none by the deadline.
    ///
    /// # Errors
    ///
    /// As [`Daemon::answer`]. Returns that the daemon closed the connection
    /// where it ended with the request unanswered: at its end where the
    /// daemon had read the request, and at a reset, said with it, where the
    /// daemon closed with the request unread or with the connection still in
    /// the socket's backlog. Returns that the daemon did not answer where no
    /// answer came within [`DEADLINE`] of the start of the connection.
    pub async fn expect_answer(&mut self) -> Result<Response> {
        let answered = timeout_at(self.answer_by, self.answer())
            .await
            .map_err(|_elapsed| did_not_answer(&self.socket))?;

        match answered {
            Ok(Some(answer)) => Ok(answer),
            Ok(None) => bail!(CLOSED_WITHOUT_ANSWERING),
            Err(failure) => match failure.downcast_ref::<io::Error>() {
                Some(reset) if reset.kind() == io::ErrorKind::ConnectionReset => {
                    bail!("{CLOSED_WITHOUT_ANSWERING}: {reset}")
                }
                _ => Err(failure),
            },
        }
    }
}

/// What is said of a daemon that closed the connection before its first
/// answer.
const CLOSED_WITHOUT_ANSWERING: &str = "daemon closed connection without answering";

/// What is said of a daemon that gave no answer by the deadline.
fn did_not_answer(socket: &Path) -> anyhow::Error {
    anyhow!(
        "daemon at {} did not answer within {}",
        socket.display(),
        format_duration(DEADLINE)
    )
}
