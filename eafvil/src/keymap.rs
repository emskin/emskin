//! Read the host compositor's keyboard keymap via the Wayland protocol.
//!
//! Connects as a temporary wl_client, receives the `wl_keyboard.keymap` event,
//! and returns the XKB keymap string so eafvil can configure its own seat
//! with the same layout as the host.

use std::io::Read;

use wayland_client::{
    protocol::{wl_keyboard, wl_registry, wl_seat},
    Connection, Dispatch, QueueHandle, WEnum,
};

struct KeymapReader {
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for KeymapReader {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_seat" && state.seat.is_none() {
                state.seat = Some(registry.bind(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for KeymapReader {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for KeymapReader {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            if format == WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                const MAX_KEYMAP_SIZE: usize = 1024 * 1024; // 1 MB
                let size = size as usize;
                if size == 0 || size > MAX_KEYMAP_SIZE {
                    return;
                }
                let mut file = std::fs::File::from(fd);
                let mut buf = vec![0u8; size];
                if file.read_exact(&mut buf).is_ok() {
                    // XKB keymap strings are null-terminated
                    let keymap = String::from_utf8_lossy(&buf)
                        .trim_end_matches('\0')
                        .to_string();
                    state.keymap = Some(keymap);
                }
            }
        }
    }
}

/// Connect to the host Wayland compositor and read its keyboard keymap.
///
/// Returns `None` if the connection fails or no keyboard is available.
pub fn read_host_keymap() -> Option<String> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue::<KeymapReader>();
    let qh = queue.handle();

    let _registry = conn.display().get_registry(&qh, ());

    let mut reader = KeymapReader {
        seat: None,
        keyboard: None,
        keymap: None,
    };

    // Roundtrip 1: discover globals → bind wl_seat
    queue.roundtrip(&mut reader).ok()?;
    // Roundtrip 2: seat capabilities → get_keyboard
    queue.roundtrip(&mut reader).ok()?;
    // Roundtrip 3: receive keymap event
    queue.roundtrip(&mut reader).ok()?;

    // Clean up: release keyboard and seat
    if let Some(kb) = reader.keyboard.take() {
        kb.release();
    }
    if let Some(seat) = reader.seat.take() {
        seat.release();
    }

    reader.keymap
}
