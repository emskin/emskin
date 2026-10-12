//! Per-frame texture snapshots of a `wl_surface` tree, shared by the
//! render passes that re-draw client content outside smithay's `Space`
//! (window mirrors, Emacs child frames).

use smithay::{
    backend::renderer::{
        gles::{GlesRenderer, GlesTexture},
        utils::RendererSurfaceStateUserData,
        Renderer,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size, Transform},
    wayland::compositor::{with_surface_tree_downward, TraversalAction},
};

/// Snapshot of one mapped surface within a subsurface tree, in logical
/// coords. Collected once per tree per frame so each consumer only has to
/// scale/translate/crop (cheap), not re-walk the tree (expensive on GTK apps
/// with many subsurfaces).
pub(crate) struct SurfaceSnapshot {
    pub surface: WlSurface,
    /// Top-left of the surface's viewport, relative to the `origin` passed
    /// to `collect_surface_tree`.
    pub offset: Point<f64, Logical>,
    pub view_src: Rectangle<f64, Logical>,
    pub view_dst: Size<i32, Logical>,
    pub texture: GlesTexture,
    pub buffer_scale: i32,
    pub buffer_transform: Transform,
}

/// Walk `root`'s subsurface tree and collect one snapshot per mapped surface
/// with a texture. Offsets accumulate in logical space starting from
/// `origin` — for app layers that is `layer.render_offset`, which already
/// matches smithay's `Space::render_location()` (canceling GTK CSD shadow
/// padding).
pub(crate) fn collect_surface_tree(
    renderer: &mut GlesRenderer,
    root: &WlSurface,
    origin: Point<i32, Logical>,
) -> Vec<SurfaceSnapshot> {
    let ctx = renderer.context_id();
    let mut out: Vec<SurfaceSnapshot> = Vec::new();
    with_surface_tree_downward(
        root,
        origin.to_f64(),
        |_, states, loc| {
            let mut loc = *loc;
            if let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() {
                if let Some(view) = data.lock().unwrap().view() {
                    loc.x += view.offset.x as f64;
                    loc.y += view.offset.y as f64;
                    return TraversalAction::DoChildren(loc);
                }
            }
            TraversalAction::SkipChildren
        },
        |surface, states, loc| {
            let mut loc = *loc;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let data = data.lock().unwrap();
            let Some(view) = data.view() else { return };
            loc.x += view.offset.x as f64;
            loc.y += view.offset.y as f64;
            let Some(texture) = data.texture::<GlesTexture>(ctx.clone()).cloned() else {
                return;
            };
            out.push(SurfaceSnapshot {
                surface: surface.clone(),
                offset: loc,
                view_src: view.src,
                view_dst: view.dst,
                texture,
                buffer_scale: data.buffer_scale(),
                buffer_transform: data.buffer_transform(),
            });
        },
        |_, _, _| true,
    );
    out
}
