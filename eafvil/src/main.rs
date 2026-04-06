#![allow(irrefutable_let_patterns)]

mod grabs;
mod handlers;
mod input;
pub mod ipc;
mod keymap;
mod state;
mod winit;

use clap::Parser;
use smithay::reexports::{
    calloop::{generic::Generic, Interest, Mode, PostAction},
    wayland_server::Display,
};
pub use state::EafvilState;

/// Nested Wayland compositor for Emacs Application Framework.
#[derive(Parser, Debug)]
#[command(name = "eafvil")]
struct Cli {
    /// Do not spawn Emacs; wait for an external connection.
    #[arg(long)]
    no_spawn: bool,

    /// Command to launch Emacs (default: "emacs").
    #[arg(long, default_value = "emacs")]
    emacs_command: String,

    /// Explicit IPC socket path (default: $XDG_RUNTIME_DIR/eafvil-<pid>.ipc).
    #[arg(long)]
    ipc_path: Option<std::path::PathBuf>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    let cli = Cli::parse();

    let mut event_loop: smithay::reexports::calloop::EventLoop<EafvilState> =
        smithay::reexports::calloop::EventLoop::try_new()?;

    let display: Display<EafvilState> = Display::new()?;

    let ipc_path = cli.ipc_path.clone().unwrap_or_else(default_ipc_path);
    tracing::info!("IPC socket path: {}", ipc_path.display());

    let ipc = crate::ipc::IpcServer::bind(ipc_path)?;
    let mut state = EafvilState::new(&mut event_loop, display, ipc);

    // Inherit the host compositor's keyboard layout
    match keymap::read_host_keymap() {
        Some(host_keymap) => {
            tracing::info!("Loaded host keyboard keymap ({} bytes)", host_keymap.len());
            if let Err(e) = state
                .seat
                .get_keyboard()
                .unwrap()
                .set_keymap_from_string(&mut state, host_keymap)
            {
                tracing::warn!("Failed to apply host keymap: {e:?}, using default");
            }
        }
        None => tracing::info!("Could not read host keymap, using default"),
    }

    // Register IPC listener fd with calloop (accept new connections).
    {
        use std::os::unix::io::FromRawFd;
        let listener_fd = state.ipc.listener_fd();
        // SAFETY: We duplicate the fd so the Generic source owns its own copy.
        // The original fd remains valid inside IpcServer for the lifetime of state.
        let dup_fd = unsafe { libc::dup(listener_fd) };
        if dup_fd < 0 {
            return Err("dup(ipc listener fd) failed".into());
        }
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        event_loop
            .handle()
            .insert_source(
                Generic::new(file, Interest::READ, Mode::Level),
                |_, _, state| {
                    state.ipc.accept();
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| format!("failed to register IPC listener: {e}"))?;
    }

    // Open a Wayland/X11 window for our nested compositor
    crate::winit::init_winit(&mut event_loop, &mut state)?;

    spawn_emacs(&cli, &mut state);

    event_loop.run(None, &mut state, |state| {
        if let Some(ref mut child) = state.emacs_child {
            if let Ok(Some(status)) = child.try_wait() {
                tracing::info!("Emacs exited with {status}, stopping compositor");
                state.loop_signal.stop();
            }
        }

        // Dispatch incoming IPC messages from Emacs.
        if let Some(msgs) = state.ipc.recv_all() {
            for msg in msgs {
                handle_ipc_message(state, msg);
            }
        }
    })?;

    // Clean up Emacs child process
    if let Some(mut child) = state.emacs_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(())
}

fn spawn_emacs(cli: &Cli, state: &mut EafvilState) {
    if cli.no_spawn {
        tracing::info!("--no-spawn: waiting for external Emacs connection");
        return;
    }

    let socket_name = state.socket_name.to_str().unwrap_or("").to_string();
    tracing::info!(
        "Spawning Emacs: {} (WAYLAND_DISPLAY={})",
        cli.emacs_command,
        socket_name
    );
    match std::process::Command::new(&cli.emacs_command)
        .env("WAYLAND_DISPLAY", &socket_name)
        .spawn()
    {
        Ok(child) => state.emacs_child = Some(child),
        Err(e) => tracing::error!("Failed to spawn '{}': {}", cli.emacs_command, e),
    }
}

fn default_ipc_path() -> std::path::PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let pid = std::process::id();
    std::path::PathBuf::from(format!("{runtime_dir}/eafvil-{pid}.ipc"))
}

fn handle_ipc_message(_state: &mut EafvilState, msg: ipc::IncomingMessage) {
    use ipc::IncomingMessage;
    match msg {
        IncomingMessage::SetGeometry {
            window_id,
            x,
            y,
            w,
            h,
        } => {
            tracing::debug!("IPC set_geometry window={window_id} ({x},{y},{w},{h})");
            // M2 will wire this to surface configure.
        }
        IncomingMessage::Close { window_id } => {
            tracing::debug!("IPC close window={window_id}");
        }
        IncomingMessage::SetVisibility { window_id, visible } => {
            tracing::debug!("IPC set_visibility window={window_id} visible={visible}");
        }
        IncomingMessage::ForwardKey {
            window_id,
            keycode,
            state,
            modifiers,
        } => {
            tracing::debug!(
                "IPC forward_key window={window_id} key={keycode} state={state} mods={modifiers}"
            );
        }
    }
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
