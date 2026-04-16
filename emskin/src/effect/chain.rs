//! `EffectChain` — registry + dispatcher for registered effects.
//!
//! Chain order is determined by `Effect::chain_position()` (higher = topmost).
//! Per-frame steps:
//! 1. `pre_paint` — animation ticks, dismiss triggers, etc.
//! 2. `paint` — collect render elements in chain order (topmost first)
//! 3. `post_paint` — aggregate redraw requests, then cull `should_remove` effects

use smithay::backend::renderer::gles::GlesRenderer;

use super::{Effect, EffectCtx, EffectInputCtx, EventResult, KeyEvent, PointerButtonEvent};
use crate::winit::CustomElement;

#[derive(Default)]
pub struct EffectChain {
    effects: Vec<Box<dyn Effect>>,
}

impl EffectChain {
    pub fn register(&mut self, effect: Box<dyn Effect>) {
        self.effects.push(effect);
        // Sort by chain_position descending so Vec[0] = topmost. Stable sort
        // preserves registration order for equal positions.
        self.effects
            .sort_by_key(|e| std::cmp::Reverse(e.chain_position()));
    }

    pub fn pre_paint(&mut self, ctx: &EffectCtx) {
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            effect.pre_paint(ctx);
        }
    }

    pub fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>> {
        let mut out = Vec::new();
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            out.extend(effect.paint(renderer, ctx));
        }
        out
    }

    /// Returns `true` if any active effect requested another frame.
    pub fn post_paint(&mut self) -> bool {
        let mut want_redraw = false;
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            want_redraw |= effect.post_paint();
        }
        self.effects.retain(|e| !e.should_remove());
        want_redraw
    }

    pub fn on_workspace_switch(&mut self) {
        for effect in self.effects.iter_mut() {
            effect.on_workspace_switch();
        }
    }

    /// Route IPC payload to the effect whose `name()` matches `name`.
    pub fn dispatch_ipc(&mut self, name: &str, payload: &serde_json::Value) {
        if let Some(effect) = self.effects.iter_mut().find(|e| e.name() == name) {
            effect.handle_ipc(payload);
        }
    }

    pub fn dispatch_pointer_button(
        &mut self,
        event: &PointerButtonEvent,
        ctx: &mut EffectInputCtx<'_>,
    ) -> EventResult {
        for effect in self.effects.iter_mut() {
            if matches!(
                effect.handle_pointer_button(event, ctx),
                EventResult::Consumed
            ) {
                return EventResult::Consumed;
            }
        }
        EventResult::Pass
    }

    pub fn dispatch_key(&mut self, event: &KeyEvent, ctx: &mut EffectInputCtx<'_>) -> EventResult {
        for effect in self.effects.iter_mut() {
            if matches!(effect.handle_key(event, ctx), EventResult::Consumed) {
                return EventResult::Consumed;
            }
        }
        EventResult::Pass
    }
}
