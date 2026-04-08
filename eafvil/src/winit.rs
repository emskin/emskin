use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker,
            element::texture::TextureRenderElement,
            gles::{GlesRenderer, GlesTexture},
            utils::with_renderer_surface_state,
            Renderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Logical, Physical, Rectangle, Size, Transform},
};

use crate::EafvilState;

const REFRESH_RATE: i32 = 60_000;

fn make_mode(size: Size<i32, Physical>) -> Mode {
    Mode {
        size,
        refresh: REFRESH_RATE,
    }
}

fn apply_pending_state(state: &mut EafvilState, backend: &mut WinitGraphicsBackend<GlesRenderer>) {
    if let Some(title) = state.emacs_title.take() {
        backend.window().set_title(&title);
    }

    if let Some(fullscreen) = state.pending_fullscreen.take() {
        if fullscreen {
            backend
                .window()
                .set_fullscreen(Some(winit_crate::window::Fullscreen::Borderless(None)));
        } else {
            backend.window().set_fullscreen(None);
        }
    }

    if let Some(maximize) = state.pending_maximize.take() {
        backend.window().set_maximized(maximize);
    }
}

/// Build TextureRenderElements for all mirrors by reading the surface's
/// committed texture directly — no copy, no snapshot.
fn build_mirror_elements(
    state: &EafvilState,
    renderer: &mut GlesRenderer,
    scale: f64,
) -> Vec<TextureRenderElement<GlesTexture>> {
    let ctx = renderer.context_id();
    let mut elements = Vec::new();
    for app in state.apps.windows() {
        if app.mirrors.is_empty() {
            continue;
        }
        let Some(toplevel) = app.window.toplevel() else {
            continue;
        };
        let wl_surface = toplevel.wl_surface().clone();
        // Ensure the surface buffer is imported as a texture before reading it.
        if let Err(e) =
            smithay::backend::renderer::utils::import_surface_tree(renderer, &wl_surface)
        {
            tracing::warn!(
                "import_surface_tree failed for wid={}: {e:?}",
                app.window_id
            );
            continue;
        }
        let ctx_clone = ctx.clone();
        let Some((texture, buf_scale, buf_transform, view_src)) =
            with_renderer_surface_state(&wl_surface, |rss| {
                let tex = rss.texture::<GlesTexture>(ctx_clone).cloned()?;
                let src = rss.view().map(|v| v.src);
                Some((tex, rss.buffer_scale(), rss.buffer_transform(), src))
            })
            .flatten()
        else {
            continue;
        };
        let Some(source_geo) = app.geometry else {
            continue;
        };
        let src_size = source_geo.size.to_f64();

        for mv in app.mirrors.values() {
            let m = mv.geometry;
            let Some(ratio) = crate::apps::AppManager::aspect_fit_ratio(src_size, m.size.to_f64())
            else {
                continue;
            };
            let fit_w = (src_size.w * ratio).round() as i32;
            let fit_h = (src_size.h * ratio).round() as i32;

            let element = TextureRenderElement::from_static_texture(
                mv.render_id.clone(),
                ctx.clone(),
                m.loc.to_f64().to_physical(scale),
                texture.clone(),
                buf_scale,
                buf_transform,
                None, // alpha
                view_src,
                Some((fit_w, fit_h).into()),
                None, // opaque_regions
                smithay::backend::renderer::element::Kind::Unspecified,
            );
            elements.push(element);
        }
    }
    elements
}

fn render_frame(
    state: &EafvilState,
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    output: &Output,
    damage_tracker: &mut OutputDamageTracker,
) {
    let size = backend.window_size();

    if output.current_mode().map(|m| m.size) != Some(size) {
        output.change_current_state(Some(make_mode(size)), None, None, None);
    }

    let damage = Rectangle::from_size(size);

    {
        let Ok((renderer, mut framebuffer)) = backend.bind() else {
            tracing::error!("Failed to bind rendering backend, skipping frame");
            return;
        };

        // Build mirror elements by referencing the surface's committed texture
        // directly — zero copy, always up-to-date.
        let scale = output.current_scale().fractional_scale();
        let mirror_elements = build_mirror_elements(state, renderer, scale);

        let render_scale = 1.0;
        if let Err(e) =
            smithay::desktop::space::render_output::<_, TextureRenderElement<GlesTexture>, _, _>(
                output,
                renderer,
                &mut framebuffer,
                render_scale,
                0,
                [&state.space],
                &mirror_elements,
                damage_tracker,
                [1.0, 1.0, 1.0, 1.0],
            )
        {
            tracing::error!("render_output failed: {e}");
            return;
        }
    }

    if let Err(e) = backend.submit(Some(&[damage])) {
        tracing::error!("frame submit failed: {e}");
    }
}

fn post_render(state: &mut EafvilState, output: &Output) {
    state.space.elements().for_each(|window| {
        window.send_frame(
            output,
            state.start_time.elapsed(),
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        )
    });

    state.space.refresh();
    state.popups.cleanup();
    if let Err(e) = state.display_handle.flush_clients() {
        tracing::warn!("flush_clients failed: {}", e);
    }
}

/// Resize only the Emacs toplevel; EAF app sizes come from Emacs via IPC.
fn resize_emacs_surface(state: &mut EafvilState, logical: Size<i32, Logical>) {
    let Some(ref emacs_surface) = state.emacs_surface else {
        return;
    };
    for window in state.space.elements() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        if toplevel.wl_surface() != emacs_surface {
            continue;
        }
        toplevel.with_pending_state(|s| {
            s.size = Some(logical);
        });
        toplevel.send_pending_configure();
        return;
    }
}

pub fn init_winit(
    event_loop: &mut EventLoop<EafvilState>,
    state: &mut EafvilState,
) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init()?;

    backend.window().set_title("Emacs");
    backend.window().set_maximized(true);

    let mode = make_mode(backend.window_size());

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<EafvilState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    event_loop
        .handle()
        .insert_source(winit, move |event, _, state| {
            match event {
                WinitEvent::Resized { size, scale_factor } => {
                    let int_scale = scale_factor.ceil() as i32;
                    tracing::info!(
                        "Host resized: {}x{} scale={} (int={})",
                        size.w,
                        size.h,
                        scale_factor,
                        int_scale
                    );
                    output.change_current_state(
                        Some(make_mode(size)),
                        None,
                        Some(Scale::Fractional(scale_factor)),
                        None,
                    );

                    if state.initial_size_settled {
                        let logical = size.to_f64().to_logical(scale_factor).to_i32_round();
                        resize_emacs_surface(state, logical);
                    }
                }

                WinitEvent::Input(event) => state.process_input_event(event),

                WinitEvent::Redraw => {
                    apply_pending_state(state, &mut backend);
                    render_frame(state, &mut backend, &output, &mut damage_tracker);
                    post_render(state, &output);
                    backend.window().request_redraw();
                }

                WinitEvent::CloseRequested => {
                    state.loop_signal.stop();
                }

                _ => (),
            };
        })?;

    Ok(())
}
