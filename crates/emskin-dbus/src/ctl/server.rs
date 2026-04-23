//! Ctl-socket server: accept one emskin connection at a time, read
//! `EmskinToProxy` frames, fold them into [`SharedOffset`], and emit a
//! single `Ready` frame after each accept so emskin knows it is safe to
//! spawn children that will inherit `DBUS_SESSION_BUS_ADDRESS`.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use crate::broker::io::SharedOffset;
use crate::codec;
use crate::protocol::{EmskinToProxy, ProxyToEmskin};

/// What to do after processing one control message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Continue,
    Shutdown,
}

/// Bind `ctl_path`, accept one emskin client at a time, process until the
/// stream closes, then accept the next one. The `on_shutdown` callback is
/// invoked when a `Shutdown` message arrives so the caller can tear down
/// the rest of the process. If emskin just closes the socket we keep
/// listening (the next emskin spawn can reconnect).
pub fn run(
    ctl_path: impl AsRef<Path>,
    offset: SharedOffset,
    on_shutdown: impl Fn() + Send + 'static,
) -> std::io::Result<()> {
    let listener = UnixListener::bind(ctl_path.as_ref())?;
    tracing::info!(ctl_path = ?ctl_path.as_ref(), "ctl server listening");

    for incoming in listener.incoming() {
        let conn = match incoming {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(error = %e, "ctl accept failed");
                continue;
            }
        };
        match handle_one(conn, &offset) {
            Ok(Action::Continue) => continue,
            Ok(Action::Shutdown) => {
                tracing::info!("ctl received Shutdown — initiating process exit");
                on_shutdown();
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(error = %e, "ctl connection terminated with error");
            }
        }
    }
    Ok(())
}

fn handle_one(conn: UnixStream, offset: &SharedOffset) -> std::io::Result<Action> {
    // Split into read/write halves. BufReader avoids per-frame syscalls.
    let mut write_conn = conn.try_clone()?;
    let mut reader = BufReader::new(conn);

    // Ready is sent *once* per accepted connection — right before we start
    // reading frames. Emskin reads it synchronously before spawning child
    // processes that depend on `DBUS_SESSION_BUS_ADDRESS`.
    codec::write_frame(&mut write_conn, &ProxyToEmskin::Ready)?;

    loop {
        match codec::read_frame::<_, EmskinToProxy>(&mut reader)? {
            None => return Ok(Action::Continue), // clean EOF
            Some(msg) => {
                if apply(&msg, offset) == Action::Shutdown {
                    return Ok(Action::Shutdown);
                }
            }
        }
    }
}

fn apply(msg: &EmskinToProxy, offset: &SharedOffset) -> Action {
    match msg {
        EmskinToProxy::FocusChanged { rect, .. } | EmskinToProxy::RectChanged { rect, .. } => {
            offset.set(Some((rect[0], rect[1])));
            Action::Continue
        }
        EmskinToProxy::FocusCleared => {
            offset.set(None);
            Action::Continue
        }
        EmskinToProxy::Shutdown => Action::Shutdown,
        // Phase 1 doesn't consume per-ctx data yet — the offset is global.
        EmskinToProxy::ClientBorn { .. }
        | EmskinToProxy::CtxGone { .. }
        | EmskinToProxy::BulkRects { .. }
        | EmskinToProxy::WorkspaceSwitched { .. } => Action::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn apply_focus_changed_sets_offset() {
        let offset = SharedOffset::new();
        let msg = EmskinToProxy::FocusChanged {
            ctx: 1,
            rect: [50, 60, 100, 200],
        };
        assert_eq!(apply(&msg, &offset), Action::Continue);
        assert_eq!(offset.get(), Some((50, 60)));
    }

    #[test]
    fn apply_rect_changed_updates_offset() {
        let offset = SharedOffset::new();
        offset.set(Some((10, 20)));
        let msg = EmskinToProxy::RectChanged {
            ctx: 1,
            rect: [200, 300, 50, 50],
        };
        apply(&msg, &offset);
        assert_eq!(offset.get(), Some((200, 300)));
    }

    #[test]
    fn apply_focus_cleared_clears_offset() {
        let offset = SharedOffset::new();
        offset.set(Some((10, 20)));
        apply(&EmskinToProxy::FocusCleared, &offset);
        assert_eq!(offset.get(), None);
    }

    #[test]
    fn apply_shutdown_returns_shutdown_action() {
        let offset = SharedOffset::new();
        assert_eq!(apply(&EmskinToProxy::Shutdown, &offset), Action::Shutdown);
    }

    #[test]
    fn apply_unused_messages_are_noop() {
        let offset = SharedOffset::new();
        offset.set(Some((10, 20)));
        apply(&EmskinToProxy::ClientBorn { pid: 42, ctx: 1 }, &offset);
        apply(&EmskinToProxy::CtxGone { ctx: 1 }, &offset);
        apply(
            &EmskinToProxy::BulkRects {
                updates: vec![(1, [1, 2, 3, 4])],
            },
            &offset,
        );
        apply(
            &EmskinToProxy::WorkspaceSwitched { workspace_id: 2 },
            &offset,
        );
        // Offset unchanged — these messages don't touch it in Phase 1.
        assert_eq!(offset.get(), Some((10, 20)));
    }

    /// Full integration test: start the server in a thread, connect as
    /// emskin, read `Ready`, push `FocusChanged`, assert the offset moves.
    #[test]
    fn end_to_end_server_round_trip() {
        let dir = tempdir().unwrap();
        let ctl_path = dir.path().join("ctl.sock");

        let offset = SharedOffset::new();
        let offset_shared = offset.clone();
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown_called.clone();

        let ctl_path_clone = ctl_path.clone();
        let server_thread = thread::spawn(move || {
            run(&ctl_path_clone, offset_shared, move || {
                shutdown_flag.store(true, Ordering::SeqCst);
            })
            .unwrap();
        });

        // Poll until the server has bound.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let client = loop {
            match UnixStream::connect(&ctl_path) {
                Ok(c) => break c,
                Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("ctl connect timed out: {e}"),
            }
        };

        let mut client_r = BufReader::new(client.try_clone().unwrap());
        let mut client_w = client;

        // Read the initial Ready.
        let ready: ProxyToEmskin = codec::read_frame(&mut client_r).unwrap().unwrap();
        assert_eq!(ready, ProxyToEmskin::Ready);

        // Push a FocusChanged.
        codec::write_frame(
            &mut client_w,
            &EmskinToProxy::FocusChanged {
                ctx: 1,
                rect: [111, 222, 300, 400],
            },
        )
        .unwrap();

        // Wait (bounded) for the offset to be applied.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if offset.get() == Some((111, 222)) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("offset not updated within 2s");
            }
            thread::sleep(Duration::from_millis(5));
        }

        // Send Shutdown; server should invoke the callback and exit.
        codec::write_frame(&mut client_w, &EmskinToProxy::Shutdown).unwrap();
        drop(client_w);
        drop(client_r);

        server_thread.join().unwrap();
        assert!(shutdown_called.load(Ordering::SeqCst));
    }
}
