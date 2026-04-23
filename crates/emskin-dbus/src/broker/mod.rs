//! Per-connection broker logic: a pure state machine that consumes raw socket
//! bytes and emits the bytes the proxy should forward, plus parsed message
//! headers observed on the client → bus direction.
//!
//! The socket-level I/O (listening, `accept()`, `SCM_RIGHTS` fd passing,
//! `poll()`) lives in the binary crate; this module is intentionally pure
//! so it can be exercised end-to-end in unit tests without spinning up
//! Unix sockets.
//!
//! Shape follows `xdg-dbus-proxy`'s `flatpak-proxy.c` — auth bytes are
//! forwarded incrementally as they arrive, and the scanner runs against a
//! separate accumulator so it can still locate `BEGIN\r\n` across chunk
//! boundaries. After BEGIN, we parse DBus-wire messages and forward them
//! one at a time so the Task #5 rule engine can see headers at each
//! boundary.

pub mod io;
pub mod state;
