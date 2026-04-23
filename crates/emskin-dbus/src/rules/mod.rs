//! Message-rewriting rules applied by the broker.
//!
//! Each submodule owns one self-contained pattern: classify a parsed
//! [`crate::dbus::message::Header`], and — if it matches — mutate the
//! message body bytes in place. Rules are pure functions so they can be
//! exercised with byte fixtures alone, independent of sockets or the
//! [`crate::broker::state::ConnectionState`] loop.
//!
//! Current rules:
//!
//! - [`cursor`] — `SetCursorRect` / `SetCursorLocation` coordinate
//!   translation. Closes emskin issue #55.

pub mod cursor;
