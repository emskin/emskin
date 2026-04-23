//! `emskin-dbus-proxy` — thin CLI runtime wrapper around [`emskin_dbus`].
//!
//! All non-trivial logic lives in the library crate so this binary stays a
//! launcher: parse env, bind the two Unix sockets (bus-facing + ctl-facing),
//! start the ctl server in a thread, and run the broker accept loop in the
//! main thread. Exits when either loop returns or the ctl server receives
//! an explicit `Shutdown` message.
//!
//! Required env vars:
//!
//! - `EMSKIN_DBUS_PROXY_LISTEN` — path to the Unix socket we bind for
//!   embedded clients. Injected into their `DBUS_SESSION_BUS_ADDRESS`
//!   as `unix:path=<this>`.
//! - `EMSKIN_DBUS_PROXY_CTL` — path to the ctl socket emskin uses to push
//!   focus/rect updates.
//! - `DBUS_SESSION_BUS_ADDRESS` — upstream session bus; only the
//!   `unix:path=<…>` form is accepted today.
//!
//! Optional:
//!
//! - `EMSKIN_DBUS_LOG` — tracing env filter (default: `info`).

use std::io::{self, ErrorKind};
use std::path::PathBuf;
use std::thread;

use emskin_dbus::broker::io::{BrokerServer, SharedOffset};
use emskin_dbus::ctl::server as ctl_server;

fn main() -> io::Result<()> {
    init_tracing();

    let listen_path = env_path("EMSKIN_DBUS_PROXY_LISTEN")?;
    let ctl_path = env_path("EMSKIN_DBUS_PROXY_CTL")?;
    let bus_addr = env_required("DBUS_SESSION_BUS_ADDRESS")?;
    let bus_path = parse_unix_bus_address(&bus_addr)?;

    // Clean up stale sockets from prior runs. Using remove_file keeps us from
    // accidentally nuking a directory someone set up manually.
    let _ = std::fs::remove_file(&listen_path);
    let _ = std::fs::remove_file(&ctl_path);

    let offset = SharedOffset::new();

    // Bind the bus socket first so it exists by the time ctl sends `Ready`
    // to emskin; emskin may spawn children immediately on receipt.
    let server = BrokerServer::bind(&listen_path, &bus_path, offset.clone())?;
    tracing::info!(
        ?listen_path,
        ?bus_path,
        ?ctl_path,
        "emskin-dbus-proxy sockets bound"
    );

    // Ctl server owns the shutdown path; when it receives Shutdown we exit
    // the whole process rather than trying to drain the broker listener
    // (UnixListener has no portable unblock for its accept loop).
    let ctl_offset = offset;
    let ctl_path_thread = ctl_path.clone();
    thread::Builder::new()
        .name("emskin-dbus-ctl".into())
        .spawn(move || {
            if let Err(e) = ctl_server::run(&ctl_path_thread, ctl_offset, || std::process::exit(0))
            {
                tracing::error!(error = %e, "ctl server terminated");
            }
        })?;

    // This blocks forever (or until a fatal accept error).
    server.run()
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("EMSKIN_DBUS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn env_path(key: &str) -> io::Result<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, format!("{key} not set")))
}

fn env_required(key: &str) -> io::Result<String> {
    std::env::var(key)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, format!("{key} not set")))
}

/// Extract the filesystem path from a `unix:path=…[,guid=…]` DBus address.
///
/// DBus supports more forms (`unix:abstract=`, `tcp:`, `nonce-tcp:`) but the
/// session bus in every modern Linux distro is `unix:path=`; widening the
/// parser when we see something else in the wild is deferred.
fn parse_unix_bus_address(addr: &str) -> io::Result<PathBuf> {
    const PREFIX: &str = "unix:path=";
    let stripped = addr.strip_prefix(PREFIX).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported bus scheme: {addr}"),
        )
    })?;
    let path = stripped.split(',').next().unwrap_or(stripped);
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_unix_path_form() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn parses_unix_path_with_guid_suffix() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus,guid=deadbeef").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn rejects_tcp_scheme() {
        assert!(parse_unix_bus_address("tcp:host=localhost,port=1234").is_err());
    }

    #[test]
    fn rejects_abstract_scheme() {
        // We accept widening this later, but for now the error is explicit
        // so the user knows why the proxy didn't start.
        assert!(parse_unix_bus_address("unix:abstract=dbus-xyz").is_err());
    }
}
