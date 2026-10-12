//! Mirror rendering — builds TextureRenderElements for scaled copies of
//! embedded app surfaces displayed in multiple Emacs windows.

use smithay::{
    backend::renderer::{
        element::{texture::TextureRenderElement, Id, Kind},
        gles::GlesRenderer,
        utils::import_surface_tree,
        Renderer,
    },
    utils::{Logical, Point},
};

use crate::surface_snapshot::collect_surface_tree;
use crate::winit::CustomElement;
use crate::EmskinState;

/// Build TextureRenderElements for all mirrors. Handles clients (e.g. GTK /
/// Firefox) that render via `wl_subsurface` rather than attaching a buffer
/// directly to the toplevel.
pub fn build_mirror_elements(
    state: &mut EmskinState,
    renderer: &mut GlesRenderer,
    scale: f64,
) -> Vec<CustomElement<GlesRenderer>> {
    let ctx = renderer.context_id();
    let mut elements = Vec::new();
    for app in state.apps.windows() {
        if app.mirrors.is_empty() {
            continue;
        }
        let Some(source_geo) = app.geometry else {
            continue;
        };
        let src_size = source_geo.size.to_f64();
        let layers = app.surface_layers();

        // Iterate layers in reverse: popups first (higher z-order in smithay's
        // front-to-back damage tracker), then toplevel last (background).
        for (layer_idx, layer) in layers.iter().enumerate().rev() {
            if let Err(e) = import_surface_tree(renderer, &layer.surface) {
                tracing::warn!(
                    "import_surface_tree failed for wid={} layer={layer_idx}: {e:?}",
                    app.window_id
                );
                continue;
            }

            let snapshots = collect_surface_tree(renderer, &layer.surface, layer.render_offset);
            if snapshots.is_empty() {
                continue;
            }

            for (&view_id, mirror) in &app.mirrors {
                if mirror.workspace_id != state.workspace.active_id {
                    continue;
                }
                let Some(ratio) = crate::apps::AppManager::aspect_fit_ratio(
                    src_size,
                    mirror.geometry.size.to_f64(),
                ) else {
                    continue;
                };

                for snap in &snapshots {
                    let loc = Point::<f64, Logical>::from((
                        mirror.geometry.loc.x as f64 + snap.offset.x * ratio,
                        mirror.geometry.loc.y as f64 + snap.offset.y * ratio,
                    ));
                    let fit_w = (snap.view_dst.w as f64 * ratio).round() as i32;
                    let fit_h = (snap.view_dst.h as f64 * ratio).round() as i32;

                    // Stable per-(surface, mirror) ID so the damage tracker
                    // treats the same surface in different mirrors as distinct.
                    let render_id =
                        Id::from_wayland_resource(&snap.surface).namespaced(view_id as usize);

                    elements.push(
                        TextureRenderElement::from_static_texture(
                            render_id,
                            ctx.clone(),
                            loc.to_physical(scale),
                            snap.texture.clone(),
                            snap.buffer_scale,
                            snap.buffer_transform,
                            None,
                            Some(snap.view_src),
                            Some((fit_w.max(1), fit_h.max(1)).into()),
                            None,
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                }
            }
        }
    }
    elements
}
