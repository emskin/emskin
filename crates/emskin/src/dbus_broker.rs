//! In-process DBus broker — calloop-driven replacement for the
//! `emskin-dbus-proxy` subprocess.
//!
//! # Responsibilities
//!
//! - Bind a Unix socket inside `$XDG_RUNTIME_DIR/emskin-dbus-<pid>/bus.sock`
//!   that embedded apps dial via `DBUS_SESSION_BUS_ADDRESS`.
//! - For each accepted client, dial the real upstream session bus.
//! - Drive both halves of the pair via non-blocking reads + write buffers.
//! - On the `client → bus` direction, apply the cursor-coord rewrite from
//!   [`emskin_dbus::broker::apply_cursor_rewrites`] using [`Self::offset`].
//!
//! # What this is **not** (yet)
//!
//! This module is wired up in a follow-up commit. Right now it only
//! provides the plumbing — the existing `DbusBridge::spawn_and_connect`
//! subprocess path stays the source of truth until the switch-over.
//!
//! # Design choices
//!
//! - The broker struct owns fds and protocol state; the calloop glue lives
//!   in `main.rs` (`register_dbus_sources` in commit 3) so the broker has
//!   zero calloop dep. This keeps it unit-testable with plain
//!   `socketpair()`.
//! - `offset` is a plain `Option<(i32, i32)>` — no `Arc<Mutex>`, because
//!   every callback runs on the event loop thread.
//! - Writes use a `VecDeque<u8>` back-pressure buffer per direction,
//!   mirroring [`crate::ipc::IpcServer`]'s pattern. If the peer isn't
//!   readable, bytes sit in the buffer until it is.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use emskin_dbus::broker::{apply_cursor_rewrites, state::ConnectionState};

/// Newtype for per-connection id. Generated sequentially by the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(u64);

/// Returned by [`DbusBroker::accept_one`]. Caller (calloop glue in
/// `main.rs`) uses the fds to register the client + upstream sockets as
/// separate Generic sources. `id` identifies the pair for subsequent pump
/// / flush calls.
pub struct ConnAccepted {
    pub id: ConnId,
    pub client_fd: RawFd,
    pub upstream_fd: RawFd,
}

/// Per-tick outcome from a pump call. Callers use this to decide whether
/// to drop the connection (on `PeerClosed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpOutcome {
    /// Read more bytes, connection still live.
    Active,
    /// EOF on the side we just read from — the pair is dead, caller
    /// should remove both calloop sources and drop the connection.
    PeerClosed,
}

struct Connection {
    client: UnixStream,
    upstream: UnixStream,
    state: ConnectionState,
    /// Bytes waiting to be written to `client` (came from upstream).
    client_out: VecDeque<u8>,
    /// Bytes waiting to be written to `upstream` (came from client,
    /// possibly with cursor bytes rewritten in place).
    upstream_out: VecDeque<u8>,
}

/// The in-process broker. Holds the listener, the upstream bus path for
/// per-connection dials, the shared focus-origin offset, and all active
/// connection state.
pub struct DbusBroker {
    listen_path: PathBuf,
    listener: UnixListener,
    upstream_path: PathBuf,
    offset: Option<(i32, i32)>,
    connections: HashMap<ConnId, Connection>,
    next_id: u64,
}

impl DbusBroker {
    /// Bind `session_dir/bus.sock` as the listener. `upstream` is the
    /// path of the real session bus — either parsed from
    /// `DBUS_SESSION_BUS_ADDRESS=unix:path=…` or passed in directly in
    /// tests.
    pub fn bind(session_dir: &Path, upstream: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(session_dir)?;
        let listen_path = session_dir.join("bus.sock");
        // Reuse of a stale socket (from a crashed prior emskin) is safe
        // because we own the session dir; unlink first then bind.
        let _ = std::fs::remove_file(&listen_path);
        let listener = UnixListener::bind(&listen_path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listen_path,
            listener,
            upstream_path: upstream,
            offset: None,
            connections: HashMap::new(),
            next_id: 1,
        })
    }

    pub fn listen_path(&self) -> &Path {
        &self.listen_path
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Current cursor-rewrite offset. `None` = pass-through.
    pub fn offset(&self) -> Option<(i32, i32)> {
        self.offset
    }

    /// Update the cursor-rewrite offset. Called from the tick's focus
    /// reconciler. `None` disables rewrite.
    pub fn set_offset(&mut self, off: Option<(i32, i32)>) {
        self.offset = off;
    }

    /// Accept one pending connection, dial upstream, register state.
    /// Returns `Ok(None)` when the listener has no pending connection
    /// (WouldBlock) — the calloop source is level-triggered so we'll be
    /// called again on the next ready event.
    ///
    /// On upstream dial failure we drop the accepted client; the embedded
    /// app will see its first `write()` fail. Alternative would be to
    /// keep a half-open connection, but DBus clients don't have a story
    /// for "half-dialed bus" so fail-fast is kinder.
    pub fn accept_one(&mut self) -> io::Result<Option<ConnAccepted>> {
        let client = match self.listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        let upstream = match UnixStream::connect(&self.upstream_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    upstream = ?self.upstream_path,
                    "dbus broker: upstream dial failed; dropping client"
                );
                return Ok(None);
            }
        };
        client.set_nonblocking(true)?;
        upstream.set_nonblocking(true)?;

        let id = ConnId(self.next_id);
        self.next_id += 1;
        let client_fd = client.as_raw_fd();
        let upstream_fd = upstream.as_raw_fd();

        self.connections.insert(
            id,
            Connection {
                client,
                upstream,
                state: ConnectionState::new(),
                client_out: VecDeque::new(),
                upstream_out: VecDeque::new(),
            },
        );

        tracing::debug!(?id, "dbus broker: connection accepted");
        Ok(Some(ConnAccepted {
            id,
            client_fd,
            upstream_fd,
        }))
    }

    /// Client → upstream pump. Reads all readable bytes from the client,
    /// feeds them through the DBus state machine, applies the cursor
    /// rewrite if an offset is set, and writes the result to the
    /// upstream side (buffering anything the kernel refuses).
    pub fn pump_client_to_upstream(&mut self, id: ConnId) -> io::Result<PumpOutcome> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(PumpOutcome::PeerClosed);
        };
        let mut buf = [0u8; 8 * 1024];
        let n = match conn.client.read(&mut buf) {
            Ok(0) => return Ok(PumpOutcome::PeerClosed),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PumpOutcome::Active),
            Err(e) if e.kind() == ErrorKind::Interrupted => return Ok(PumpOutcome::Active),
            Err(e) => return Err(e),
        };

        let mut out = conn
            .state
            .client_feed(&buf[..n])
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;

        for msg in &out.messages {
            tracing::info!(
                member = msg.header.member.as_deref().unwrap_or(""),
                interface = msg.header.interface.as_deref().unwrap_or(""),
                signature = msg.header.signature.as_deref().unwrap_or(""),
                body_len = msg.header.body_len,
                "client → bus message"
            );
        }

        if let Some(delta) = self.offset {
            apply_cursor_rewrites(&mut out, delta);
        }

        conn.upstream_out.extend(out.forward);
        Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
        Ok(PumpOutcome::Active)
    }

    /// Upstream → client pump. Raw pass-through (phase 1 doesn't inspect
    /// bus → client traffic). Same buffering story as the other pump.
    pub fn pump_upstream_to_client(&mut self, id: ConnId) -> io::Result<PumpOutcome> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(PumpOutcome::PeerClosed);
        };
        let mut buf = [0u8; 8 * 1024];
        let n = match conn.upstream.read(&mut buf) {
            Ok(0) => return Ok(PumpOutcome::PeerClosed),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PumpOutcome::Active),
            Err(e) if e.kind() == ErrorKind::Interrupted => return Ok(PumpOutcome::Active),
            Err(e) => return Err(e),
        };
        conn.client_out.extend(&buf[..n]);
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;
        Ok(PumpOutcome::Active)
    }

    /// Retry draining the upstream_out buffer after a prior WouldBlock.
    /// Wired to a WRITE-interest calloop source by the glue layer.
    pub fn flush_upstream_out(&mut self, id: ConnId) -> io::Result<bool> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(false);
        };
        Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
        Ok(!conn.upstream_out.is_empty())
    }

    /// Symmetric partner to [`Self::flush_upstream_out`] for the other
    /// direction.
    pub fn flush_client_out(&mut self, id: ConnId) -> io::Result<bool> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(false);
        };
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;
        Ok(!conn.client_out.is_empty())
    }

    /// Drop connection state. Caller is responsible for removing the two
    /// calloop sources first — this only frees the fds and the parser.
    pub fn remove_connection(&mut self, id: ConnId) {
        if self.connections.remove(&id).is_some() {
            tracing::debug!(?id, "dbus broker: connection removed");
        }
    }

    /// Write as many bytes from `buf` to `stream` as the kernel will
    /// take without blocking. Leftover stays in `buf`. Matches the
    /// pattern in [`crate::ipc::connection::IpcConn::try_flush`].
    fn try_flush(stream: &mut UnixStream, buf: &mut VecDeque<u8>) -> io::Result<()> {
        while !buf.is_empty() {
            let (front, back) = buf.as_slices();
            let slice = if !front.is_empty() { front } else { back };
            match stream.write(slice) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    buf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Drop for DbusBroker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.listen_path);
    }
}

/// Parse `unix:path=/run/user/1000/bus[,guid=…]` into the filesystem
/// path. Mirrors the parser in the old `emskin-dbus-proxy` binary but
/// lives alongside the broker now.
pub fn parse_unix_bus_address(addr: &str) -> io::Result<PathBuf> {
    const PREFIX: &str = "unix:path=";
    let stripped = addr.strip_prefix(PREFIX).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported bus scheme: {addr}"),
        )
    })?;
    let path = stripped.split(',').next().unwrap_or(stripped);
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn parses_plain_unix_path_form() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn parses_unix_path_with_guid_suffix() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus,guid=deadbeef").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn rejects_tcp_scheme() {
        assert!(parse_unix_bus_address("tcp:host=localhost,port=1234").is_err());
    }

    #[test]
    fn set_offset_round_trip() {
        // Tiny: just exercise the Option<(i32, i32)> plumbing.
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        // Fake upstream: a listener we never dial to.
        let upstream_path = dir.path().join("upstream.sock");
        let _u = UnixListener::bind(&upstream_path).unwrap();
        let mut b = DbusBroker::bind(&session, upstream_path).unwrap();
        assert_eq!(b.offset(), None);
        b.set_offset(Some((10, 20)));
        assert_eq!(b.offset(), Some((10, 20)));
        b.set_offset(None);
        assert_eq!(b.offset(), None);
    }

    /// End-to-end rewrite: simulated client writes a SetCursorRect via
    /// the listener, broker forwards to upstream with `(dx, dy)` added.
    #[test]
    fn client_to_upstream_applies_offset() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();

        let mut broker = DbusBroker::bind(&session, upstream_path).unwrap();
        broker.set_offset(Some((50, 60)));

        // Client dials the broker.
        let mut client = UnixStream::connect(broker.listen_path()).unwrap();
        client.set_nonblocking(true).unwrap();

        // Broker accepts + dials upstream. The listener's accept() was
        // just triggered so poll briefly.
        thread::sleep(Duration::from_millis(20));
        let accepted = broker.accept_one().unwrap().expect("accept ready");
        let (upstream_peer, _) = upstream_listener.accept().unwrap();
        upstream_peer.set_nonblocking(true).unwrap();

        // Build handshake + a SetCursorRect method_call by hand.
        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let call = build_set_cursor_rect(7, (100, 200, 10, 20));
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&call);
        client.write_all(&payload).unwrap();

        // Drain the broker's client fd a couple of times to be sure
        // both the auth prefix and the full message pass through.
        for _ in 0..5 {
            broker.pump_client_to_upstream(accepted.id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Read from upstream side.
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..5 {
            match (&upstream_peer).read(&mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                _ => thread::sleep(Duration::from_millis(5)),
            }
        }

        assert!(got.starts_with(handshake), "handshake should pass through");
        let msg_bytes = &got[handshake.len()..];
        let hdr = emskin_dbus::dbus::message::parse_header(msg_bytes).unwrap();
        assert_eq!(hdr.member.as_deref(), Some("SetCursorRect"));
        let body_start = msg_bytes.len() - hdr.body_len as usize;
        let body = &msg_bytes[body_start..];
        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), 150);
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 260);
        // w, h unchanged
        assert_eq!(i32::from_le_bytes(body[8..12].try_into().unwrap()), 10);
        assert_eq!(i32::from_le_bytes(body[12..16].try_into().unwrap()), 20);

        broker.remove_connection(accepted.id);
    }

    // ------- DBus message builders (copied from emskin-dbus io.rs tests) -------

    fn pad_to(out: &mut Vec<u8>, bound: usize) {
        while !out.len().is_multiple_of(bound) {
            out.push(0);
        }
    }

    fn push_string_field(out: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
        pad_to(out, 8);
        out.push(code);
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
        pad_to(out, 4);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }

    fn push_signature_field(out: &mut Vec<u8>, code: u8, sig: &str) {
        pad_to(out, 8);
        out.push(code);
        out.push(1);
        out.push(b'g');
        out.push(0);
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
    }

    fn build_set_cursor_rect(serial: u32, coords: (i32, i32, i32, i32)) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/a");
        push_string_field(&mut fields, 2, "s", "org.fcitx.Fcitx.InputContext1");
        push_string_field(&mut fields, 3, "s", "SetCursorRect");
        push_string_field(&mut fields, 6, "s", "org.fcitx.Fcitx5");
        push_signature_field(&mut fields, 8, "iiii");

        let mut body = Vec::new();
        body.extend_from_slice(&coords.0.to_le_bytes());
        body.extend_from_slice(&coords.1.to_le_bytes());
        body.extend_from_slice(&coords.2.to_le_bytes());
        body.extend_from_slice(&coords.3.to_le_bytes());

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }
}
