//! Synthesize method_return frames for intercepted fcitx5 method_calls.
//!
//! Given a parsed request [`Frame`] + classified [`FcitxMethod`], build
//! the bytes the broker writes back to the client. The broker doesn't
//! forward the request to the real fcitx5 once we're in this code path.

use zvariant::ObjectPath;

use crate::dbus::frame::Frame;

use super::classify::FcitxMethod;
use super::ic::IcRegistry;

/// Mint a synthetic method_return for `method`, encode to wire bytes.
///
/// Mutates `registry` for `CreateInputContext` (allocates a new IC) and
/// `DestroyIC` (frees one); other variants update in-place IC state.
///
/// `serial_counter` is the broker's per-connection outgoing serial
/// counter — the DBus spec requires non-zero serials, so callers should
/// initialize to 1 and let [`next_nonzero`] do the housekeeping.
pub fn build_reply(
    request: &Frame<'_>,
    method: &FcitxMethod,
    registry: &mut IcRegistry,
    serial_counter: &mut u32,
) -> Vec<u8> {
    let serial = next_nonzero(serial_counter);

    let frame = match method {
        FcitxMethod::CreateInputContext { .. } => {
            let (path, state) = registry.allocate();
            let object_path =
                ObjectPath::try_from(path.as_str()).expect("registry produces valid path");
            // Reply signature `oay` is two top-level args, not a struct.
            // Wrapping `(oay)` as a struct trips strict DBus decoders
            // (GDBus, Qt DBus) — that's how WeChat silently drops the
            // reply when this is wrong.
            Frame::method_return(request)
                .serial(serial)
                .body_args()
                .arg(&object_path)
                .arg(&state.uuid.to_vec())
                .done()
                .build()
        }

        FcitxMethod::DestroyIC { ic_path } => {
            registry.destroy(ic_path);
            Frame::method_return(request).serial(serial).build()
        }

        FcitxMethod::FocusIn { ic_path } | FcitxMethod::FocusOut { ic_path } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.focused = matches!(method, FcitxMethod::FocusIn { .. });
            }
            Frame::method_return(request).serial(serial).build()
        }

        FcitxMethod::SetCapability {
            ic_path,
            capability,
        } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.capability = *capability;
            }
            Frame::method_return(request).serial(serial).build()
        }

        FcitxMethod::SetCursorRect {
            ic_path, x, y, w, h,
        }
        | FcitxMethod::SetCursorRectV2 {
            ic_path, x, y, w, h, ..
        } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.cursor_rect = Some([*x, *y, *w, *h]);
            }
            Frame::method_return(request).serial(serial).build()
        }

        FcitxMethod::SetCursorLocation { ic_path, x, y } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.cursor_rect = Some([*x, *y, 0, 0]);
            }
            Frame::method_return(request).serial(serial).build()
        }

        FcitxMethod::Reset { .. }
        | FcitxMethod::SetSurroundingText { .. }
        | FcitxMethod::SetSurroundingTextPosition { .. } => {
            Frame::method_return(request).serial(serial).build()
        }
    };

    frame.encode()
}

/// Increment `counter`, skipping zero (DBus spec requires non-zero
/// serials). Wraps `u32::MAX` → 1 to stay positive.
pub fn next_nonzero(counter: &mut u32) -> u32 {
    *counter = counter.wrapping_add(1);
    if *counter == 0 {
        *counter = 1;
    }
    *counter
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::frame::{Frame, Kind};

    fn create_input_context_request(serial: u32) -> Vec<u8> {
        let hints: Vec<(String, String)> = Vec::new();
        let mut frame = Frame::signal(
            "/org/freedesktop/portal/inputmethod",
            "org.fcitx.Fcitx.InputMethod1",
            "CreateInputContext",
        )
        .serial(serial)
        .destination("org.fcitx.Fcitx5")
        .sender(":1.42")
        .body(&hints)
        .build();
        frame.kind = Kind::MethodCall;
        frame.encode()
    }

    fn empty_request(member: &str, path: &str, serial: u32) -> Vec<u8> {
        let mut frame = Frame::signal(path, "org.fcitx.Fcitx.InputContext1", member)
            .serial(serial)
            .destination("org.fcitx.Fcitx5")
            .sender(":1.42")
            .build();
        frame.kind = Kind::MethodCall;
        frame.encode()
    }

    #[test]
    fn create_input_context_returns_oay_with_swapped_endpoints() {
        let bytes = create_input_context_request(42);
        let request = Frame::parse(&bytes).unwrap();
        let mut reg = IcRegistry::new();
        let mut serial = 0;
        let reply_bytes = build_reply(
            &request,
            &FcitxMethod::CreateInputContext { hints: vec![] },
            &mut reg,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.kind, Kind::MethodReturn);
        assert_eq!(reply.fields.reply_serial, Some(42));
        // sender/destination swapped — reply originates from the bus the
        // request was destined for.
        assert_eq!(reply.fields.destination.as_deref(), Some(":1.42"));
        assert_eq!(reply.fields.sender.as_deref(), Some("org.fcitx.Fcitx5"));
        assert_eq!(reply.fields.signature.as_deref(), Some("oay"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn focus_in_updates_registry_and_returns_empty() {
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        let bytes = empty_request("FocusIn", &path, 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut serial = 0;
        let reply_bytes = build_reply(
            &request,
            &FcitxMethod::FocusIn {
                ic_path: path.clone(),
            },
            &mut reg,
            &mut serial,
        );
        let reply = Frame::parse(&reply_bytes).unwrap();
        assert_eq!(reply.kind, Kind::MethodReturn);
        assert_eq!(reply.body.len(), 0);
        assert!(reg.get(&path).unwrap().focused);
    }

    #[test]
    fn focus_out_clears_focused_flag() {
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        reg.get_mut(&path).unwrap().focused = true;
        let bytes = empty_request("FocusOut", &path, 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut serial = 0;
        build_reply(
            &request,
            &FcitxMethod::FocusOut {
                ic_path: path.clone(),
            },
            &mut reg,
            &mut serial,
        );
        assert!(!reg.get(&path).unwrap().focused);
    }

    #[test]
    fn destroy_ic_removes_from_registry() {
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        let bytes = empty_request("DestroyIC", &path, 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut serial = 0;
        build_reply(
            &request,
            &FcitxMethod::DestroyIC { ic_path: path },
            &mut reg,
            &mut serial,
        );
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn set_cursor_rect_v2_stores_rect() {
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        let bytes = empty_request("SetCursorRectV2", &path, 1);
        let request = Frame::parse(&bytes).unwrap();
        let mut serial = 0;
        build_reply(
            &request,
            &FcitxMethod::SetCursorRectV2 {
                ic_path: path.clone(),
                x: 100,
                y: 200,
                w: 10,
                h: 20,
                scale: 1.0,
            },
            &mut reg,
            &mut serial,
        );
        assert_eq!(reg.get(&path).unwrap().cursor_rect, Some([100, 200, 10, 20]));
    }

    #[test]
    fn serial_counter_skips_zero_on_wrap() {
        let mut c: u32 = u32::MAX;
        assert_eq!(next_nonzero(&mut c), 1);
        assert_eq!(c, 1);
    }

    #[test]
    fn serial_counter_increments_normally() {
        let mut c: u32 = 41;
        assert_eq!(next_nonzero(&mut c), 42);
        assert_eq!(next_nonzero(&mut c), 43);
    }
}
