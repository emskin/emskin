//! Turn a parsed [`Frame`] into a typed [`FcitxMethod`].
//!
//! The classifier matches on `interface` + `member` + body `signature`
//! from `frame.fields`, then decodes the body via [`Frame::decode_body`]
//! into the per-method args. Bodies whose signatures we don't recognize
//! return `None` and the broker forwards the message verbatim.

use crate::dbus::frame::Frame;

use super::{INPUT_CONTEXT_IFACE, INPUT_CONTEXT_IFACE_FCITX4, INPUT_METHOD_IFACE};

/// A recognized fcitx5 method_call with its args extracted. `ic_path`
/// fields are the request's `path` (object path of the IC). Methods on
/// `InputMethod1` don't carry an IC.
#[derive(Debug, Clone, PartialEq)]
pub enum FcitxMethod {
    /// `InputMethod1.CreateInputContext(a(ss)) → (o, ay)`.
    CreateInputContext { hints: Vec<(String, String)> },

    /// `InputContext1.FocusIn()`. Empty body.
    FocusIn { ic_path: String },
    /// `InputContext1.FocusOut()`. Empty body.
    FocusOut { ic_path: String },
    /// `InputContext1.Reset()`. Empty body.
    Reset { ic_path: String },
    /// `InputContext1.DestroyIC()`. Empty body.
    DestroyIC { ic_path: String },

    /// `InputContext1.SetCapability(t)`.
    SetCapability { ic_path: String, capability: u64 },

    /// `InputContext1.SetCursorRect(iiii)`.
    SetCursorRect {
        ic_path: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    /// `InputContext1.SetCursorRectV2(iiiid)`. Scale kept verbatim for
    /// HiDPI-aware callers.
    SetCursorRectV2 {
        ic_path: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        scale: f64,
    },
    /// fcitx4: `InputContext.SetCursorLocation(ii)`.
    SetCursorLocation { ic_path: String, x: i32, y: i32 },

    /// `InputContext1.SetSurroundingText(suu)`.
    SetSurroundingText {
        ic_path: String,
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// `InputContext1.SetSurroundingTextPosition(uu)`.
    SetSurroundingTextPosition {
        ic_path: String,
        cursor: u32,
        anchor: u32,
    },
}

/// Classify one method_call. Returns `None` for unrelated DBus traffic,
/// known interfaces with unrecognized members, the right member with
/// the wrong body signature, or bodies that don't decode cleanly.
pub fn classify(frame: &Frame<'_>) -> Option<FcitxMethod> {
    let iface = frame.fields.interface.as_deref()?;
    let member = frame.fields.member.as_deref()?;
    let sig = frame.fields.signature.as_deref().unwrap_or("");
    let path = frame.fields.path.clone();

    match (iface, member, sig) {
        (INPUT_METHOD_IFACE, "CreateInputContext", "a(ss)") => {
            let hints: Vec<(String, String)> = frame.decode_body()?;
            Some(FcitxMethod::CreateInputContext { hints })
        }
        (INPUT_CONTEXT_IFACE, "FocusIn", "") => Some(FcitxMethod::FocusIn { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "FocusOut", "") => Some(FcitxMethod::FocusOut { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "Reset", "") => Some(FcitxMethod::Reset { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "DestroyIC", "") => Some(FcitxMethod::DestroyIC { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "SetCapability", "t") => {
            let capability: u64 = frame.decode_body()?;
            Some(FcitxMethod::SetCapability {
                ic_path: path?,
                capability,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetCursorRect", "iiii") => {
            let (x, y, w, h): (i32, i32, i32, i32) = frame.decode_body()?;
            Some(FcitxMethod::SetCursorRect {
                ic_path: path?,
                x,
                y,
                w,
                h,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetCursorRectV2", "iiiid") => {
            let (x, y, w, h, scale): (i32, i32, i32, i32, f64) = frame.decode_body()?;
            Some(FcitxMethod::SetCursorRectV2 {
                ic_path: path?,
                x,
                y,
                w,
                h,
                scale,
            })
        }
        (INPUT_CONTEXT_IFACE_FCITX4, "SetCursorLocation", "ii") => {
            let (x, y): (i32, i32) = frame.decode_body()?;
            Some(FcitxMethod::SetCursorLocation {
                ic_path: path?,
                x,
                y,
            })
        }
        // NOTE: ProcessKeyEvent is intentionally **not** classified here.
        // It must be forwarded to the upstream real fcitx5 — the broker
        // models IC state (FocusIn/Out, SetCursorRect, …) but key events
        // need real IM logic, otherwise GTK IM clients lose Chinese
        // input whenever the host fcitx5 isn't grabbing the keyboard
        // (English mode, hotkey passthrough, etc.). Classifying it would
        // make the broker `reply false` and cut off the only path real
        // fcitx5 has to receive that key, with no compensating route.
        // (The earlier "intercept + reply false" was a side-effect of
        // a signature typo — `"uubuu"` vs the spec's `"uuubu"` — that
        // accidentally made this branch a no-op in production.)
        (INPUT_CONTEXT_IFACE, "SetSurroundingText", "suu") => {
            let (text, cursor, anchor): (String, u32, u32) = frame.decode_body()?;
            Some(FcitxMethod::SetSurroundingText {
                ic_path: path?,
                text,
                cursor,
                anchor,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetSurroundingTextPosition", "uu") => {
            let (cursor, anchor): (u32, u32) = frame.decode_body()?;
            Some(FcitxMethod::SetSurroundingTextPosition {
                ic_path: path?,
                cursor,
                anchor,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::frame::Kind;

    /// Build a method_call frame for testing. Builder targets
    /// method_return / signal / error directly; here we synthesize a
    /// signal-shaped frame and flip `kind` to MethodCall.
    fn method_call<T>(iface: &str, member: &str, path: &str, body: &T) -> Vec<u8>
    where
        T: serde::Serialize + zvariant::Type,
    {
        let mut frame = Frame::signal(path, iface, member)
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .body(body)
            .build();
        frame.kind = Kind::MethodCall;
        frame.encode()
    }

    fn method_call_empty(iface: &str, member: &str, path: &str) -> Vec<u8> {
        let mut frame = Frame::signal(path, iface, member)
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .build();
        frame.kind = Kind::MethodCall;
        frame.encode()
    }

    #[test]
    fn classifies_focus_in() {
        let bytes = method_call_empty(INPUT_CONTEXT_IFACE, "FocusIn", "/ic/7");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::FocusIn {
                ic_path: "/ic/7".into()
            })
        );
    }

    #[test]
    fn classifies_focus_out() {
        let bytes = method_call_empty(INPUT_CONTEXT_IFACE, "FocusOut", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::FocusOut {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_reset() {
        let bytes = method_call_empty(INPUT_CONTEXT_IFACE, "Reset", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::Reset {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_destroy_ic() {
        let bytes = method_call_empty(INPUT_CONTEXT_IFACE, "DestroyIC", "/ic/1");
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::DestroyIC {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_set_cursor_rect() {
        let bytes = method_call(
            INPUT_CONTEXT_IFACE,
            "SetCursorRect",
            "/ic/7",
            &(100i32, 200i32, 10i32, 20i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::SetCursorRect {
                ic_path: "/ic/7".into(),
                x: 100,
                y: 200,
                w: 10,
                h: 20,
            })
        );
    }

    #[test]
    fn classifies_set_cursor_rect_v2() {
        let bytes = method_call(
            INPUT_CONTEXT_IFACE,
            "SetCursorRectV2",
            "/ic/7",
            &(10i32, 20i32, 30i32, 40i32, 1.25f64),
        );
        let frame = Frame::parse(&bytes).unwrap();
        let Some(FcitxMethod::SetCursorRectV2 {
            ic_path,
            x,
            y,
            w,
            h,
            scale,
        }) = classify(&frame)
        else {
            panic!("not V2");
        };
        assert_eq!(ic_path, "/ic/7");
        assert_eq!((x, y, w, h), (10, 20, 30, 40));
        assert_eq!(scale, 1.25);
    }

    #[test]
    fn classifies_fcitx4_set_cursor_location() {
        let bytes = method_call(
            INPUT_CONTEXT_IFACE_FCITX4,
            "SetCursorLocation",
            "/ic/7",
            &(50i32, 60i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::SetCursorLocation {
                ic_path: "/ic/7".into(),
                x: 50,
                y: 60,
            })
        );
    }

    #[test]
    fn process_key_event_is_not_classified() {
        // ProcessKeyEvent must be forwarded to upstream real fcitx5,
        // not intercepted — see classify.rs comment.
        let mut frame = Frame::signal("/ic/7", INPUT_CONTEXT_IFACE, "ProcessKeyEvent")
            .serial(1)
            .destination("org.fcitx.Fcitx5")
            .body_args()
            .arg(&0x61u32)
            .arg(&38u32)
            .arg(&0u32)
            .arg(&false)
            .arg(&1234u32)
            .done()
            .build();
        frame.kind = Kind::MethodCall;
        let bytes = frame.encode();
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }

    #[test]
    fn classifies_set_capability() {
        let bytes = method_call(
            INPUT_CONTEXT_IFACE,
            "SetCapability",
            "/ic/7",
            &0xDEADBEEFu64,
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::SetCapability {
                ic_path: "/ic/7".into(),
                capability: 0xDEADBEEF,
            })
        );
    }

    #[test]
    fn classifies_create_input_context_empty() {
        let hints: Vec<(String, String)> = Vec::new();
        let bytes = method_call(INPUT_METHOD_IFACE, "CreateInputContext", "/im", &hints);
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::CreateInputContext { hints: vec![] })
        );
    }

    #[test]
    fn classifies_create_input_context_with_one_hint() {
        let hints: Vec<(String, String)> = vec![("program".into(), "wechat".into())];
        let bytes = method_call(INPUT_METHOD_IFACE, "CreateInputContext", "/im", &hints);
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(
            classify(&frame),
            Some(FcitxMethod::CreateInputContext {
                hints: vec![("program".into(), "wechat".into())],
            })
        );
    }

    #[test]
    fn wrong_signature_is_not_classified() {
        // SetCursorRect declares "iiii" — frame body with declared
        // signature "ii" must be rejected.
        let mut frame = Frame::signal("/ic/7", INPUT_CONTEXT_IFACE, "SetCursorRect")
            .serial(1)
            .body(&(10i32, 20i32))
            .build();
        frame.kind = Kind::MethodCall;
        let bytes = frame.encode();
        let parsed = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&parsed), None);
    }

    #[test]
    fn unrelated_interface_is_not_classified() {
        let bytes = method_call_empty(
            "org.freedesktop.DBus",
            "Hello",
            "/org/freedesktop/DBus",
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }

    #[test]
    fn unknown_member_on_known_iface_is_not_classified() {
        let bytes = method_call(
            INPUT_CONTEXT_IFACE,
            "MysterySettings",
            "/ic/7",
            &(0i32, 0i32, 0i32),
        );
        let frame = Frame::parse(&bytes).unwrap();
        assert_eq!(classify(&frame), None);
    }
}
