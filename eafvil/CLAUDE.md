# eafvil - Nested Wayland Compositor for Emacs

## Build
- `cargo check` / `cargo clippy -- -D warnings` / `cargo fmt`
- smithay: local path dependency (see `Cargo.toml`). Upstream: `git clone https://github.com/Smithay/smithay.git`

## Architecture
- Nested Wayland compositor using smithay, hosting Emacs inside a winit window
- Single-window constraint: only one Emacs toplevel allowed
- grabs/ directory is placeholder code for future move/resize support

## Key Gotchas
- smithay winit backend defaults to 10-10-10-2 pixel format (2-bit alpha) — breaks GTK semi-transparent UI. Fixed by prioritizing 8-bit in smithay's `backend/winit/mod.rs`
- winit `scale_factor()` returns 1.0 at init time; real scale arrives later via `ScaleFactorChanged` → `WinitEvent::Resized { scale_factor }`
- Use `Scale::Fractional(scale_factor)` not `Scale::Integer(ceil)` to match host compositor's actual DPI
- `render_scale` in `render_output()` should be 1.0 (smallvil pattern); smithay handles client buffer_scale internally
- `Transform::Flipped180` is required for correct orientation with the winit EGL backend
- Use smithay's type-safe geometry: `size.to_f64().to_logical(scale).to_i32_round()` instead of manual arithmetic
- GTK3 Emacs does NOT support xdg-decoration protocol — setting `Fullscreen` state on the toplevel is what actually hides CSD titlebar/borders
- GTK4/GTK3 will send `unmaximize_request`/`unfullscreen_request` immediately on connect if those states are set in initial configure — must ignore these for single-window compositor
- Host keyboard layout: smithay winit backend does NOT expose the host's keymap. Use `wayland-client` to separately connect, receive `wl_keyboard.keymap`, then `KeyboardHandle::set_keymap_from_string()` — env vars (`XKB_DEFAULT_*`) are unreliable on KDE Wayland

## Wayland Protocols Implemented
- xdg_shell (toplevel, popup)
- xdg-decoration (force ServerSide — no decorations drawn)
- wl_seat (keyboard + pointer)
- wl_data_device (DnD)
- fractional_scale, viewporter
