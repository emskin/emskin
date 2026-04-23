//! emskin-dbus — Selective DBus session-bus proxy for nested Wayland compositors.
//!
//! Scope (phase 1):
//!   - Transparent pass-through of DBus session-bus traffic from embedded
//!     clients to the host session bus.
//!   - Control channel (ctl-socket) so the compositor can push per-client
//!     host-screen rectangles.
//!   - Arg rewrite for `org.fcitx.Fcitx5.InputContext1.SetCursorRect` /
//!     `org.fcitx.Fcitx.InputContext.SetCursorLocation` that translates
//!     client-local caret coordinates into host-screen-absolute coordinates
//!     using the rectangles pushed over ctl-socket. Closes emskin/emskin#55.
//!
//! Later phases (not in this crate's phase 1 surface):
//!   - Local name registry for `org.gnome.*` / `org.kde.*` `RequestName`
//!     interception (closes emskin/emskin#60).
//!   - Merged `ListNames` / `NameOwnerChanged` view.
//!   - Policy-driven per-service passthrough / local-own / deny matrix.
//!
//! The crate has zero smithay deps on purpose — it is reusable by any
//! other nested compositor (cage, wio, niri-in-plasma, …) in the same
//! spirit as the sibling `emskin-clipboard` crate.

pub mod broker;
pub mod codec;
pub mod ctl;
pub mod dbus;
pub mod protocol;
pub mod rules;
