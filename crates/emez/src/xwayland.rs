//! XWayland integration for `emez`.
//!
//! emez spawns its own XWayland instance when `--xwayland` is passed so
//! the Wayland host can expose an X DISPLAY (letting outside X clients
//! like `xclip` participate in clipboard tests). The implementation is
//! deliberately minimal:
//!
//! - `XwmHandler` accepts map/unmap/configure requests but does not
//!   draw, place, or stack anything. Selection-owner clients (xclip)
//!   only need their unmapped window to reach MapRequest → set_mapped
//!   for xfixes events to start flowing.
//! - `SelectionHandler::new_selection` / `send_selection` bridges the
//!   Wayland side (fed by emskin over data-control) to the X side so
//!   xclip sees it, and vice versa.
//! - `start_xwayland` writes the assigned DISPLAY to a file when ready,
//!   which the test harness polls to know when XWayland is usable.

use std::{os::unix::io::OwnedFd, path::PathBuf, process::Stdio};

use smithay::{
    utils::{Logical, Rectangle},
    wayland::selection::{
        data_device::{
            clear_data_device_selection, current_data_device_selection_userdata,
            request_data_device_client_selection, set_data_device_selection,
        },
        primary_selection::{
            clear_primary_selection, current_primary_selection_userdata,
            request_primary_client_selection, set_primary_selection,
        },
        SelectionTarget,
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
    },
};

use crate::state::Emez;

impl Emez {
    /// Spawn XWayland and register its event source on the loop. When
    /// XWayland reports `Ready`, start the X11Wm and (optionally) write
    /// the DISPLAY number to `ready_file`.
    ///
    /// `display` pins a specific DISPLAY number (used by the test
    /// harness to avoid /tmp/.X11-unix/X* races between parallel emez
    /// spawns). `None` lets smithay pick one.
    pub fn start_xwayland(
        &mut self,
        display: Option<u32>,
        ready_file: Option<PathBuf>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Capture Xwayland's own stderr to `xwayland.log` next to the
        // ready-file. Otherwise startup crashes (e.g. `SIGABRT` on some
        // CI distros with no obvious cause in the parent logs) leave no
        // trace. Files-based capture survives signal-truncated shutdown.
        let xwayland_stderr = ready_file
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("xwayland.log"))
            .and_then(|path| std::fs::File::create(&path).ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);

        self.xwayland_ready_file = ready_file;

        let (xwayland, client) = XWayland::spawn(
            &self.display_handle,
            display,
            std::iter::empty::<(String, String)>(),
            true,
            Stdio::null(),
            xwayland_stderr,
            |_| (),
        )?;
        self.xwayland_client = Some(client.clone());

        let display_handle = self.display_handle.clone();
        let loop_handle = self.loop_handle.clone();
        self.loop_handle
            .insert_source(xwayland, move |event, _, state| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => match X11Wm::start_wm(
                    loop_handle.clone(),
                    &display_handle,
                    x11_socket,
                    client.clone(),
                ) {
                    Ok(wm) => {
                        state.xwm = Some(wm);
                        state.xdisplay = Some(display_number);
                        tracing::info!("emez XWayland ready on DISPLAY=:{display_number}");
                        if let Some(path) = state.xwayland_ready_file.as_ref() {
                            if let Err(e) = std::fs::write(path, format!(":{display_number}\n")) {
                                tracing::error!(
                                    "emez: failed to write xwayland ready file {}: {e}",
                                    path.display()
                                );
                            }
                        }
                    }
                    Err(e) => tracing::error!("emez: X11Wm::start_wm failed: {e}"),
                },
                XWaylandEvent::Error => {
                    tracing::error!("emez: XWayland crashed on startup");
                }
            })
            .map_err(|e| format!("insert xwayland source: {e}"))?;

        Ok(())
    }
}

impl XwmHandler for Emez {
    fn xwm_state(&mut self, _: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("xwm accessed before Ready")
    }

    fn new_window(&mut self, _: XwmId, _: X11Surface) {}
    fn new_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn map_window_request(&mut self, _: XwmId, window: X11Surface) {
        // Accept the map so X clients finish their lifecycle. emez does
        // not render anything; we just need the mapped bit flipped so
        // selection-owner clients (xclip) can proceed.
        let _ = window.set_mapped(true);
    }

    fn mapped_override_redirect_window(&mut self, _: XwmId, _: X11Surface) {}

    fn unmapped_window(&mut self, _: XwmId, window: X11Surface) {
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _: XwmId, _: X11Surface) {}

    fn configure_request(
        &mut self,
        _: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geo = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _: XwmId,
        _: X11Surface,
        _: Rectangle<i32, Logical>,
        _: Option<u32>,
    ) {
    }

    fn resize_request(&mut self, _: XwmId, _: X11Surface, _: u32, _: X11ResizeEdge) {}
    fn move_request(&mut self, _: XwmId, _: X11Surface, _: u32) {}

    // -- Selection bridging (X → Wayland) -------------------------------

    fn allow_selection_access(&mut self, _: XwmId, _: SelectionTarget) -> bool {
        // emez has no focus concept; always allow so pending transfers
        // don't block. The worst case is spurious data on an idle seat,
        // which tests tolerate.
        true
    }

    fn send_selection(
        &mut self,
        _: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                    tracing::error!(?err, "emez: request wayland clipboard for XWayland");
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) = request_primary_client_selection(&self.seat, mime_type, fd) {
                    tracing::error!(?err, "emez: request wayland primary for XWayland");
                }
            }
        }
    }

    fn new_selection(&mut self, _: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types, ());
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types, ());
            }
        }
    }

    fn cleared_selection(&mut self, _: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&self.display_handle, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&self.display_handle, &self.seat);
                }
            }
        }
    }

    fn disconnected(&mut self, _: XwmId) {
        self.xwm = None;
    }
}

// Wayland → X selection forwarding is implemented directly in
// `handlers.rs::SelectionHandler for Emez` — see `new_selection` /
// `send_selection` there. Those hooks call into `self.xwm` in two
// lines, which is shorter than a free-function indirection would be.
