//! Wire protocol for the ctl-socket shared between `emskin` and the
//! `emskin-dbus-proxy` binary.
//!
//! Frame format: `[u32 LE: payload_len][payload: UTF-8 JSON]` — same codec
//! emskin already uses for its Emacs IPC, chosen for consistency.
//!
//! Protocol characteristics:
//!   - Bidirectional, length-framed, JSON body.
//!   - `Ready` is the startup barrier — emskin waits for it before spawning
//!     anything that will inherit `DBUS_SESSION_BUS_ADDRESS`.
//!   - Idempotent where possible: `RectChanged` can be dropped under
//!     backpressure; `ClientBorn` / `CtxGone` must be delivered (they
//!     bracket per-ctx lookups in the proxy).

use serde::{Deserialize, Serialize};

/// Rectangle in emskin-winit-local coordinates (x, y, w, h).
///
/// The proxy adds these to the incoming client-caret-local `SetCursorRect`
/// coordinates to produce host-screen-absolute coordinates that fcitx5
/// consumes.
pub type Rect = [i32; 4];

/// Opaque per-client handle minted by emskin.
///
/// The proxy resolves `bus_unique_name -> pid -> ctx` (the `pid -> ctx`
/// half is populated by `EmskinToProxy::ClientBorn`, the
/// `bus_unique_name -> pid` half by `SO_PEERCRED` on the bus-socket
/// accept).
pub type Ctx = u64;

/// Messages emskin pushes down the ctl-socket to the proxy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EmskinToProxy {
    /// A freshly-spawned Wayland/X client just produced its first toplevel.
    /// Lets the proxy map the connection back to a compositor-side `ctx`.
    ClientBorn { pid: u32, ctx: Ctx },

    /// Keyboard focus moved to `ctx`. `rect` is the current host-local
    /// rectangle of that app's surface — used as the offset baseline for
    /// any `SetCursorRect` arriving from this ctx afterwards.
    FocusChanged { ctx: Ctx, rect: Rect },

    /// The focused ctx's rectangle changed (app resize, fullscreen toggle,
    /// re-layout) but focus itself didn't move.
    RectChanged { ctx: Ctx, rect: Rect },

    /// Multiple ctx rectangles changed at once (workspace switch, layer-
    /// shell relayout, batch migrate_app_to_active). Sent as one message
    /// to avoid a burst of `RectChanged`.
    BulkRects { updates: Vec<(Ctx, Rect)> },

    /// No focus currently — clear any "active ctx" state in the proxy.
    FocusCleared,

    /// Workspace switch. Proxy may clear per-ctx caches tied to the old
    /// workspace.
    WorkspaceSwitched { workspace_id: u64 },

    /// `ctx`'s client disappeared. Drop the pid/ctx mapping.
    CtxGone { ctx: Ctx },

    /// Polite shutdown — proxy drains in-flight messages and exits.
    Shutdown,
}

/// Messages the proxy pushes back to emskin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyToEmskin {
    /// Bus socket is now listening — it is safe for emskin to spawn child
    /// processes that will consume `DBUS_SESSION_BUS_ADDRESS`.
    Ready,

    /// Non-fatal condition worth surfacing in the compositor's logs.
    Error { context: String, message: String },

    /// Optional health tick. Can be used by e2e tests to assert liveness
    /// without tailing logs.
    Stats { active_conns: u32, msg_rate: u32 },
}
