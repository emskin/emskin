//! IME (input method) bridge between the host compositor and embedded
//! Wayland clients via `text_input_v3`.
//!
//! Three smithay-imposed constraints drive the design — see
//! `crates/emskin/CLAUDE.md` → IME for the full "why":
//!
//! - `set_ime_allowed` must be toggled per-focused-client (registering `TextInputManagerState` makes fcitx5-gtk abandon its DBus path for text_input_v3, so enabling host IME for a GTK/Qt client that handles its own IM breaks input).
//! - `text_input.enter()/leave()` must be called by hand from `focus_changed` (smithay gates them on `input_method.has_instance()` and emskin implements no input_method protocol).
//! - The `set_ime_allowed` decision is deferred via `ime_enabled` + [`ImeBridge::take_ime_enabled`] (`focus_changed` has no access to the winit backend).

use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::text_input::{TextInputHandle, TextInputManagerState, TextInputSeat};

use crate::apps::AppManager;
use crate::EmskinState;

/// `(-1, -1)` sentinel per text_input_v3 for "no cursor position".
const NO_CURSOR: (i32, i32) = (-1, -1);

/// Identifier for an fcitx5 input context the broker has allocated.
/// `(DbusBroker connection id, IC object path)`. Paired with
/// [`ActiveFcitxIc::app_origin`] so the winit IME caret area stays
/// correct across window moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveFcitxIc {
    pub conn: crate::dbus_broker::ConnId,
    pub ic_path: String,
}

pub struct ImeBridge {
    focused_surface: Option<WlSurface>,
    /// Host IME enabled/disabled decision waiting for the render loop
    /// to apply via `set_ime_allowed`. Drained by `take_ime_enabled`
    /// (write-once, read-once semantic — the `Option` distinguishes
    /// "no change to apply" from "apply false").
    ime_enabled: Option<bool>,
    /// Fcitx5 IC currently focused (via broker-observed `FocusIn`).
    /// When winit emits an IME event (`Preedit` / `Commit`) we look up
    /// this IC to decide which DBus client to forward the result to.
    /// At most one IC is active at a time — `FocusIn` on a new IC
    /// evicts the previous one.
    active_fcitx_ic: Option<ActiveFcitxIc>,
    /// Cursor area waiting for the render loop to call
    /// `window.set_ime_cursor_area`. Drained by
    /// [`ImeBridge::take_pending_cursor_area`]. Coords are
    /// **emskin-winit-local** (`focused_app_origin + client_rect`) so
    /// the render loop can hand them straight to winit.
    pending_cursor_area: Option<([i32; 2], [i32; 2])>,
}

impl ImeBridge {
    pub fn new(dh: &DisplayHandle) -> Self {
        // The global is owned by `Display` after registration; the
        // returned `TextInputManagerState` (a bare `GlobalId` wrapper)
        // has no Drop impl that unregisters, so dropping it is a no-op.
        let _ = TextInputManagerState::new::<EmskinState>(dh);
        Self {
            focused_surface: None,
            ime_enabled: None,
            active_fcitx_ic: None,
            pending_cursor_area: None,
        }
    }

    /// Current fcitx5 IC, if any, that winit IME events should be
    /// forwarded to.
    pub fn active_fcitx_ic(&self) -> Option<&ActiveFcitxIc> {
        self.active_fcitx_ic.as_ref()
    }

    /// Drain the pending `set_ime_cursor_area` call. Called by the
    /// winit render loop where the backend is accessible. `(position,
    /// size)` in emskin-winit-local coords (`i32` × 2 + `i32` × 2).
    pub fn take_pending_cursor_area(&mut self) -> Option<([i32; 2], [i32; 2])> {
        self.pending_cursor_area.take()
    }

    /// Process a [`crate::dbus_broker::FcitxEvent`] observed by the
    /// broker. Updates `active_fcitx_ic`, stages an
    /// `ime_enabled` / `pending_cursor_area` change for the winit
    /// render loop to apply. `app_origin` is the focused embedded
    /// app's emskin-space origin (computed elsewhere — the broker's
    /// rects are client-surface-local, so this offset moves them into
    /// emskin-winit-local).
    pub fn on_fcitx_event(
        &mut self,
        event: crate::dbus_broker::FcitxEvent,
        app_origin: Option<[i32; 2]>,
    ) {
        use crate::dbus_broker::FcitxEvent;

        match event {
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: true,
                rect,
            } => {
                tracing::debug!(?conn, ?ic_path, "fcitx IC FocusIn → activating winit IME");
                self.active_fcitx_ic = Some(ActiveFcitxIc { conn, ic_path });
                self.ime_enabled = Some(true);
                if let (Some(r), Some(origin)) = (rect, app_origin) {
                    self.pending_cursor_area = Some((
                        [origin[0] + r[0], origin[1] + r[1]],
                        [r[2].max(1), r[3].max(1)],
                    ));
                }
            }
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: false,
                ..
            } => {
                // Only clear if the unfocused IC is the active one.
                // Spurious FocusOut on a stale IC mustn't kick out the
                // currently-active client.
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    tracing::debug!(?conn, ?ic_path, "fcitx IC FocusOut → deactivating winit IME");
                    self.active_fcitx_ic = None;
                    self.ime_enabled = Some(false);
                }
            }
            FcitxEvent::CursorRect {
                conn,
                ic_path,
                rect,
            } => {
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    if let Some(origin) = app_origin {
                        self.pending_cursor_area = Some((
                            [origin[0] + rect[0], origin[1] + rect[1]],
                            [rect[2].max(1), rect[3].max(1)],
                        ));
                    }
                }
            }
            FcitxEvent::IcDestroyed { conn, ic_path } => {
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    self.active_fcitx_ic = None;
                    self.ime_enabled = Some(false);
                }
            }
        }
    }

    /// Bridge text_input enter/leave on keyboard focus change and decide
    /// whether host IME should be enabled for the new focus.
    ///
    /// `new_focus` is the focused surface projected from
    /// `KeyboardFocusTarget` via `WaylandFocus::wl_surface()` — X clients
    /// surface here too once associated by xwayland-satellite.
    pub fn on_focus_changed(&mut self, seat: &Seat<EmskinState>, new_focus: Option<WlSurface>) {
        let ti = seat.text_input();
        let old = self.focused_surface.take();
        transition_focus(ti, old, &new_focus);
        let enabled = focused_client_has_text_input(ti);
        tracing::debug!(
            "IME focus_changed: has_focus={} ime_enabled={enabled}",
            new_focus.is_some()
        );
        self.focused_surface = new_focus;
        self.ime_enabled = Some(enabled);
    }

    /// Forward a host IME event to the focused text_input_v3 client and
    /// reposition the host IME popup to follow the client's caret.
    pub fn on_host_ime_event(
        &mut self,
        event: winit_crate::event::Ime,
        seat: &Seat<EmskinState>,
        apps: &AppManager,
        window: &winit_crate::window::Window,
    ) {
        use winit_crate::event::Ime;

        let ti = seat.text_input();
        sync_ime_cursor_area(ti, apps, window);

        match event {
            Ime::Enabled => {
                tracing::trace!("IME host event: Enabled");
                ti.enter();
            }
            Ime::Preedit(text, cursor) => {
                tracing::trace!(
                    "IME host event: Preedit (len={}, cursor={cursor:?})",
                    text.len()
                );
                let (begin, end) = cursor
                    .map(|(b, e)| (b as i32, e as i32))
                    .unwrap_or(NO_CURSOR);
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(Some(text.clone()), begin, end);
                });
                ti.done(false);
            }
            Ime::Commit(text) => {
                tracing::trace!("IME host event: Commit (len={})", text.len());
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(None, 0, 0);
                    client.commit_string(Some(text.clone()));
                });
                ti.done(false);
            }
            Ime::Disabled => {
                tracing::trace!("IME host event: Disabled");
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(None, 0, 0);
                });
                ti.done(false);
                ti.leave();
            }
        }
    }

    /// Drain the deferred `set_ime_allowed` decision, if any. Called
    /// from the winit render loop where the backend is accessible.
    pub fn take_ime_enabled(&mut self) -> Option<bool> {
        let taken = self.ime_enabled.take();
        if let Some(enabled) = taken {
            tracing::debug!("IME: applying set_ime_allowed({enabled})");
        }
        taken
    }

    /// Clear state on workspace switch — stale surface refs would
    /// otherwise route text_input events to the wrong client. The
    /// `Some(false)` pending decision also disables host IME during the
    /// switch transient; the next `on_focus_changed` will re-enable it
    /// if the incoming focus has text_input_v3 bound.
    pub fn reset_on_workspace_switch(&mut self) {
        tracing::debug!("IME: reset on workspace switch");
        self.focused_surface = None;
        self.ime_enabled = Some(false);
        self.active_fcitx_ic = None;
        self.pending_cursor_area = None;
    }
}

/// Update smithay's text_input focus and fire enter/leave at the right
/// clients. smithay's keyboard handler would do this automatically if
/// we had an input_method protocol registered, but we don't — hence
/// the manual dance. The `leave` event must be sent *while* text_input
/// focus still points at `old`, otherwise smithay routes it to the new
/// surface instead of the departing one.
fn transition_focus(ti: &TextInputHandle, old: Option<WlSurface>, new: &Option<WlSurface>) {
    if old.as_ref() == new.as_ref() {
        return;
    }
    tracing::debug!(
        "IME focus transition: had_old={} has_new={}",
        old.is_some(),
        new.is_some()
    );
    if old.is_some() {
        ti.set_focus(old);
        ti.leave();
    }
    ti.set_focus(new.clone());
    if new.is_some() {
        ti.enter();
    }
}

/// Whether the currently focused client has bound `text_input_v3`.
/// smithay exposes no direct query, so we probe via the mutation API.
fn focused_client_has_text_input(ti: &TextInputHandle) -> bool {
    let mut found = false;
    ti.with_focused_text_input(|_, _| found = true);
    found
}

/// Position the host IME popup on the embedded client's caret.
fn sync_ime_cursor_area(
    ti: &TextInputHandle,
    apps: &AppManager,
    window: &winit_crate::window::Window,
) {
    let Some(rect) = ti.cursor_rectangle() else {
        return;
    };
    // cursor_rectangle is surface-local; offset by the embedded app's
    // compositor-space origin so the popup lands on-screen.
    let app_loc = ti
        .focus()
        .and_then(|surface| apps.surface_geometry(&surface))
        .map(|geo| geo.loc)
        .unwrap_or_default();
    window.set_ime_cursor_area(
        winit_crate::dpi::LogicalPosition::new(
            (rect.loc.x + app_loc.x) as f64,
            (rect.loc.y + app_loc.y) as f64,
        ),
        winit_crate::dpi::LogicalSize::new(rect.size.w as f64, rect.size.h as f64),
    );
}

smithay::delegate_text_input_manager!(EmskinState);
