//! E2E smoke tests for the `--xwayland-backend=satellite` path.
//!
//! Uses a mock `xwayland-satellite` shell script (ignores the real
//! protocol; just records that it was spawned and with what argv). This
//! validates emskin's wiring — probe, socket bind, XWaylandReady IPC,
//! on-demand spawn — without requiring the real satellite binary on the
//! test machine.

#![allow(dead_code)]

mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{wait_for_xwayland_ready, Compositor, NestedHost};

/// Tmp dir unique to this test invocation.
fn tmpdir(tag: &str) -> PathBuf {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "emskin-sat-e2e-{}-{}-{}",
        tag,
        std::process::id(),
        ns
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Write an executable mock xwayland-satellite. Accepts:
///   - `:N --test-listenfd-support` → exit 0 (probe)
///   - anything else → write PID + argv to the test hook files,
///     then block on sleep
fn write_mock_satellite(dir: &Path) -> PathBuf {
    let script = dir.join("xwls-mock");
    let body = r#"#!/bin/sh
# Probe mode: emskin's `test_ondemand` runs us with --test-listenfd-support
# and expects a zero-exit.
for arg in "$@"; do
    if [ "$arg" = "--test-listenfd-support" ]; then
        exit 0
    fi
done

# Normal (on-demand spawn) mode: record that we launched + argv, then
# block so emskin's spawner thread keeps us in `Running`.
if [ -n "$EMSKIN_TEST_MOCK_PIDFILE" ]; then
    echo "$$" > "$EMSKIN_TEST_MOCK_PIDFILE"
fi
if [ -n "$EMSKIN_TEST_MOCK_ARGSFILE" ]; then
    printf '%s\n' "$@" > "$EMSKIN_TEST_MOCK_ARGSFILE"
fi

exec sleep 30
"#;
    let mut f = fs::File::create(&script).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    drop(f);
    let mut perm = fs::metadata(&script).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&script, perm).unwrap();
    script
}

fn wait_for_pidfile(path: &Path, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(s) = fs::read_to_string(path) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                return Some(pid);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn satellite_backend_binds_sockets_and_spawns_on_x_client_connect() {
    let tmp = tmpdir("smoke");
    let mock = write_mock_satellite(&tmp);
    let pidfile = tmp.join("satellite.pid");
    let argsfile = tmp.join("satellite.args");

    let compositor = Compositor::spawn_with_satellite(
        NestedHost::wayland(),
        &mock,
        &[
            ("EMSKIN_TEST_MOCK_PIDFILE", &pidfile),
            ("EMSKIN_TEST_MOCK_ARGSFILE", &argsfile),
        ],
    );
    let mut ipc = compositor.connect_ipc();

    // (1) XWaylandReady IPC fires — emskin's satellite wiring is alive.
    let display = wait_for_xwayland_ready(&mut ipc);
    assert_eq!(
        display,
        compositor.emskin_display_num(),
        "XWaylandReady display should match the pinned --xwayland-display"
    );

    // (2) satellite has NOT been spawned yet — on-demand semantics: no
    // X client connected, no child.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !pidfile.exists(),
        "satellite should not spawn before an X11 client connects (found pidfile)"
    );

    // (3) X11 unix socket must be bound by emskin.
    let x_socket = PathBuf::from(format!("/tmp/.X11-unix/X{display}"));
    assert!(
        x_socket.exists(),
        "emskin should have pre-bound {x_socket:?} before any client connects"
    );

    // (4) Connect an X11 client → satellite should spawn.
    let _stream =
        UnixStream::connect(&x_socket).expect("X11 unix socket should accept connections");

    let mock_pid = wait_for_pidfile(&pidfile, Duration::from_secs(5))
        .expect("mock satellite should have written its pid after X client connected");
    assert!(mock_pid > 0);

    // Process should still be alive (mock does `exec sleep 30`).
    // kill(pid, 0) returns 0 if process exists, -1 / ESRCH if not.
    // SAFETY: kill with signal 0 just probes existence, no side effects.
    let alive = unsafe { libc::kill(mock_pid, 0) } == 0;
    assert!(
        alive,
        "mock satellite pid {mock_pid} should still be running"
    );

    // (5) argv check: first arg is `:N`, followed by at least one
    // `-listenfd <fd>` pair.
    let args_raw = fs::read_to_string(&argsfile).expect("argsfile must exist");
    let args: Vec<&str> = args_raw.lines().collect();
    assert!(!args.is_empty(), "args must have at least the display name");
    assert_eq!(args[0], format!(":{display}"), "first arg must be :N");
    let listenfd_count = args.iter().filter(|a| **a == "-listenfd").count();
    assert!(
        listenfd_count >= 1,
        "expected at least one -listenfd in argv, got: {args:?}"
    );
}
