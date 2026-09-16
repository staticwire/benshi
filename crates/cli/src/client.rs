//! Speaking to a running daemon over its local socket.
//!
//! One request per line, one answer per line, exactly as the daemon writes
//! them. Nothing here decides anything or renders anything: it carries
//! messages, and `commands` does the rest.

use std::path::Path;

use anyhow::{Context, Result};
use benshi_daemon::protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::net::UnixStream;

/// A connection to a running daemon.
#[derive(Debug)]
pub struct Daemon {
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
    /// are fixed differently.
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("no daemon at {}", socket.display()))?;
        let (reader, writer) = tokio::io::split(stream);

        Ok(Self {
            writer,
            lines: BufReader::new(reader).lines(),
        })
    }

    /// Send one request.
    ///
    /// # Errors
    ///
    /// Returns whatever the socket did.
    pub async fn ask(&mut self, request: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(request).context("the request could not be written")?;
        line.push(b'\n');

        self.writer
            .write_all(&line)
            .await
            .context("the request could not be sent")?;
        self.writer
            .flush()
            .await
            .context("the request could not be sent")
    }

    /// The next answer, or nothing once the daemon has closed the connection.
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
            .context("the daemon stopped mid-answer")?
        else {
            return Ok(None);
        };

        serde_json::from_str(&line)
            .map(Some)
            .with_context(|| format!("the daemon sent a line this client cannot read: {line}"))
    }

    /// The next answer, failing when the daemon closed instead of answering.
    ///
    /// # Errors
    ///
    /// As [`Daemon::answer`], and when the connection ended with the request
    /// unanswered.
    pub async fn expect_answer(&mut self) -> Result<Response> {
        self.answer()
            .await?
            .context("the daemon closed the connection without answering")
    }
}
