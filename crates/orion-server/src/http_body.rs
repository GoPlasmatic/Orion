//! Reading an outbound HTTP response body under a byte cap — while streaming.
//!
//! Every egress path in Orion caps the response it will accept: `http_call`
//! and Elasticsearch by the connector's `max_response_size`, JWKS at 256 KiB,
//! an OAuth2 token response at 64 KiB. Three of the four enforced that cap by
//! calling `Response::bytes()` and *then* measuring what came back, which
//! enforces nothing a peer has to respect: `bytes()` reads to end of body
//! first, so a chunked response — or one with a missing or dishonest
//! `Content-Length` — is fully in memory by the time the check runs. The cap
//! rejected the result after paying its whole cost, concurrently, on a path
//! reachable by any workflow naming an attacker-influenced URL.
//!
//! The correct shape lived in `engine::functions::http_common` and nowhere
//! else, so each new caller reimplemented the check and one of them got it
//! wrong. It lives here instead, as a leaf: `engine`, `connector` and `jwt`
//! all reach egress and none of them may reach each other, the same argument
//! that put HMAC and the base64 table in [`crate::crypto`].
//!
//! What stays with the caller is interpretation — which status codes are
//! errors, whether the body is JSON, what to say about it. This module
//! only answers "give me at most N bytes, and stop pulling if the peer sends
//! more".

use std::fmt;

/// Why a bounded read did not produce a body.
#[derive(Debug)]
pub enum ReadError {
    /// The response is larger than the caller allows.
    ///
    /// `declared` is `Some` when the rejection came from the peer's
    /// `Content-Length` header, before any body was read; `None` when the
    /// bytes themselves crossed the limit mid-stream. The distinction is worth
    /// keeping: the first is a peer that announced it would overrun, the
    /// second is one that did it without saying so, and only the second tells
    /// you the header was absent or a lie.
    TooLarge { limit: usize, declared: Option<u64> },
    /// The transport failed before the body ended.
    Transport(reqwest::Error),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge {
                limit,
                declared: Some(declared),
            } => write!(
                f,
                "declared Content-Length {declared} exceeds the {limit} byte limit"
            ),
            Self::TooLarge {
                limit,
                declared: None,
            } => write!(f, "body exceeds the {limit} byte limit"),
            Self::Transport(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            Self::TooLarge { .. } => None,
        }
    }
}

impl ReadError {
    /// Whether this was the size cap rather than a transport failure. Lets a
    /// caller classify an oversized peer response as its own fault rather than
    /// a retryable network blip.
    pub fn is_too_large(&self) -> bool {
        matches!(self, Self::TooLarge { .. })
    }
}

/// Reject a response whose `Content-Length` already exceeds `limit`, before
/// reading a byte of it.
///
/// A hint, not a guarantee — the header is the peer's claim and may be absent
/// or wrong, which is exactly why [`read_bounded`] enforces the limit again
/// while streaming. Worth checking anyway: an honest oversized response is
/// refused without opening the body at all.
///
/// Exposed separately because a caller may need it to fire *before* it
/// branches on the status code.
pub fn check_declared_length(response: &reqwest::Response, limit: usize) -> Result<(), ReadError> {
    match response.content_length() {
        Some(declared) if declared > limit as u64 => Err(ReadError::TooLarge {
            limit,
            declared: Some(declared),
        }),
        _ => Ok(()),
    }
}

/// Read the whole body, refusing it as soon as it is known to exceed `limit`.
///
/// The limit is enforced *before* each chunk is appended, so the peak memory
/// this can be made to hold is one chunk beyond the limit rather than the
/// whole body: a peer streaming 8 MiB into a 1 KiB cap is hung up on after
/// roughly a kilobyte, not read to the end and then rejected.
///
/// The buffer is grown rather than pre-allocated from `Content-Length`:
/// reserving what the peer *says* it will send hands an unauthenticated party
/// an allocation of up to `limit` bytes per in-flight request for a body it
/// never has to send.
pub async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReadError> {
    check_declared_length(&response, limit)?;

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Transport)? {
        if body.len() + chunk.len() > limit {
            return Err(ReadError::TooLarge {
                limit,
                declared: None,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// As much of a body as fits in `limit`, for putting in an error message.
///
/// Never fails and never rejects: the caller has already decided this response
/// is a failure and only wants something quotable. Bytes past the limit are
/// dropped as they arrive rather than buffered, and a transport error simply
/// ends the preview — a truncated explanation of a failure is still better
/// than none, and the status code the caller already has carries the failure
/// itself.
pub async fn read_preview(mut response: reqwest::Response, limit: usize) -> Preview {
    let mut bytes = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = response.chunk().await.ok().flatten() {
        let room = limit.saturating_sub(bytes.len());
        let take = chunk.len().min(room);
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            truncated = true;
            break;
        }
    }
    Preview { bytes, truncated }
}

/// The first bytes of a body, and whether there were more.
pub struct Preview {
    pub bytes: Vec<u8>,
    /// `true` when the body was longer than the preview limit. Worth saying
    /// out loud in an error message: without it a truncated body reads as the
    /// whole of a short one to whoever is trying to explain a failure.
    pub truncated: bool,
}

impl Preview {
    /// The preview as text, lossy on invalid UTF-8, with an explicit ellipsis
    /// when it was cut short.
    pub fn to_message(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        if self.truncated {
            format!("{text}… (truncated)")
        } else {
            text.into_owned()
        }
    }
}

/// Test-only: a loopback server that answers with a chunked body and streams
/// `chunk_bytes` at a time until the client hangs up or `max_chunks` are
/// written.
///
/// Lives here rather than inside one `mod tests` because the property it
/// measures belongs to every caller, not to this module: each egress path
/// that caps a response has a test that floods it and asserts the peer *did
/// not get to send the whole body*. Written against a raw socket because the
/// point is to answer with chunked transfer encoding and no `Content-Length`
/// — the shape that makes a read-then-measure cap ineffective — which a
/// server framework will not let you do wrongly enough.
///
/// Returns the URL and a handle yielding how many body bytes the server
/// actually managed to write.
#[cfg(test)]
pub(crate) async fn flood_server(
    chunk_bytes: usize,
    max_chunks: usize,
) -> (String, tokio::task::JoinHandle<usize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener");
    let url = format!("http://{}/", listener.local_addr().expect("test addr"));
    let chunk = vec![b'x'; chunk_bytes];
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("test accept");
        // Enough of the request to let the client finish sending it; nothing
        // here parses it.
        let mut discard = [0u8; 4096];
        let _ = socket.read(&mut discard).await;
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        if socket.write_all(head.as_bytes()).await.is_err() {
            return 0;
        }
        let mut written = 0usize;
        for _ in 0..max_chunks {
            // Chunked framing: size line, payload, CRLF.
            let framed = format!("{:x}\r\n", chunk.len());
            if socket.write_all(framed.as_bytes()).await.is_err()
                || socket.write_all(&chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                break;
            }
            written += chunk.len();
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
        written
    });
    (url, handle)
}

/// Test-only: assert a flooded peer did not get to send its whole body.
///
/// A read-then-measure cap lets all of it through and *then* reports a size
/// error, which is why every caller's test asserts on this rather than on the
/// error alone — the error is identical either way.
///
/// The bound is "not all of it", which is exactly the discriminator and
/// nothing more: `Response::bytes()` consumes the body to its end, so every
/// write succeeds and `written == attempted` on the nose. A streaming reader
/// hangs up, the next write fails, and the peer stops short. Anything tighter
/// is a statement about how far the writer runs ahead of the reader before the
/// hang-up registers — which is a timing assumption, not a property of the
/// code. A `written < attempted / 2` version of this passed everywhere except
/// under `cargo llvm-cov`, where instrumentation slows the reader and the peer
/// got 5,570,560 of 8,388,608 bytes out before it noticed. The percentage is
/// in the message because it is worth *seeing*; it is not worth asserting.
#[cfg(test)]
pub(crate) fn assert_stopped_early(written: usize, attempted: usize, what: &str) {
    assert!(
        written < attempted,
        "{what}: the peer got to write all {attempted} bytes — the cap must be \
         enforced while streaming, not after the body is buffered"
    );
    let leaked = (written as f64 / attempted as f64) * 100.0;
    // Not a failure, but worth knowing: a reader that lets most of the body
    // through is still bounded, just not promptly.
    if written * 2 >= attempted {
        eprintln!(
            "note: {what} stopped the peer at {written} of {attempted} bytes \
             ({leaked:.0}%) — bounded, but the reader is running well behind \
             the writer"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 128; // 8 MiB if a reader lets it all through

    /// The property the whole module exists for: a chunked body far larger
    /// than the cap is refused *while streaming*, so the peer never gets to
    /// hand over the whole thing.
    ///
    /// The load-bearing assertion is on what the **server** managed to write,
    /// not on the error — `Response::bytes()` produces the same `TooLarge`
    /// here, after reading all 8 MiB into memory, which is the bug.
    #[tokio::test]
    async fn a_chunked_body_is_refused_without_reading_it_all() {
        let (url, server) = flood_server(CHUNK, CHUNKS).await;

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let err = read_bounded(response, 1024).await.expect_err("must refuse");

        assert!(
            matches!(err, ReadError::TooLarge { declared: None, .. }),
            "a chunked response declares no length, so the refusal must come \
             from the bytes themselves: {err}"
        );
        assert_stopped_early(
            server.await.expect("test server"),
            CHUNK * CHUNKS,
            "read_bounded",
        );
    }

    /// An honest oversized `Content-Length` is refused before the body is
    /// touched at all — the cheap half of the defence, which is all three of
    /// the migrated callers used to have.
    #[tokio::test]
    async fn a_declared_oversize_body_is_refused_unread() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let url = format!("http://{}/", listener.local_addr().expect("test addr"));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("test accept");
            let mut discard = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut discard).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n")
                .await;
            // Deliberately never sends the body: nothing may be waiting on it.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_bounded(response, 1024),
        )
        .await
        .expect("must not wait on a body it has already refused")
        .expect_err("must refuse");

        assert!(matches!(
            err,
            ReadError::TooLarge {
                limit: 1024,
                declared: Some(5000)
            }
        ));
        server.abort();
    }

    /// A body that fits is returned whole — the cap is a ceiling, not a budget
    /// the response has to stay under by some margin.
    #[tokio::test]
    async fn a_body_exactly_at_the_limit_is_accepted() {
        let (url, server) = flood_server(512, 2).await;

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let body = read_bounded(response, 1024).await.expect("accepted");

        assert_eq!(body.len(), 1024);
        assert!(body.iter().all(|b| *b == b'x'));
        let _ = server.await;
    }

    /// The preview keeps what fits, says it was cut, and does not pull the
    /// rest of the body to find out.
    #[tokio::test]
    async fn a_preview_truncates_and_says_so() {
        let (url, server) = flood_server(CHUNK, CHUNKS).await;

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let preview = read_preview(response, 16).await;

        assert_eq!(preview.bytes.len(), 16);
        assert!(preview.truncated);
        assert_eq!(
            preview.to_message(),
            format!("{}… (truncated)", "x".repeat(16))
        );
        assert_stopped_early(
            server.await.expect("test server"),
            CHUNK * CHUNKS,
            "read_preview",
        );
    }

    /// An error body that is not valid UTF-8 still produces a message.
    ///
    /// The preview exists to explain a failure, so bytes that do not decode
    /// must not become a second failure on top of the first — a peer that
    /// answers 500 with a binary payload would otherwise cost the caller the
    /// only diagnostic it had.
    #[tokio::test]
    async fn a_preview_of_non_utf8_bytes_is_lossy_rather_than_lost() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener");
        let url = format!("http://{}/", listener.local_addr().expect("test addr"));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("test accept");
            let mut discard = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut discard).await;
            let _ = socket
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\n")
                .await;
            // A lone 0xFF is not valid UTF-8 in any position.
            let _ = socket.write_all(&[0xff, b'b', 0xfe, b'd']).await;
        });

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let preview = read_preview(response, 512).await;

        assert!(!preview.truncated);
        assert_eq!(
            preview.to_message(),
            "\u{fffd}b\u{fffd}d",
            "invalid bytes must become replacement characters, not an error"
        );
        let _ = server.await;
    }

    /// A short error body is quoted verbatim, with no ellipsis to suggest
    /// there was more.
    #[tokio::test]
    async fn a_short_preview_is_not_marked_truncated() {
        let (url, server) = flood_server(4, 1).await;

        let response = reqwest::Client::new().get(url).send().await.expect("head");
        let preview = read_preview(response, 512).await;

        assert!(!preview.truncated);
        assert_eq!(preview.to_message(), "xxxx");
        let _ = server.await;
    }
}
