//! Unified keyboard focus abstraction.
//!
//! Wayland and XWayland clients need different focus plumbing:
//! - Wayland toplevels: `wl_keyboard.enter(wl_surface)` is enough.
//! - X11 clients: the compositor must also send X11 `SetInputFocus` /
//!   ICCCM `WM_TAKE_FOCUS` (per `WmInputModel`) and toggle EWMH
//!   `_NET_WM_STATE_FOCUSED`.
//!
//! Smithay already implements `KeyboardTarget` for both `WlSurface` and
//! `X11Surface`; the X11 impl handles `SetInputFocus` / `WM_TAKE_FOCUS`
//! automatically, and queues `pending_enter` when the backing `wl_surface`
//! hasn't been associated yet. Wrapping both in a single enum lets the
//! rest of the compositor just call `keyboard.set_focus(target)` without
//! caring which protocol the client speaks.
//!
//! This mirrors anvil's `KeyboardFocusTarget`. We only need `Window`
//! (covers both Wayland and X11 via `WindowSurface`) and `LayerSurface`
//! (rofi / zofi / emskin-bar).

use std::borrow::Cow;

use smithay::{
    backend::input::KeyState,
    desktop::{LayerSurface, PopupKind, Window, WindowSurface},
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        Seat,
    },
    reexports::wayland_server::{backend::ObjectId, protocol::wl_surface::WlSurface},
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
};

use crate::EmskinState;

/// What the keyboard is focused on. Wayland clients and X11 clients share
/// one type so call sites don't have to branch on protocol.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum KeyboardFocusTarget {
    /// A toplevel window — Wayland xdg_toplevel or XWayland X11 surface.
    Window(Window),
    /// A wlr-layer-shell surface (launcher, bar, etc.).
    Layer(LayerSurface),
    /// An xdg_popup / input-method popup grabbing the keyboard.
    Popup(PopupKind),
}

impl KeyboardFocusTarget {
    /// Returns the underlying smithay `KeyboardTarget` so the enum impl
    /// can just delegate.
    fn inner(&self) -> &dyn KeyboardTarget<EmskinState> {
        match self {
            Self::Window(w) => match w.underlying_surface() {
                WindowSurface::Wayland(t) => t.wl_surface(),
                WindowSurface::X11(s) => s,
            },
            Self::Layer(l) => l.wl_surface(),
            Self::Popup(p) => p.wl_surface(),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            Self::Window(w) => w.alive(),
            Self::Layer(l) => l.alive(),
            Self::Popup(p) => p.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Window(w) => w.wl_surface(),
            Self::Layer(l) => Some(Cow::Borrowed(l.wl_surface())),
            Self::Popup(p) => Some(Cow::Borrowed(p.wl_surface())),
        }
    }

    #[inline]
    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Window(w) => match w.underlying_surface() {
                WindowSurface::Wayland(t) => t.wl_surface().same_client_as(object_id),
                WindowSurface::X11(s) => s.same_client_as(object_id),
            },
            Self::Layer(l) => l.wl_surface().same_client_as(object_id),
            Self::Popup(p) => p.wl_surface().same_client_as(object_id),
        }
    }
}

impl KeyboardTarget<EmskinState> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.inner().enter(seat, data, keys, serial);
        // EWMH activation state (`_NET_WM_STATE_FOCUSED`) is orthogonal to
        // X11 SetInputFocus / WM_TAKE_FOCUS — the X11 `KeyboardTarget` impl
        // handles focus-transfer but not the Activated window-state bit,
        // which GTK uses to highlight the frame. Flip it here so X11
        // clients see themselves as "the active window".
        if let Self::Window(w) = self {
            if let Some(x11) = w.x11_surface() {
                if let Err(e) = x11.set_activated(true) {
                    tracing::warn!("X11 set_activated(true) failed: {e}");
                }
            }
        }
    }

    fn leave(&self, seat: &Seat<EmskinState>, data: &mut EmskinState, serial: Serial) {
        if let Self::Window(w) = self {
            if let Some(x11) = w.x11_surface() {
                if let Err(e) = x11.set_activated(false) {
                    tracing::warn!("X11 set_activated(false) failed: {e}");
                }
            }
        }
        self.inner().leave(seat, data, serial);
    }

    fn key(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        self.inner().key(seat, data, key, state, serial, time);
    }

    fn modifiers(
        &self,
        seat: &Seat<EmskinState>,
        data: &mut EmskinState,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        self.inner().modifiers(seat, data, modifiers, serial);
    }
}

impl From<Window> for KeyboardFocusTarget {
    #[inline]
    fn from(value: Window) -> Self {
        Self::Window(value)
    }
}

impl From<&Window> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &Window) -> Self {
        Self::Window(value.clone())
    }
}

impl From<LayerSurface> for KeyboardFocusTarget {
    #[inline]
    fn from(value: LayerSurface) -> Self {
        Self::Layer(value)
    }
}

impl From<&LayerSurface> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &LayerSurface) -> Self {
        Self::Layer(value.clone())
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    #[inline]
    fn from(value: PopupKind) -> Self {
        Self::Popup(value)
    }
}

impl From<&PopupKind> for KeyboardFocusTarget {
    #[inline]
    fn from(value: &PopupKind) -> Self {
        Self::Popup(value.clone())
    }
}

/// Some smithay APIs (PopupGrab, pointer grab helpers) require the
/// compositor's `KeyboardFocus` to be convertible back to a `WlSurface` —
/// every variant we hold ultimately wraps one.
impl From<KeyboardFocusTarget> for WlSurface {
    #[inline]
    fn from(value: KeyboardFocusTarget) -> Self {
        value
            .wl_surface()
            .map(|c| c.into_owned())
            .expect("KeyboardFocusTarget must always have a wl_surface")
    }
}
