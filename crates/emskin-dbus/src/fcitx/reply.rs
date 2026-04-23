//! Reply synthesis for intercepted fcitx5 method_calls.
//!
//! Given a parsed request header + classified [`FcitxMethod`], build
//! the bytes the broker should write back to the client. The broker
//! doesn't forward the request to the real fcitx5 once we're in this
//! code path.

use crate::dbus::encode::{body_bool, body_empty, body_oay, MethodReturn};
use crate::dbus::message::Header;

use super::classify::FcitxMethod;
use super::ic::IcRegistry;

/// Mint a synthetic method_return for `method`.
///
/// Mutates `registry` for `CreateInputContext` (allocates a new IC)
/// and `DestroyIC` (frees one). All other variants are read-only.
///
/// `serial_counter` is the broker's per-connection outgoing serial
/// counter — incremented for every reply + signal we send on that
/// connection. The DBus spec requires non-zero serials so callers
/// should initialize it to 1.
pub fn build_reply(
    request: &Header,
    method: &FcitxMethod,
    registry: &mut IcRegistry,
    serial_counter: &mut u32,
) -> Vec<u8> {
    let our_serial = next_nonzero(serial_counter);
    // Where should the reply say it came *from*? The request's
    // `destination` is the fcitx5 well-known or unique name — echoing
    // it back as `sender` keeps clients that filter by sender happy.
    let sender = request.destination.as_deref();
    // Where does the reply go? The request's `sender` (the client's
    // unique bus name, e.g. ":1.42"). For clients that dialed via the
    // broker directly (no unique name yet), this is `None` — DBus
    // clients match on reply_serial in that case.
    let destination = request.sender.as_deref();
    let reply_to_serial = request.serial;

    match method {
        FcitxMethod::CreateInputContext { .. } => {
            let (path, state) = registry.allocate();
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_oay(&path, &state.uuid),
            }
            .encode()
        }
        FcitxMethod::ProcessKeyEvent { .. } => MethodReturn {
            our_serial,
            reply_to_serial,
            destination,
            sender,
            // `false` — key not consumed by (fake) fcitx5. The real
            // host fcitx5 consumes IME keys via emskin's winit IC
            // before they reach WeChat; anything that does reach
            // WeChat and flows back as ProcessKeyEvent is by
            // definition a non-IME key, which should pass through to
            // the client widget.
            body: body_bool(false),
        }
        .encode(),
        FcitxMethod::DestroyIC { ic_path } => {
            registry.destroy(ic_path);
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_empty(),
            }
            .encode()
        }
        FcitxMethod::FocusIn { ic_path } | FcitxMethod::FocusOut { ic_path } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.focused = matches!(method, FcitxMethod::FocusIn { .. });
            }
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_empty(),
            }
            .encode()
        }
        FcitxMethod::SetCapability {
            ic_path,
            capability,
        } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.capability = *capability;
            }
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_empty(),
            }
            .encode()
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
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_empty(),
            }
            .encode()
        }
        FcitxMethod::SetCursorLocation { ic_path, x, y } => {
            if let Some(st) = registry.get_mut(ic_path) {
                st.cursor_rect = Some([*x, *y, 0, 0]);
            }
            MethodReturn {
                our_serial,
                reply_to_serial,
                destination,
                sender,
                body: body_empty(),
            }
            .encode()
        }
        FcitxMethod::Reset { .. }
        | FcitxMethod::SetSurroundingText { .. }
        | FcitxMethod::SetSurroundingTextPosition { .. } => MethodReturn {
            our_serial,
            reply_to_serial,
            destination,
            sender,
            body: body_empty(),
        }
        .encode(),
    }
}

/// Increment `counter`, skipping zero (DBus spec requires non-zero
/// serials). Wraps around `u32::MAX` → 1 to stay positive.
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
    use crate::dbus::message::{parse_header, Endian, MessageType};

    fn request(
        member: &str,
        sig: &str,
        path: &str,
        serial: u32,
        destination: &str,
        sender: &str,
    ) -> Header {
        Header {
            endian: Endian::Little,
            msg_type: MessageType::MethodCall,
            flags: 0,
            body_len: 0,
            serial,
            path: Some(path.into()),
            interface: Some("org.fcitx.Fcitx.InputContext1".into()),
            member: Some(member.into()),
            error_name: None,
            destination: Some(destination.into()),
            sender: Some(sender.into()),
            signature: Some(sig.into()),
            reply_serial: None,
            unix_fds: None,
        }
    }

    #[test]
    fn create_input_context_allocates_and_returns_oay() {
        let req = request(
            "CreateInputContext",
            "a(ss)",
            "/org/freedesktop/portal/inputmethod",
            42,
            "org.fcitx.Fcitx5",
            ":1.100",
        );
        let mut reg = IcRegistry::new();
        let mut serial = 0;
        let bytes = build_reply(
            &req,
            &FcitxMethod::CreateInputContext { hints: vec![] },
            &mut reg,
            &mut serial,
        );
        let hdr = parse_header(&bytes).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.reply_serial, Some(42));
        assert_eq!(hdr.destination.as_deref(), Some(":1.100"));
        assert_eq!(hdr.sender.as_deref(), Some("org.fcitx.Fcitx5"));
        assert_eq!(hdr.signature.as_deref(), Some("oay"));
        // And the registry should now hold the IC.
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn process_key_event_returns_false() {
        let req = request(
            "ProcessKeyEvent",
            "uubuu",
            "/org/freedesktop/portal/inputcontext/1",
            7,
            "org.fcitx.Fcitx5",
            ":1.42",
        );
        let mut reg = IcRegistry::new();
        reg.allocate(); // so the IC exists
        let mut serial = 0;
        let bytes = build_reply(
            &req,
            &FcitxMethod::ProcessKeyEvent {
                ic_path: "/org/freedesktop/portal/inputcontext/1".into(),
                keyval: 0x61,
                keycode: 38,
                state: 0,
                is_release: false,
                time: 0,
            },
            &mut reg,
            &mut serial,
        );
        let hdr = parse_header(&bytes).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.signature.as_deref(), Some("b"));
        assert_eq!(hdr.body_len, 4);
        // Body: u32 LE, `false` = 0
        let body_start = bytes.len() - 4;
        assert_eq!(&bytes[body_start..], &[0, 0, 0, 0]);
    }

    #[test]
    fn focus_in_updates_registry_and_returns_empty() {
        let req = request(
            "FocusIn",
            "",
            "/ic/1",
            1,
            "org.fcitx.Fcitx5",
            ":1.42",
        );
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        let mut serial = 0;
        let bytes = build_reply(
            &req,
            &FcitxMethod::FocusIn {
                ic_path: path.clone(),
            },
            &mut reg,
            &mut serial,
        );
        let hdr = parse_header(&bytes).unwrap();
        assert_eq!(hdr.msg_type, MessageType::MethodReturn);
        assert_eq!(hdr.body_len, 0);
        assert!(reg.get(&path).unwrap().focused);
    }

    #[test]
    fn focus_out_clears_focused_flag() {
        let req = request("FocusOut", "", "/ic/1", 1, "org.fcitx.Fcitx5", ":1.42");
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        reg.get_mut(&path).unwrap().focused = true;
        let mut serial = 0;
        build_reply(
            &req,
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
        let req = request("DestroyIC", "", "/ic/1", 1, "org.fcitx.Fcitx5", ":1.42");
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        assert_eq!(reg.len(), 1);
        let mut serial = 0;
        build_reply(
            &req,
            &FcitxMethod::DestroyIC { ic_path: path },
            &mut reg,
            &mut serial,
        );
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn set_cursor_rect_v2_stores_rect_and_returns_empty() {
        let req = request(
            "SetCursorRectV2",
            "iiiid",
            "/ic/1",
            1,
            "org.fcitx.Fcitx5",
            ":1.42",
        );
        let mut reg = IcRegistry::new();
        let (path, _) = reg.allocate();
        let mut serial = 0;
        let bytes = build_reply(
            &req,
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
        let hdr = parse_header(&bytes).unwrap();
        assert_eq!(hdr.body_len, 0);
        assert_eq!(reg.get(&path).unwrap().cursor_rect, Some([100, 200, 10, 20]));
    }

    #[test]
    fn serial_counter_skips_zero_on_wrap() {
        let mut c: u32 = u32::MAX;
        let s = next_nonzero(&mut c);
        assert_eq!(s, 1);
        assert_eq!(c, 1);
    }

    #[test]
    fn serial_counter_increments_normally() {
        let mut c: u32 = 41;
        assert_eq!(next_nonzero(&mut c), 42);
        assert_eq!(next_nonzero(&mut c), 43);
    }
}
