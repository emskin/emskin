use serde::{Deserialize, Serialize};

/// Emacs → eafvil
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IncomingMessage {
    SetGeometry {
        window_id: u64,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    },
    Close {
        window_id: u64,
    },
    SetVisibility {
        window_id: u64,
        visible: bool,
    },
    ForwardKey {
        window_id: u64,
        keycode: u32,
        state: u32,
        modifiers: u32,
    },
}

/// eafvil → Emacs
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutgoingMessage {
    Connected { version: &'static str },
    Error { msg: String },
    WindowCreated { window_id: u64, title: String },
    WindowDestroyed { window_id: u64 },
    TitleChanged { window_id: u64, title: String },
}
