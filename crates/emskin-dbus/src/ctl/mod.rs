//! Control channel used by emskin to push focus rectangles (and other
//! hints) into the running proxy. Frame format is the same length-prefixed
//! JSON codec as emskin's existing Emacs IPC; see [`crate::protocol`] for
//! the message shapes.
//!
//! Phase 1 only cares about the rect stream. The server [`server::run`]
//! translates incoming `FocusChanged` / `RectChanged` / `FocusCleared`
//! messages into updates on the shared [`crate::broker::io::SharedOffset`]
//! cell the per-connection brokers read on every feed.

pub mod client;
pub mod server;
