//! Per-connection state machine.
//!
//! Two independent byte streams run in parallel for each connected client:
//!
//! - **client → bus**: starts in [`Phase::Auth`], transitions to
//!   [`Phase::Messages`] once `BEGIN\r\n` is seen. During auth we forward
//!   every byte as it arrives (xdg-dbus-proxy does the same — the bus needs
//!   each `AUTH`/`NEGOTIATE_UNIX_FD` line to respond in real time). After
//!   auth we buffer until a complete DBus message is available, parse its
//!   header, forward the whole message, and expose the header to callers so
//!   rule engines (Task #5) can match on it.
//!
//! - **bus → client**: unchanged pass-through for Phase 1. The bus never
//!   sees anything we synthesize ourselves, so there is no need to parse its
//!   side yet. Task #2/Phase-2 work on local-name-ownership will revisit
//!   this direction to intercept `Hello` / `RequestName` replies.
//!
//! The type exposes owned byte buffers in its output rather than borrowing
//! internal state; this keeps the I/O layer simple (it can interleave
//! `write_all()` with further reads without re-entering the state machine).

use crate::dbus::{
    message::{self, Header, MessageError},
    sasl::{self, SaslError, MAX_AUTH_BUFFER},
};

use std::{error, fmt};

/// What portion of the client → bus stream we're currently parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Auth,
    Messages,
}

/// Everything observed while feeding one chunk of bytes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// Bytes to write to the opposite peer, in order.
    pub forward: Vec<u8>,
    /// Complete DBus messages observed in this feed, in order. Each entry
    /// points at a byte range inside [`Output::forward`] so a rule engine
    /// can rewrite the body in place before the I/O layer forwards it.
    /// Only populated on the client → bus direction in Phase 1.
    pub messages: Vec<ObservedMessage>,
}

/// A single complete DBus message observed in a feed, pointing at its byte
/// range inside the parent [`Output::forward`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedMessage {
    pub header: Header,
    /// Byte offset within [`Output::forward`] where this message starts.
    pub offset: usize,
    /// Total length of the message in bytes (fixed prefix + fields + padding + body).
    pub length: usize,
}

impl ObservedMessage {
    pub fn range(&self) -> std::ops::Range<usize> {
        self.offset..self.offset + self.length
    }
}

/// Reasons the broker state machine cannot continue; every one terminates
/// the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    Sasl(SaslError),
    Message(MessageError),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sasl(e) => write!(f, "SASL error: {e}"),
            Self::Message(e) => write!(f, "message error: {e}"),
        }
    }
}

impl error::Error for BrokerError {}

impl From<SaslError> for BrokerError {
    fn from(e: SaslError) -> Self {
        Self::Sasl(e)
    }
}

impl From<MessageError> for BrokerError {
    fn from(e: MessageError) -> Self {
        Self::Message(e)
    }
}

/// Per-connection pass-through state machine. Cheap to create — allocate one
/// per accepted client, drop when the connection closes.
#[derive(Debug)]
pub struct ConnectionState {
    client_phase: Phase,
    /// Rolling accumulator used only to feed [`sasl::find_begin_end`]. Reset
    /// and shrunk once auth completes.
    auth_accumulator: Vec<u8>,
    /// Incomplete DBus message bytes waiting for more data. Only used after
    /// auth completes.
    msg_buf: Vec<u8>,
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionState {
    pub fn new() -> Self {
        Self {
            client_phase: Phase::Auth,
            auth_accumulator: Vec::new(),
            msg_buf: Vec::new(),
        }
    }

    /// Feed bytes received from the client socket. Returns bytes to write to
    /// the bus plus any full messages seen in this feed.
    pub fn client_feed(&mut self, chunk: &[u8]) -> Result<Output, BrokerError> {
        let mut out = Output::default();
        let mut consumed = 0usize;

        if self.client_phase == Phase::Auth {
            let original_auth_len = self.auth_accumulator.len();
            self.auth_accumulator.extend_from_slice(chunk);

            match sasl::find_begin_end(&self.auth_accumulator)? {
                None => {
                    // Still pre-BEGIN. Forward every byte so the bus can
                    // respond in real time.
                    out.forward.extend_from_slice(chunk);
                    return Ok(out);
                }
                Some(end) => {
                    // BEGIN\r\n landed in this chunk (or exactly at the end
                    // of the previous one). Split: up to `end` in the
                    // accumulator is auth; the remainder is the first
                    // bytes of the DBus message stream.
                    let auth_bytes_in_chunk = end.saturating_sub(original_auth_len);
                    out.forward.extend_from_slice(&chunk[..auth_bytes_in_chunk]);
                    consumed = auth_bytes_in_chunk;

                    // Drop the accumulator — it isn't needed anymore and
                    // MAX_AUTH_BUFFER is generous enough that freeing it is
                    // worth the shrink.
                    self.auth_accumulator = Vec::new();
                    self.client_phase = Phase::Messages;
                }
            }
        }

        // Message phase.
        if consumed < chunk.len() {
            self.msg_buf.extend_from_slice(&chunk[consumed..]);
        }

        while !self.msg_buf.is_empty() {
            let total = match message::bytes_needed(&self.msg_buf)? {
                None => break, // need more bytes for the fixed prefix
                Some(n) => n,
            };
            if self.msg_buf.len() < total {
                break; // waiting for the rest of the body
            }
            let header = message::parse_header(&self.msg_buf[..total])?;
            let offset = out.forward.len();
            out.forward.extend_from_slice(&self.msg_buf[..total]);
            out.messages.push(ObservedMessage {
                header,
                offset,
                length: total,
            });
            self.msg_buf.drain(..total);
        }

        Ok(out)
    }

    /// Feed bytes received from the bus socket. Phase 1 is raw pass-through;
    /// we never synthesize replies and never look at bus traffic. Returning
    /// a structured [`Output`] keeps the surface symmetric with `client_feed`
    /// so Phase 2 can start inspecting bus bytes without API churn.
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

    /// Upper bound on the auth accumulator; exposed for diagnostics and
    /// symmetric with [`sasl::MAX_AUTH_BUFFER`].
    pub const MAX_AUTH_BUFFER: usize = MAX_AUTH_BUFFER;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::message::{Endian, MessageType};

    // ---------------------------------------------------------------------
    // Fixture: hand-rolled DBus method_call encoder. Only exercises the
    // layout the parser already tests; intentionally small.
    // ---------------------------------------------------------------------

    fn pad_to(out: &mut Vec<u8>, bound: usize) {
        while !out.len().is_multiple_of(bound) {
            out.push(0);
        }
    }

    fn push_string_field(out: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
        pad_to(out, 8);
        out.push(code);
        // variant signature
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
        // string value
        pad_to(out, 4);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }

    fn build_hello(serial: u32) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
        push_string_field(&mut fields, 2, "s", "org.freedesktop.DBus");
        push_string_field(&mut fields, 3, "s", "Hello");
        push_string_field(&mut fields, 6, "s", "org.freedesktop.DBus");

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&0u32.to_le_bytes()); // body_len
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg
    }

    fn handshake() -> Vec<u8> {
        b"\0AUTH EXTERNAL 30\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n".to_vec()
    }

    // ---------------------------------------------------------------------
    // Single-chunk feeds
    // ---------------------------------------------------------------------

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
        let msg = &out.messages[0];
        assert_eq!(msg.header.member.as_deref(), Some("Hello"));
        assert_eq!(msg.header.msg_type, MessageType::MethodCall);
        assert_eq!(msg.header.endian, Endian::Little);
        assert_eq!(msg.length, hello.len());
        // The message starts right after the handshake bytes in `forward`.
        assert_eq!(msg.offset, chunk.len() - hello.len());
        assert_eq!(&out.forward[msg.range()], hello.as_slice());
    }

    // ---------------------------------------------------------------------
    // Fragmented feeds
    // ---------------------------------------------------------------------

    #[test]
    fn handshake_split_across_chunks_locates_begin() {
        let mut st = ConnectionState::new();
        let handshake = handshake();

        // Byte-by-byte feed exercises every split point.
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
        // First: complete handshake.
        let out = st.client_feed(&handshake()).unwrap();
        assert!(out.forward == handshake());
        assert!(st.is_authed());

        let hello = build_hello(1);
        // Split Hello across the fixed-prefix / fields boundary.
        let (a, b) = hello.split_at(10);
        let out1 = st.client_feed(a).unwrap();
        assert!(out1.forward.is_empty());
        assert!(out1.messages.is_empty());

        let out2 = st.client_feed(b).unwrap();
        assert_eq!(out2.forward, hello);
        assert_eq!(out2.messages.len(), 1);
        assert_eq!(out2.messages[0].header.member.as_deref(), Some("Hello"));
        assert_eq!(out2.messages[0].offset, 0);
        assert_eq!(out2.messages[0].length, hello.len());
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
        assert_eq!(out.messages[0].header.serial, 1);
        assert_eq!(out.messages[0].offset, 0);
        assert_eq!(out.messages[1].header.serial, 2);
        assert_eq!(out.messages[1].offset, out.messages[0].length);
        assert_eq!(
            out.messages[0].length + out.messages[1].length,
            combined.len()
        );
    }

    // ---------------------------------------------------------------------
    // Error paths
    // ---------------------------------------------------------------------

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
        // Garbage: unknown endian marker.
        let mut bad = vec![b'X', 1, 0, 1];
        bad.extend_from_slice(&[0u8; 12]);
        let err = st.client_feed(&bad).unwrap_err();
        assert!(matches!(
            err,
            BrokerError::Message(MessageError::InvalidEndian(b'X'))
        ));
    }

    // ---------------------------------------------------------------------
    // Bus feed (raw pass-through)
    // ---------------------------------------------------------------------

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
        // Bus bytes arrive before client auth finishes — still just a copy.
        let out = st.bus_feed(b"DATA\r\n").unwrap();
        assert_eq!(out.forward, b"DATA\r\n");
        assert!(!st.is_authed());
    }
}
