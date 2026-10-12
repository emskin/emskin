//! Emacs child-frame rendering — lifts the regions of the Emacs surface
//! occupied by child frames (corfu, posframe) above embedded apps.

use smithay::{
    backend::renderer::{
        element::{texture::TextureRenderElement, Id, Kind},
        gles::GlesRenderer,
        utils::import_surface_tree,
        Renderer,
    },
    utils::{Logical, Point, Rectangle, Size},
};

use crate::surface_snapshot::collect_surface_tree;
use crate::winit::CustomElement;
use crate::EmskinState;

/// Damage-tracker namespace for child-frame copies, kept clear of the
/// mirror `view_id` namespaces (small integers handed out by Emacs).
const CHILD_FRAME_ID_NAMESPACE: usize = usize::MAX / 2;

/// Build TextureRenderElements that re-draw the Emacs surface inside each
/// child-frame rect. pgtk paints child frames (corfu, posframe) into the
/// parent frame's own `wl_surface`, so without this an embedded app mapped
/// above Emacs hides them. Each element is a 1:1 crop of the Emacs buffer,
/// placed exactly where Emacs already draws it.
pub fn build_child_frame_elements(
    state: &EmskinState,
    renderer: &mut GlesRenderer,
    scale: f64,
) -> Vec<CustomElement<GlesRenderer>> {
    let rects = state.emacs.child_frame_rects();
    if rects.is_empty() {
        return Vec::new();
    }
    let (Some(surface), Some((_, origin))) = (state.emacs.surface(), state.emacs_render_origin())
    else {
        return Vec::new();
    };
    if let Err(e) = import_surface_tree(renderer, surface) {
        tracing::warn!("import_surface_tree failed for Emacs child frames: {e:?}");
        return Vec::new();
    }

    let ctx = renderer.context_id();
    let snapshots = collect_surface_tree(renderer, surface, origin);
    let mut elements = Vec::new();
    for (rect_idx, rect) in rects.iter().enumerate() {
        // Subsurfaces sit above their parent: emit topmost first.
        for snap in snapshots.iter().rev() {
            let dst = Rectangle::new(snap.offset, snap.view_dst.to_f64());
            let Some((clip, src)) = crop_view(dst, snap.view_src, rect.to_f64()) else {
                continue;
            };
            let render_id = Id::from_wayland_resource(&snap.surface)
                .namespaced(CHILD_FRAME_ID_NAMESPACE + rect_idx);
            elements.push(
                TextureRenderElement::from_static_texture(
                    render_id,
                    ctx.clone(),
                    clip.loc.to_physical(scale),
                    snap.texture.clone(),
                    snap.buffer_scale,
                    snap.buffer_transform,
                    None,
                    Some(src),
                    Some(clip.size.to_i32_round()),
                    None,
                    Kind::Unspecified,
                )
                .into(),
            );
        }
    }
    elements
}

/// Crop a surface drawn at `dst` (sampling `view_src` from its buffer) down
/// to `clip_to`. Returns the visible on-screen rect and the matching
/// sub-rect of `view_src`, or `None` when nothing (≥1px) is left.
fn crop_view(
    dst: Rectangle<f64, Logical>,
    view_src: Rectangle<f64, Logical>,
    clip_to: Rectangle<f64, Logical>,
) -> Option<(Rectangle<f64, Logical>, Rectangle<f64, Logical>)> {
    let clip = dst.intersection(clip_to)?;
    let size = clip.size.to_i32_round::<i32>();
    if size.w <= 0 || size.h <= 0 {
        return None;
    }
    let sx = view_src.size.w / dst.size.w;
    let sy = view_src.size.h / dst.size.h;
    let src = Rectangle::new(
        Point::from((
            view_src.loc.x + (clip.loc.x - dst.loc.x) * sx,
            view_src.loc.y + (clip.loc.y - dst.loc.y) * sy,
        )),
        Size::from((clip.size.w * sx, clip.size.h * sy)),
    );
    Some((clip, src))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn crop_view_maps_clip_into_src_one_to_one() {
        let dst = rect(0.0, 68.0, 1280.0, 732.0);
        let (clip, src) = crop_view(
            dst,
            rect(0.0, 0.0, 1280.0, 732.0),
            rect(200.0, 268.0, 392.0, 160.0),
        )
        .unwrap();
        assert_eq!(clip, rect(200.0, 268.0, 392.0, 160.0));
        assert_eq!(src, rect(200.0, 200.0, 392.0, 160.0));
    }

    #[test]
    fn crop_view_scales_into_viewport() {
        // Surface sampled from a 2x viewport offset by (10, 20).
        let dst = rect(100.0, 100.0, 50.0, 50.0);
        let (clip, src) = crop_view(
            dst,
            rect(10.0, 20.0, 100.0, 100.0),
            rect(120.0, 90.0, 100.0, 20.0),
        )
        .unwrap();
        assert_eq!(clip, rect(120.0, 100.0, 30.0, 10.0));
        assert_eq!(src, rect(50.0, 20.0, 60.0, 20.0));
    }

    #[test]
    fn crop_view_rejects_disjoint_and_subpixel() {
        let dst = rect(0.0, 0.0, 100.0, 100.0);
        let src = rect(0.0, 0.0, 100.0, 100.0);
        assert!(crop_view(dst, src, rect(200.0, 200.0, 10.0, 10.0)).is_none());
        assert!(crop_view(dst, src, rect(99.8, 0.0, 10.0, 10.0)).is_none());
    }
}
