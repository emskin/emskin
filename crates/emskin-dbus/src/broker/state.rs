//! Per-connection byte-stream state machine.
//!
//! Two independent streams run in parallel for each connected client:
//!
//! - **client → bus**: starts in [`Phase::Auth`], transitions to
//!   [`Phase::Messages`] once `BEGIN\r\n` is seen. During auth we forward
//!   every byte as it arrives (xdg-dbus-proxy does the same — the bus
//!   needs each `AUTH` / `NEGOTIATE_UNIX_FD` line to respond in real
//!   time). After auth we buffer until a complete DBus frame is
//!   available, forward it, and report its byte range so callers can
//!   parse it via [`crate::dbus::frame::Frame::parse`].
//!
//! - **bus → client**: raw pass-through. The bus never sees anything we
//!   synthesize ourselves, so this side has no parsing.
//!
//! State machine output uses owned byte buffers; the I/O layer can
//! interleave `write_all()` with further reads without re-entering.

use crate::dbus::{
    frame::{self, Frame},
    sasl::{self, SaslError, MAX_AUTH_BUFFER},
};

use std::ops::Range;
use std::{error, fmt};

/// What portion of the client → bus stream we're currently parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Auth,
    Messages,
}

/// Everything observed while feeding one chunk of bytes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Bytes to write to the opposite peer, in order.
    pub forward: Vec<u8>,
    /// Byte ranges within [`Output::forward`] containing one complete
    /// frame each. Callers parse via [`Frame::parse`] on the slice.
    pub messages: Vec<MessageSpan>,
}

/// Location of one complete DBus frame within [`Output::forward`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSpan {
    pub offset: usize,
    pub length: usize,
}

impl MessageSpan {
    pub fn range(&self) -> Range<usize> {
        self.offset..self.offset + self.length
    }
}

/// Reasons the broker state machine cannot continue; every one terminates
/// the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    Sasl(SaslError),
    Frame(frame::Error),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sasl(e) => write!(f, "SASL error: {e}"),
            Self::Frame(e) => write!(f, "frame error: {e}"),
        }
    }
}

impl error::Error for BrokerError {}

impl From<SaslError> for BrokerError {
    fn from(e: SaslError) -> Self {
        Self::Sasl(e)
    }
}

impl From<frame::Error> for BrokerError {
    fn from(e: frame::Error) -> Self {
        Self::Frame(e)
    }
}

/// Per-connection pass-through state machine.
#[derive(Debug, Default)]
pub struct ConnectionState {
    client_phase: Phase,
    /// Rolling accumulator used only to feed [`sasl::find_begin_end`].
    /// Reset and shrunk once auth completes.
    auth_accumulator: Vec<u8>,
    /// Incomplete DBus message bytes waiting for more data. Only used
    /// after auth completes.
    msg_buf: Vec<u8>,
}

impl ConnectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes received from the client socket. Returns bytes to
    /// write to the bus plus the byte ranges of any full frames seen.
    pub fn client_feed(&mut self, chunk: &[u8]) -> Result<Output, BrokerError> {
        let mut out = Output::default();
        let mut consumed = 0usize;

        if self.client_phase == Phase::Auth {
            let original_auth_len = self.auth_accumulator.len();
            self.auth_accumulator.extend_from_slice(chunk);

            match sasl::find_begin_end(&self.auth_accumulator)? {
                None => {
                    out.forward.extend_from_slice(chunk);
                    return Ok(out);
                }
                Some(end) => {
                    let auth_bytes_in_chunk = end.saturating_sub(original_auth_len);
                    out.forward.extend_from_slice(&chunk[..auth_bytes_in_chunk]);
                    consumed = auth_bytes_in_chunk;
                    self.auth_accumulator = Vec::new();
                    self.client_phase = Phase::Messages;
                }
            }
        }

        if consumed < chunk.len() {
            self.msg_buf.extend_from_slice(&chunk[consumed..]);
        }

        // Split the buffer into full frames. We don't parse fields here;
        // callers that need typed access call `Frame::parse` on the
        // returned span.
        while !self.msg_buf.is_empty() {
            let total = match Frame::bytes_needed(&self.msg_buf)? {
                None => break,
                Some(n) => n,
            };
            if self.msg_buf.len() < total {
                break;
            }
            let offset = out.forward.len();
            out.forward.extend_from_slice(&self.msg_buf[..total]);
            out.messages.push(MessageSpan {
                offset,
                length: total,
            });
            self.msg_buf.drain(..total);
        }

        Ok(out)
    }

    /// Feed bytes received from the bus socket — raw pass-through.
    pub fn bus_feed(&mut self, chunk: &[u8]) -> Result<Output, BrokerError> {
        Ok(Output {
            forward: chunk.to_vec(),
            messages: Vec::new(),
        })
    }

    /// True once `BEGIN\r\n` has crossed the client → bus stream.
    pub fn is_authed(&self) -> bool {
        matches!(self.client_phase, Phase::Messages)
    }

    /// Upper bound on the auth accumulator; symmetric with
    /// [`sasl::MAX_AUTH_BUFFER`].
    pub const MAX_AUTH_BUFFER: usize = MAX_AUTH_BUFFER;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::frame::{Frame, Kind};

    fn build_hello(serial: u32) -> Vec<u8> {
        let mut frame = Frame::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
        )
        .serial(serial)
        .destination("org.freedesktop.DBus")
        .build();
        frame.kind = Kind::MethodCall;
        frame.encode()
    }

    fn handshake() -> Vec<u8> {
        b"\0AUTH EXTERNAL 30\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n".to_vec()
    }

    #[test]
    fn handshake_only_feed_forwards_verbatim_and_stays_in_auth() {
        let mut st = ConnectionState::new();
        let chunk = b"\0AUTH EXTERNAL 30\r\n";
        let out = st.client_feed(chunk).unwrap();
        assert_eq!(out.forward, chunk);
        assert!(out.messages.is_empty());
        assert!(!st.is_authed());
    }

    #[test]
    fn full_handshake_transitions_to_message_phase() {
        let mut st = ConnectionState::new();
        let chunk = handshake();
        let out = st.client_feed(&chunk).unwrap();
        assert_eq!(out.forward, chunk);
        assert!(out.messages.is_empty());
        assert!(st.is_authed());
    }

    #[test]
    fn handshake_plus_hello_in_one_chunk() {
        let mut st = ConnectionState::new();
        let hello = build_hello(1);
        let mut chunk = handshake();
        chunk.extend_from_slice(&hello);

        let out = st.client_feed(&chunk).unwrap();
        assert_eq!(out.forward, chunk);
        assert_eq!(out.messages.len(), 1);
        let span = &out.messages[0];
        assert_eq!(span.length, hello.len());
        assert_eq!(span.offset, chunk.len() - hello.len());
        assert_eq!(&out.forward[span.range()], hello.as_slice());
        // Caller decodes typed access via `Frame::parse`:
        let parsed = Frame::parse(&out.forward[span.range()]).unwrap();
        assert_eq!(parsed.fields.member.as_deref(), Some("Hello"));
        assert_eq!(parsed.kind, Kind::MethodCall);
    }

    #[test]
    fn handshake_split_across_chunks_locates_begin() {
        let mut st = ConnectionState::new();
        let handshake = handshake();
        let mut forwarded = Vec::new();
        for byte in &handshake {
            let out = st.client_feed(std::slice::from_ref(byte)).unwrap();
            forwarded.extend_from_slice(&out.forward);
            assert!(out.messages.is_empty());
        }
        assert_eq!(forwarded, handshake);
        assert!(st.is_authed());
    }

    #[test]
    fn hello_split_mid_header_buffers_then_completes() {
        let mut st = ConnectionState::new();
        st.client_feed(&handshake()).unwrap();

        let hello = build_hello(1);
        let (a, b) = hello.split_at(10);
        let out1 = st.client_feed(a).unwrap();
        assert!(out1.forward.is_empty());
        assert!(out1.messages.is_empty());

        let out2 = st.client_feed(b).unwrap();
        assert_eq!(out2.forward, hello);
        assert_eq!(out2.messages.len(), 1);
        let parsed = Frame::parse(&out2.forward[out2.messages[0].range()]).unwrap();
        assert_eq!(parsed.fields.member.as_deref(), Some("Hello"));
    }

    #[test]
    fn hello_byte_by_byte_buffers_then_completes() {
        let mut st = ConnectionState::new();
        st.client_feed(&handshake()).unwrap();

        let hello = build_hello(7);
        let mut forwarded = Vec::new();
        let mut msgs_observed = 0;
        for byte in &hello {
            let out = st.client_feed(std::slice::from_ref(byte)).unwrap();
            forwarded.extend_from_slice(&out.forward);
            msgs_observed += out.messages.len();
        }
        assert_eq!(forwarded, hello);
        assert_eq!(msgs_observed, 1);
    }

    #[test]
    fn two_messages_in_single_feed() {
        let mut st = ConnectionState::new();
        st.client_feed(&handshake()).unwrap();

        let mut combined = build_hello(1);
        combined.extend_from_slice(&build_hello(2));
        let out = st.client_feed(&combined).unwrap();
        assert_eq!(out.forward, combined);
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0].offset, 0);
        assert_eq!(out.messages[1].offset, out.messages[0].length);
        assert_eq!(
            out.messages[0].length + out.messages[1].length,
            combined.len()
        );
        let p0 = Frame::parse(&out.forward[out.messages[0].range()]).unwrap();
        let p1 = Frame::parse(&out.forward[out.messages[1].range()]).unwrap();
        assert_eq!(p0.serial, 1);
        assert_eq!(p1.serial, 2);
    }

    #[test]
    fn missing_nul_prefix_errors() {
        let mut st = ConnectionState::new();
        let err = st.client_feed(b"AUTH EXTERNAL\r\n").unwrap_err();
        assert_eq!(err, BrokerError::Sasl(SaslError::MissingNulPrefix));
    }

    #[test]
    fn malformed_message_after_auth_errors() {
        let mut st = ConnectionState::new();
        st.client_feed(&handshake()).unwrap();
        let mut bad = vec![b'X', 1, 0, 1];
        bad.extend_from_slice(&[0u8; 12]);
        let err = st.client_feed(&bad).unwrap_err();
        assert!(matches!(
            err,
            BrokerError::Frame(frame::Error::InvalidEndian(b'X'))
        ));
    }

    #[test]
    fn bus_feed_forwards_bytes_verbatim() {
        let mut st = ConnectionState::new();
        let chunk = b"anything at all: OK 0123456789abcdef0123\r\n";
        let out = st.bus_feed(chunk).unwrap();
        assert_eq!(out.forward, chunk);
        assert!(out.messages.is_empty());
    }

    #[test]
    fn bus_feed_is_independent_of_client_phase() {
        let mut st = ConnectionState::new();
        let out = st.bus_feed(b"DATA\r\n").unwrap();
        assert_eq!(out.forward, b"DATA\r\n");
        assert!(!st.is_authed());
    }
}
