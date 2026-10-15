//! How a request reaches AniList and its answer comes back.
//!
//! One method, a request to an answer, so that a test can stand in for the
//! service with answers it scripts or recorded. [`Http`] is the one that
//! speaks to the service.

use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use serde_json::{Value, json};

use crate::Error;

/// The address of AniList's GraphQL API.
pub const ANILIST: &str = "https://graphql.anilist.co";

/// One GraphQL request: a query and its variables.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The query.
    pub query: String,
    /// Its variables, a JSON object.
    pub variables: Value,
}

impl Request {
    /// The body the request is posted as.
    #[must_use]
    pub fn body(&self) -> String {
        json!({ "query": self.query, "variables": self.variables }).to_string()
    }
}

/// What the service answered, before it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// How long the service asked to be left alone, from `Retry-After`.
    pub retry_after: Option<Duration>,
    /// The body.
    pub body: String,
}

/// A way to send a request and take its answer.
pub trait Transport {
    /// Send one request.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] where the service cannot be reached, and
    /// [`Error::Unreadable`] where the body of its answer is not text.
    fn send(&self, request: &Request) -> impl Future<Output = Result<Answer, Error>> + Send;
}

/// Requests over HTTPS, with a token.
#[derive(Debug)]
pub struct Http {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl Http {
    /// A transport to `url` that sends `token` with every request.
    ///
    /// `reqwest` builds its TLS configuration from the process's default
    /// provider of cryptography, and panics where none is installed. It is
    /// installed here, `ring` where nothing has installed one, so that every
    /// `Http`, in a test as much as in the daemon, works without a step of its
    /// own. Installing fails only where another was installed in the
    /// meantime, which serves as well.
    ///
    /// # Errors
    ///
    /// Where the HTTP client cannot be built.
    pub fn new(url: &str, token: &str) -> Result<Self, reqwest::Error> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok();
        }

        Ok(Self {
            client: reqwest::Client::builder().build()?,
            url: url.to_owned(),
            token: token.to_owned(),
        })
    }
}

impl Transport for Http {
    async fn send(&self, request: &Request) -> Result<Answer, Error> {
        let unreachable = |cause: reqwest::Error| Error::Unreachable(cause.into());
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(request.body())
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(seconds);
        let body = response.bytes().await.map_err(unreachable)?;
        let body =
            String::from_utf8(body.to_vec()).map_err(|cause| Error::Unreadable(cause.into()))?;

        Ok(Answer {
            status,
            retry_after,
            body,
        })
    }
}

/// The seconds of a `Retry-After` header, and none for any other form.
///
/// The header may also hold a date. AniList's documentation gives it in
/// seconds alone.
fn seconds(value: &str) -> Option<Duration> {
    value.parse().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{Answer, Http, Request, Transport, seconds};
    use crate::Error;

    const TOKEN: &str = "a-token-for-tests";

    fn a_request() -> Request {
        Request {
            query: "query { Viewer { id name } }".to_owned(),
            variables: json!({ "chunk": 1 }),
        }
    }

    /// A listener on a free port between 20000 and 40000.
    async fn a_listener() -> (TcpListener, u16) {
        let start = u16::try_from(std::process::id() % 20_000).expect("below 20000");
        for offset in 0..20_000 {
            let port = 20_000 + (start + offset) % 20_000;
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                return (listener, port);
            }
        }
        panic!("no port between 20000 and 40000 is free");
    }

    /// Take one request off the listener, answer it with `answer`, and hand
    /// back the request as it was sent.
    async fn answer_one(listener: TcpListener, answer: Vec<u8>) -> String {
        let (mut socket, _) = listener.accept().await.expect("the client connects");
        let mut received = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let read = socket.read(&mut chunk).await.expect("the request is read");
            assert_ne!(read, 0, "the client went before its request was whole");
            received.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&received);
            if let Some(end) = text.find("\r\n\r\n") {
                let length: usize = text[..end]
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map_or(0, |length| length.parse().expect("a length"));
                if received.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket
            .write_all(&answer)
            .await
            .expect("the answer is written");
        socket.shutdown().await.expect("the answer ends");

        String::from_utf8(received).expect("the request is text")
    }

    /// What the server was sent, once the client has had its answer.
    ///
    /// Bounded, so that a client that never connects fails the test rather
    /// than leaving the server waiting on it for ever.
    async fn received(server: tokio::task::JoinHandle<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("the client reached the server within five seconds")
            .expect("the server ran")
    }

    #[tokio::test]
    async fn a_request_is_posted_as_json_with_the_token() {
        let (listener, port) = a_listener().await;
        let server = tokio::spawn(answer_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ));
        let http = Http::new(&format!("http://127.0.0.1:{port}/"), TOKEN).expect("a client");

        http.send(&a_request())
            .await
            .expect("the request is answered");

        let sent = received(server).await;
        let (head, body) = sent.split_once("\r\n\r\n").expect("a head and a body");
        let head = head.to_ascii_lowercase();
        assert!(head.starts_with("post / http/1.1\r\n"), "{head}");
        assert!(
            head.contains(&format!("authorization: bearer {TOKEN}")),
            "{head}"
        );
        assert!(head.contains("content-type: application/json"), "{head}");
        assert!(head.contains("accept: application/json"), "{head}");
        assert_eq!(
            serde_json::from_str::<Value>(body).expect("the body is JSON"),
            json!({ "query": "query { Viewer { id name } }", "variables": { "chunk": 1 } })
        );
    }

    #[tokio::test]
    async fn an_answer_carries_its_status_its_wait_and_its_body() {
        let (listener, port) = a_listener().await;
        let server = tokio::spawn(answer_one(
            listener,
            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 14\r\n\r\n{\"errors\":[]}\n"
                .to_vec(),
        ));
        let http = Http::new(&format!("http://127.0.0.1:{port}/"), TOKEN).expect("a client");

        let answer = http
            .send(&a_request())
            .await
            .expect("the request is answered");

        received(server).await;
        assert_eq!(
            answer,
            Answer {
                status: 429,
                retry_after: Some(Duration::from_secs(30)),
                body: "{\"errors\":[]}\n".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn an_answer_that_is_not_text_is_unreadable() {
        let (listener, port) = a_listener().await;
        let server = tokio::spawn(answer_one(
            listener,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n\xff\xfe".to_vec(),
        ));
        let http = Http::new(&format!("http://127.0.0.1:{port}/"), TOKEN).expect("a client");

        let answer = http.send(&a_request()).await;

        assert!(
            matches!(answer, Err(Error::Unreadable(_))),
            "got {answer:?}"
        );
        received(server).await;
    }

    /// A port between 20000 and 40000 held by a socket that is bound and
    /// never listens, so that a connection to it is refused and no other
    /// test can take it in the meantime.
    fn a_port_nobody_listens_on() -> (tokio::net::TcpSocket, u16) {
        let start = u16::try_from(std::process::id() % 20_000).expect("below 20000");
        for offset in (0..20_000).rev() {
            let port = 20_000 + (start + offset) % 20_000;
            let socket = tokio::net::TcpSocket::new_v4().expect("a socket");
            if socket.bind(([127, 0, 0, 1], port).into()).is_ok() {
                return (socket, port);
            }
        }
        panic!("no port between 20000 and 40000 is free");
    }

    #[tokio::test]
    async fn a_service_nobody_listens_for_cannot_be_reached() {
        let (_held, port) = a_port_nobody_listens_on();
        let http = Http::new(&format!("http://127.0.0.1:{port}/"), TOKEN).expect("a client");

        let answer = http.send(&a_request()).await;

        assert!(
            matches!(answer, Err(Error::Unreachable(_))),
            "got {answer:?}"
        );
    }

    #[test]
    fn a_wait_is_read_in_whole_seconds_and_no_other_form() {
        assert_eq!(seconds("30"), Some(Duration::from_secs(30)));
        assert_eq!(seconds("0"), Some(Duration::ZERO));
        assert_eq!(seconds("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(seconds("-1"), None);
        assert_eq!(seconds(""), None);
    }
}
