//! Effect trait and supporting types.
//!
//! An `Effect` is any component that produces render elements on top of the
//! scene each frame — pixel inspector, skeleton debug overlay, splash animation,
//! workspace bar, etc. Inspired by KWin's `Effect` API, adapted for emskin:
//!
//! - Single-crate (no dynamic loading / plugin ABI)
//! - Visual **and** optional input hooks on the same trait
//! - Screen-level only — no per-window effects
//!
//! Effects are registered with an [`EffectChain`](chain::EffectChain) at
//! compositor start-up and invoked every frame in `chain_position` order.

pub mod chain;

use std::time::Duration;

use smithay::{
    backend::{
        input::{ButtonState, KeyState, MouseButton},
        renderer::gles::GlesRenderer,
    },
    input::keyboard::{Keysym, ModifiersState},
    utils::{Logical, Point, Size},
};

use crate::ipc::IpcServer;
use crate::winit::CustomElement;

/// Read-only state a host exposes to effects each frame.
///
/// Built once per frame in `winit::render_frame` and passed into
/// `pre_paint`/`paint`. Only contains data overlays actually read — grows as
/// new effects need more.
pub struct EffectCtx {
    pub cursor_pos: Option<Point<f64, Logical>>,
    pub output_size: Size<i32, Logical>,
    pub scale: f64,
    pub emacs_connected: bool,
    pub active_workspace_id: u64,
    /// All workspaces as (id, name) in stable order. Used by
    /// [`WorkspaceBar`](crate::workspace_bar::WorkspaceBar) to keep its pill
    /// buttons in sync without a side-channel update call.
    pub workspaces: Vec<(u64, String)>,
    /// Monotonic time approximating when the frame will display (borrowed from
    /// KWin's `presentTime` concept). Used by animated effects so state stays
    /// correct even if a frame is delayed.
    pub present_time: Duration,
}

/// Pointer button event delivered to `Effect::handle_pointer_button`.
///
/// Not a type alias for smithay's `PointerButtonEvent` trait (which is
/// InputBackend-generic); this is a plain struct the host materialises.
pub struct PointerButtonEvent {
    pub button: MouseButton,
    pub state: ButtonState,
    pub pos: Point<f64, Logical>,
    pub time_ms: u32,
}

pub struct KeyEvent {
    pub keysym: Keysym,
    pub mods: ModifiersState,
    pub state: KeyState,
    pub time_ms: u32,
}

/// Outcome of an input handler. First `Consumed` in the chain wins.
#[derive(Debug, PartialEq, Eq)]
pub enum EventResult {
    Consumed,
    Pass,
}

/// State mutation an input handler wants the host to perform after the chain
/// finishes dispatch. Keeps effects away from `&mut EmskinState`.
pub enum EffectCommand {
    SwitchWorkspace(u64),
    RequestRedraw,
}

/// Context passed to input handlers. Gives direct IPC access (simple send-only
/// operations) plus a command queue for state mutations.
pub struct EffectInputCtx<'a> {
    pub ipc: &'a mut IpcServer,
    pub commands: &'a mut Vec<EffectCommand>,
}

/// The effect trait. All methods except `name` / `is_active` / `paint` have
/// sensible default implementations — a simple visual overlay only needs those
/// three.
pub trait Effect: Send {
    /// Stable identifier used for IPC routing (`set_measure`, `set_skeleton`)
    /// and debug logs.
    fn name(&self) -> &'static str;

    /// Per-frame filter. When `false`, the chain skips `pre_paint` / `paint` /
    /// `post_paint` for this frame. Mirrors KWin's `Effect::isActive`.
    fn is_active(&self) -> bool;

    /// 0..=100, higher = painted on top. Determines Vec ordering in the chain
    /// (higher first = topmost in `custom_elements` push order).
    fn chain_position(&self) -> u8 {
        50
    }

    /// Animation tick / state update before paint. Default: no-op.
    fn pre_paint(&mut self, _ctx: &EffectCtx) {}

    /// Produce this effect's render elements for the frame. Intra-effect
    /// z-order is the Vec order (index 0 = topmost within this effect).
    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>>;

    /// Post-paint housekeeping. Return `true` to request another frame
    /// (keeps animation running).
    fn post_paint(&mut self) -> bool {
        false
    }

    /// Called on every workspace switch. Effects that hold per-workspace state
    /// (e.g. skeleton rects) should clear/reset here.
    fn on_workspace_switch(&mut self) {}

    /// When `true`, the chain removes this effect after `post_paint`. Used by
    /// one-shot effects like `SplashScreen`.
    fn should_remove(&self) -> bool {
        false
    }

    /// IPC message dispatched to the effect matching `name()`. Payload is free-
    /// form JSON; effects parse what they need.
    fn handle_ipc(&mut self, _payload: &serde_json::Value) {}

    /// Pointer button hook. Default: pass. Return `Consumed` to stop the chain.
    fn handle_pointer_button(
        &mut self,
        _ev: &PointerButtonEvent,
        _ctx: &mut EffectInputCtx<'_>,
    ) -> EventResult {
        EventResult::Pass
    }

    /// Keyboard key hook. Default: pass.
    fn handle_key(&mut self, _ev: &KeyEvent, _ctx: &mut EffectInputCtx<'_>) -> EventResult {
        EventResult::Pass
    }
}
