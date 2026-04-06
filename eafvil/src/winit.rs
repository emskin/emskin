use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement,
            gles::GlesRenderer,
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::EventLoop,
    utils::{Physical, Rectangle, Size, Transform},
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
        let (renderer, mut framebuffer) = backend.bind().expect("failed to bind rendering backend");
        let render_scale = 1.0;
        smithay::desktop::space::render_output::<
            _,
            WaylandSurfaceRenderElement<GlesRenderer>,
            _,
            _,
        >(
            output,
            renderer,
            &mut framebuffer,
            render_scale,
            0,
            [&state.space],
            &[],
            damage_tracker,
            [1.0, 1.0, 1.0, 1.0],
        )
        .expect("render_output failed");
    }
    backend
        .submit(Some(&[damage]))
        .expect("frame submit failed");
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
                        for window in state.space.elements() {
                            if let Some(toplevel) = window.toplevel() {
                                toplevel.with_pending_state(|s| {
                                    s.size = Some(logical);
                                });
                                toplevel.send_pending_configure();
                            }
                        }
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
