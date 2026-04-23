//! Turn a parsed `Header` + its body bytes into a typed [`FcitxMethod`].
//!
//! The rules:
//!
//! - Match `interface` + `member` against the fcitx5 wire names.
//! - Check `signature` matches the expected one for that method; a
//!   mismatch returns `None` so we never mis-interpret a body.
//! - Parse just enough of the body to extract the args the broker
//!   needs. Bodies whose signatures we don't consume (`SetCapability`'s
//!   `t`, `ProcessKeyEvent`'s 5-arg tuple, etc.) still parse out their
//!   values — callers downstream rely on the typed args.

use crate::dbus::message::{Endian, Header};

use super::{INPUT_CONTEXT_IFACE, INPUT_CONTEXT_IFACE_FCITX4, INPUT_METHOD_IFACE};

/// A recognized fcitx5 method_call with its args extracted. `ic_path`
/// fields are the header's `path` (object path of the IC). Methods on
/// `InputMethod1` don't carry an IC.
#[derive(Debug, Clone, PartialEq)]
pub enum FcitxMethod {
    /// `InputMethod1.CreateInputContext(a(ss)) → (o, ay)`.
    /// The `a(ss)` arg carries `{name: value}` pairs (program name,
    /// display, etc.) that fcitx5 uses for IM selection. We don't use
    /// them in phase-1 but still parse so future policy hooks can.
    CreateInputContext { hints: Vec<(String, String)> },

    /// `InputContext1.FocusIn()`. Empty body.
    FocusIn { ic_path: String },
    /// `InputContext1.FocusOut()`. Empty body.
    FocusOut { ic_path: String },
    /// `InputContext1.Reset()`. Empty body.
    Reset { ic_path: String },
    /// `InputContext1.DestroyIC()`. Empty body.
    DestroyIC { ic_path: String },

    /// `InputContext1.SetCapability(t)`. Single `u64` of IC flags.
    SetCapability { ic_path: String, capability: u64 },

    /// fcitx5 legacy: `InputContext1.SetCursorRect(iiii)`.
    SetCursorRect {
        ic_path: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    /// fcitx5 modern: `InputContext1.SetCursorRectV2(iiiid)`. Scale is
    /// kept verbatim; the broker rewrites only `(x, y)`.
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

    /// `InputContext1.ProcessKeyEvent(uubuu) → b`. Args are
    /// `(keyval, keycode, state, is_release, time)`. The body uses
    /// fcitx5's historical signature `uubuu` where `b` is a bool sent
    /// as `u32` (0/1) — matches any other bool in DBus bodies.
    ProcessKeyEvent {
        ic_path: String,
        keyval: u32,
        keycode: u32,
        state: u32,
        is_release: bool,
        time: u32,
    },

    /// `InputContext1.SetSurroundingText(suu)`. We don't use the text
    /// yet but decode it so future surrounding-text-aware handlers
    /// don't have to re-parse.
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

/// Classify a single method_call against the fcitx5 interface tables.
/// Returns `None` for anything else — unrelated DBus traffic, known
/// interfaces but unrecognized members, or the right member with the
/// wrong signature.
pub fn classify(header: &Header, body: &[u8]) -> Option<FcitxMethod> {
    let iface = header.interface.as_deref()?;
    let member = header.member.as_deref()?;
    let sig = header.signature.as_deref().unwrap_or("");
    let path = header.path.clone();
    let endian = header.endian;

    match (iface, member, sig) {
        (INPUT_METHOD_IFACE, "CreateInputContext", "a(ss)") => {
            let hints = parse_array_of_ss(body, endian)?;
            Some(FcitxMethod::CreateInputContext { hints })
        }
        (INPUT_CONTEXT_IFACE, "FocusIn", "") => Some(FcitxMethod::FocusIn { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "FocusOut", "") => Some(FcitxMethod::FocusOut { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "Reset", "") => Some(FcitxMethod::Reset { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "DestroyIC", "") => Some(FcitxMethod::DestroyIC { ic_path: path? }),
        (INPUT_CONTEXT_IFACE, "SetCapability", "t") => {
            let capability = read_u64(body, endian)?;
            Some(FcitxMethod::SetCapability {
                ic_path: path?,
                capability,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetCursorRect", "iiii") => {
            let (x, y, w, h) = read_four_i32(body, endian)?;
            Some(FcitxMethod::SetCursorRect {
                ic_path: path?,
                x,
                y,
                w,
                h,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetCursorRectV2", "iiiid") => {
            let (x, y, w, h) = read_four_i32(body, endian)?;
            // double at offset 16 (already aligned to 8)
            if body.len() < 24 {
                return None;
            }
            let arr: [u8; 8] = body[16..24].try_into().ok()?;
            let scale = match endian {
                Endian::Little => f64::from_le_bytes(arr),
                Endian::Big => f64::from_be_bytes(arr),
            };
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
            if body.len() < 8 {
                return None;
            }
            let x = read_i32_at(body, 0, endian);
            let y = read_i32_at(body, 4, endian);
            Some(FcitxMethod::SetCursorLocation {
                ic_path: path?,
                x,
                y,
            })
        }
        (INPUT_CONTEXT_IFACE, "ProcessKeyEvent", "uubuu") => {
            if body.len() < 20 {
                return None;
            }
            Some(FcitxMethod::ProcessKeyEvent {
                ic_path: path?,
                keyval: read_u32_at(body, 0, endian),
                keycode: read_u32_at(body, 4, endian),
                state: read_u32_at(body, 8, endian),
                is_release: read_u32_at(body, 12, endian) != 0,
                time: read_u32_at(body, 16, endian),
            })
        }
        (INPUT_CONTEXT_IFACE, "SetSurroundingText", "suu") => {
            let mut off = 0usize;
            let text = read_string(body, &mut off, endian)?;
            // Strings end with NUL; next u32 starts on 4-byte align.
            off = align_to(off, 4);
            if body.len() < off + 8 {
                return None;
            }
            let cursor = read_u32_at(body, off, endian);
            let anchor = read_u32_at(body, off + 4, endian);
            Some(FcitxMethod::SetSurroundingText {
                ic_path: path?,
                text,
                cursor,
                anchor,
            })
        }
        (INPUT_CONTEXT_IFACE, "SetSurroundingTextPosition", "uu") => {
            if body.len() < 8 {
                return None;
            }
            Some(FcitxMethod::SetSurroundingTextPosition {
                ic_path: path?,
                cursor: read_u32_at(body, 0, endian),
                anchor: read_u32_at(body, 4, endian),
            })
        }
        _ => None,
    }
}

// ---------- body parse helpers (minimal, internal) ----------

fn align_to(n: usize, bound: usize) -> usize {
    (n + bound - 1) & !(bound - 1)
}

fn read_u32_at(buf: &[u8], off: usize, endian: Endian) -> u32 {
    let arr: [u8; 4] = buf[off..off + 4].try_into().expect("caller checked bounds");
    match endian {
        Endian::Little => u32::from_le_bytes(arr),
        Endian::Big => u32::from_be_bytes(arr),
    }
}

fn read_i32_at(buf: &[u8], off: usize, endian: Endian) -> i32 {
    read_u32_at(buf, off, endian) as i32
}

fn read_u64(body: &[u8], endian: Endian) -> Option<u64> {
    if body.len() < 8 {
        return None;
    }
    let arr: [u8; 8] = body[..8].try_into().ok()?;
    Some(match endian {
        Endian::Little => u64::from_le_bytes(arr),
        Endian::Big => u64::from_be_bytes(arr),
    })
}

fn read_four_i32(body: &[u8], endian: Endian) -> Option<(i32, i32, i32, i32)> {
    if body.len() < 16 {
        return None;
    }
    Some((
        read_i32_at(body, 0, endian),
        read_i32_at(body, 4, endian),
        read_i32_at(body, 8, endian),
        read_i32_at(body, 12, endian),
    ))
}

/// Read a DBus `s` (string): u32 length + UTF-8 bytes + NUL
/// terminator. Advances `off` past the NUL.
fn read_string(body: &[u8], off: &mut usize, endian: Endian) -> Option<String> {
    *off = align_to(*off, 4);
    if body.len() < *off + 4 {
        return None;
    }
    let len = read_u32_at(body, *off, endian) as usize;
    *off += 4;
    if body.len() < *off + len + 1 {
        return None;
    }
    let s = std::str::from_utf8(&body[*off..*off + len]).ok()?.to_string();
    *off += len + 1; // + NUL
    Some(s)
}

/// Read a DBus `a(ss)`: u32 array length (in bytes), then packed
/// structs of (string, string). Each struct starts 8-aligned. Empty
/// arrays skip the alignment padding (no elements to align to).
fn parse_array_of_ss(body: &[u8], endian: Endian) -> Option<Vec<(String, String)>> {
    if body.len() < 4 {
        return None;
    }
    let array_bytes = read_u32_at(body, 0, endian) as usize;
    if array_bytes == 0 {
        return Some(Vec::new());
    }
    // Array contents start at 4 (after length); struct alignment is 8,
    // so first struct starts at the next 8-aligned offset >= 4 → 8.
    let start = align_to(4, 8);
    let end = start.checked_add(array_bytes)?;
    if body.len() < end {
        return None;
    }

    let mut items = Vec::new();
    let mut off = start;
    while off < end {
        off = align_to(off, 8);
        if off >= end {
            break;
        }
        let k = read_string(body, &mut off, endian)?;
        let v = read_string(body, &mut off, endian)?;
        items.push((k, v));
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbus::message::MessageType;

    fn hdr(
        iface: &str,
        member: &str,
        sig: Option<&str>,
        path: Option<&str>,
        body_len: u32,
    ) -> Header {
        Header {
            endian: Endian::Little,
            msg_type: MessageType::MethodCall,
            flags: 0,
            body_len,
            serial: 1,
            path: path.map(String::from),
            interface: Some(iface.into()),
            member: Some(member.into()),
            error_name: None,
            destination: None,
            sender: None,
            signature: sig.map(String::from),
            reply_serial: None,
            unix_fds: None,
        }
    }

    #[test]
    fn classifies_focus_in() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "FocusIn",
            Some(""),
            Some("/ic/7"),
            0,
        );
        assert_eq!(
            classify(&h, &[]),
            Some(FcitxMethod::FocusIn {
                ic_path: "/ic/7".into()
            })
        );
    }

    #[test]
    fn classifies_focus_out() {
        let h = hdr(INPUT_CONTEXT_IFACE, "FocusOut", Some(""), Some("/ic/1"), 0);
        assert_eq!(
            classify(&h, &[]),
            Some(FcitxMethod::FocusOut {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_reset() {
        let h = hdr(INPUT_CONTEXT_IFACE, "Reset", Some(""), Some("/ic/1"), 0);
        assert_eq!(
            classify(&h, &[]),
            Some(FcitxMethod::Reset {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_destroy_ic() {
        let h = hdr(INPUT_CONTEXT_IFACE, "DestroyIC", Some(""), Some("/ic/1"), 0);
        assert_eq!(
            classify(&h, &[]),
            Some(FcitxMethod::DestroyIC {
                ic_path: "/ic/1".into()
            })
        );
    }

    #[test]
    fn classifies_set_cursor_rect_and_parses_body() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "SetCursorRect",
            Some("iiii"),
            Some("/ic/7"),
            16,
        );
        let mut body = Vec::new();
        body.extend_from_slice(&100i32.to_le_bytes());
        body.extend_from_slice(&200i32.to_le_bytes());
        body.extend_from_slice(&10i32.to_le_bytes());
        body.extend_from_slice(&20i32.to_le_bytes());
        assert_eq!(
            classify(&h, &body),
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
    fn classifies_set_cursor_rect_v2_and_preserves_scale() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "SetCursorRectV2",
            Some("iiiid"),
            Some("/ic/7"),
            24,
        );
        let mut body = Vec::new();
        body.extend_from_slice(&10i32.to_le_bytes());
        body.extend_from_slice(&20i32.to_le_bytes());
        body.extend_from_slice(&30i32.to_le_bytes());
        body.extend_from_slice(&40i32.to_le_bytes());
        body.extend_from_slice(&1.25f64.to_le_bytes());
        let Some(FcitxMethod::SetCursorRectV2 {
            ic_path,
            x,
            y,
            w,
            h,
            scale,
        }) = classify(&h, &body)
        else {
            panic!("not V2");
        };
        assert_eq!(ic_path, "/ic/7");
        assert_eq!((x, y, w, h), (10, 20, 30, 40));
        assert_eq!(scale, 1.25);
    }

    #[test]
    fn classifies_fcitx4_set_cursor_location() {
        let h = hdr(
            INPUT_CONTEXT_IFACE_FCITX4,
            "SetCursorLocation",
            Some("ii"),
            Some("/ic/7"),
            8,
        );
        let mut body = Vec::new();
        body.extend_from_slice(&50i32.to_le_bytes());
        body.extend_from_slice(&60i32.to_le_bytes());
        assert_eq!(
            classify(&h, &body),
            Some(FcitxMethod::SetCursorLocation {
                ic_path: "/ic/7".into(),
                x: 50,
                y: 60,
            })
        );
    }

    #[test]
    fn classifies_process_key_event() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "ProcessKeyEvent",
            Some("uubuu"),
            Some("/ic/7"),
            20,
        );
        let mut body = Vec::new();
        body.extend_from_slice(&0x61u32.to_le_bytes()); // keyval 'a'
        body.extend_from_slice(&38u32.to_le_bytes()); // keycode
        body.extend_from_slice(&0u32.to_le_bytes()); // state
        body.extend_from_slice(&0u32.to_le_bytes()); // is_release=false
        body.extend_from_slice(&1234u32.to_le_bytes()); // time
        assert_eq!(
            classify(&h, &body),
            Some(FcitxMethod::ProcessKeyEvent {
                ic_path: "/ic/7".into(),
                keyval: 0x61,
                keycode: 38,
                state: 0,
                is_release: false,
                time: 1234,
            })
        );
    }

    #[test]
    fn classifies_set_capability() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "SetCapability",
            Some("t"),
            Some("/ic/7"),
            8,
        );
        let body = 0xDEADBEEFu64.to_le_bytes();
        assert_eq!(
            classify(&h, &body),
            Some(FcitxMethod::SetCapability {
                ic_path: "/ic/7".into(),
                capability: 0xDEADBEEF,
            })
        );
    }

    #[test]
    fn classifies_empty_create_input_context() {
        let h = hdr(
            INPUT_METHOD_IFACE,
            "CreateInputContext",
            Some("a(ss)"),
            Some("/im"),
            4,
        );
        // u32 array length = 0
        let body = 0u32.to_le_bytes();
        assert_eq!(
            classify(&h, &body),
            Some(FcitxMethod::CreateInputContext { hints: vec![] })
        );
    }

    #[test]
    fn classifies_create_input_context_with_one_hint() {
        // a(ss) with one (name="program", value="wechat")
        // Array body: (
        //   string "program": 4-byte length + 7 bytes + NUL = 12 bytes
        //   pad to 4: 0 bytes
        //   string "wechat": 4-byte length + 6 bytes + NUL = 11 bytes
        // ) = 23 bytes
        // Array starts at offset 4 (after u32 array-len), pad to 8 = offset 8
        let mut body = Vec::new();
        // We need to emit: u32 array_len, then padding so first struct at 8
        body.extend_from_slice(&0u32.to_le_bytes()); // placeholder for array_len
        while body.len() < 8 {
            body.push(0);
        }
        let struct_start = body.len();
        // "program"
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(b"program\0");
        // "wechat" — next string aligns to 4 for length prefix
        while body.len() % 4 != 0 {
            body.push(0);
        }
        body.extend_from_slice(&6u32.to_le_bytes());
        body.extend_from_slice(b"wechat\0");
        let struct_end = body.len();
        let array_bytes = (struct_end - struct_start) as u32;
        body[0..4].copy_from_slice(&array_bytes.to_le_bytes());

        let h = hdr(
            INPUT_METHOD_IFACE,
            "CreateInputContext",
            Some("a(ss)"),
            Some("/im"),
            body.len() as u32,
        );
        assert_eq!(
            classify(&h, &body),
            Some(FcitxMethod::CreateInputContext {
                hints: vec![("program".into(), "wechat".into())],
            })
        );
    }

    #[test]
    fn wrong_signature_is_not_classified() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "SetCursorRect",
            Some("ii"), // wrong — fcitx4 sig on fcitx5 iface
            Some("/ic/7"),
            8,
        );
        assert_eq!(classify(&h, &[0; 16]), None);
    }

    #[test]
    fn unrelated_interface_is_not_classified() {
        let h = hdr(
            "org.freedesktop.DBus",
            "Hello",
            Some(""),
            Some("/"),
            0,
        );
        assert_eq!(classify(&h, &[]), None);
    }

    #[test]
    fn unknown_member_on_known_iface_is_not_classified() {
        let h = hdr(
            INPUT_CONTEXT_IFACE,
            "MysterySettings",
            Some("iii"),
            Some("/ic/7"),
            12,
        );
        assert_eq!(classify(&h, &[0; 12]), None);
    }
}
