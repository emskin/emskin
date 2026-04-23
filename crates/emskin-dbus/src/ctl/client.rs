//! Ctl-socket client — the emskin side of the connection that the
//! [`super::server`] handles. Exposes a minimal synchronous API tuned for
//! the compositor's needs: establish the connection, read the one-shot
//! `Ready` frame so emskin knows it is safe to inject
//! `DBUS_SESSION_BUS_ADDRESS` into child processes, then push rect /
//! focus updates from the focus state machine.
//!
//! Phase 1 client is deliberately single-threaded and owned by the
//! compositor's main loop. If finer-grained concurrency becomes necessary
//! we'll wrap this in a channel-backed worker thread; today the traffic
//! volume is on the order of one message per focus change plus one per
//! resize, so direct writes from the main loop are sufficient.

use std::io::{self, BufReader, ErrorKind};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::codec;
use crate::protocol::{EmskinToProxy, ProxyToEmskin};

/// A live ctl-socket connection. Drops the connection cleanly on [`Drop`].
#[derive(Debug)]
pub struct CtlClient {
    write_side: UnixStream,
    read_side: BufReader<UnixStream>,
}

impl CtlClient {
    /// Connect to `ctl_path`, retrying until `timeout` elapses.
    ///
    /// Retries exist because emskin typically spawns the proxy in parallel
    /// with preparing its own state; the proxy may take a few milliseconds
    /// to bind the ctl socket. A tight loop with `Duration::from_millis(10)`
    /// sleeps keeps startup latency imperceptible without busy-spinning.
    pub fn connect(ctl_path: impl AsRef<Path>, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(ctl_path.as_ref()) {
                Ok(conn) => {
                    let write_side = conn.try_clone()?;
                    let read_side = BufReader::new(conn);
                    return Ok(Self {
                        write_side,
                        read_side,
                    });
                }
                Err(e)
                    if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Read the one-shot `ProxyToEmskin::Ready` frame the server sends
    /// immediately after accept. Returns an error if the first frame is
    /// anything other than `Ready` — that indicates the server is buggy or
    /// we connected to the wrong socket.
    pub fn wait_ready(&mut self) -> io::Result<()> {
        match codec::read_frame(&mut self.read_side)? {
            Some(ProxyToEmskin::Ready) => Ok(()),
            Some(other) => Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("expected Ready, got {other:?}"),
            )),
            None => Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "ctl socket closed before Ready",
            )),
        }
    }

    /// Send one control message. Callers typically invoke this from the
    /// focus handler — short, rare, synchronous writes are fine.
    pub fn send(&mut self, msg: &EmskinToProxy) -> io::Result<()> {
        codec::write_frame(&mut self.write_side, msg)
    }

    /// Drain any pending `ProxyToEmskin` frames without blocking. Used by
    /// callers who want to surface proxy-side errors in the compositor log
    /// without wiring a separate reader thread. The underlying socket is
    /// blocking, so this just peeks once per call; callers should invoke
    /// it in a poll loop if they need continuous drainage.
    pub fn try_recv(&mut self) -> io::Result<Option<ProxyToEmskin>> {
        self.read_side.get_ref().set_nonblocking(true)?;
        let result = codec::read_frame::<_, ProxyToEmskin>(&mut self.read_side);
        // Always restore the blocking state — failing to do so leaves the
        // socket in an inconsistent mode for the next call.
        let restore = self.read_side.get_ref().set_nonblocking(false);
        match result {
            Ok(opt) => {
                restore?;
                Ok(opt)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                restore?;
                Ok(None)
            }
            Err(e) => {
                let _ = restore;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::io::SharedOffset;
    use crate::ctl::server;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use tempfile::tempdir;

    fn spawn_server(ctl_path: std::path::PathBuf, offset: SharedOffset) -> Arc<AtomicBool> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        thread::spawn(move || {
            server::run(&ctl_path, offset, move || {
                flag.store(true, Ordering::SeqCst)
            })
            .unwrap();
        });
        shutdown
    }

    #[test]
    fn connect_then_wait_ready_then_send() {
        let dir = tempdir().unwrap();
        let ctl_path = dir.path().join("ctl.sock");
        let offset = SharedOffset::new();
        let _shutdown = spawn_server(ctl_path.clone(), offset.clone());

        let mut client = CtlClient::connect(&ctl_path, Duration::from_secs(2)).unwrap();
        client.wait_ready().unwrap();
        client
            .send(&EmskinToProxy::FocusChanged {
                ctx: 7,
                rect: [500, 600, 100, 100],
            })
            .unwrap();

        // Poll until the server has applied the message.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if offset.get() == Some((500, 600)) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("offset not updated");
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn connect_retries_until_server_binds() {
        let dir = tempdir().unwrap();
        let ctl_path = dir.path().join("ctl.sock");
        let ctl_path_for_server = ctl_path.clone();

        // Delay server startup so the client spends time retrying.
        let offset = SharedOffset::new();
        let offset_clone = offset.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            server::run(&ctl_path_for_server, offset_clone, || {}).unwrap();
        });

        let mut client = CtlClient::connect(&ctl_path, Duration::from_secs(2)).unwrap();
        client.wait_ready().unwrap();
    }

    #[test]
    fn connect_times_out_when_server_never_binds() {
        let dir = tempdir().unwrap();
        let ctl_path = dir.path().join("ctl-nobind.sock");
        let err = CtlClient::connect(&ctl_path, Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn try_recv_returns_none_without_data() {
        let dir = tempdir().unwrap();
        let ctl_path = dir.path().join("ctl.sock");
        let offset = SharedOffset::new();
        let _shutdown = spawn_server(ctl_path.clone(), offset);

        let mut client = CtlClient::connect(&ctl_path, Duration::from_secs(2)).unwrap();
        client.wait_ready().unwrap();

        // Immediately after Ready the server has nothing more for us.
        assert!(client.try_recv().unwrap().is_none());
    }
}
