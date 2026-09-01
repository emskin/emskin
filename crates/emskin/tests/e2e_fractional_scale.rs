//! End-to-end coverage for wp-fractional-scale-v1 propagation.

mod common;

use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::Compositor;
use wayland_client::protocol::{wl_compositor, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};

#[derive(Default)]
struct ClientState {
    compositor: Option<wl_compositor::WlCompositor>,
    compositor_version: Option<u32>,
    fractional_scale_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    preferred_scale: Option<u32>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientState {
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
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor_version = Some(version);
                    state.compositor = Some(registry.bind(name, version.min(6), qh, ()));
                }
                "wp_fractional_scale_manager_v1" if state.fractional_scale_manager.is_none() => {
                    state.fractional_scale_manager =
                        Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

wayland_client::delegate_noop!(ClientState: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(ClientState: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(ClientState: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for ClientState {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.preferred_scale = Some(scale);
        }
    }
}

#[test]
fn surface_receives_the_output_preferred_scale() {
    let compositor = Compositor::spawn();
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(10));

    let socket = compositor
        .xdg_runtime_dir()
        .join(compositor.emskin_wayland());
    let connection = Connection::from_socket(UnixStream::connect(socket).unwrap()).unwrap();
    let mut queue = connection.new_event_queue::<ClientState>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = ClientState::default();

    queue.roundtrip(&mut state).unwrap();
    assert_eq!(
        state.compositor_version,
        Some(6),
        "emskin must advertise wl_compositor v6"
    );
    let wl_surface = state
        .compositor
        .as_ref()
        .expect("emskin did not advertise wl_compositor")
        .create_surface(&qh, ());
    let _fractional_scale = state
        .fractional_scale_manager
        .as_ref()
        .expect("emskin did not advertise wp-fractional-scale-v1")
        .get_fractional_scale(&wl_surface, &qh, ());

    queue.roundtrip(&mut state).unwrap();
    assert_eq!(
        state.preferred_scale,
        Some(120),
        "the 1x test output must be announced as 120/120"
    );
}
