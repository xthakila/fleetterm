//! `fleetd` — the FleetTerm daemon. Owns agent PTYs + fleet state and serves a unix
//! socket for the UI and the hook forwarder. Runs headless; the UI is a separate client.

use std::path::PathBuf;

use fleetd::daemon::Daemon;
use fleetd::server;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let base = runtime_base();
    let sock_path = base.join("fleetd.sock");
    let run_dir = base.join("sessions");
    let hook_bin = resolve_hook_bin();

    tracing::info!("hook binary: {}", hook_bin.display());
    let daemon = Daemon::new(sock_path, run_dir, hook_bin);
    server::serve(daemon).await
}

/// `$XDG_RUNTIME_DIR/fleetterm` (falls back to a temp dir).
fn runtime_base() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("fleetterm")
}

/// Locate the `fleetterm-hook` binary: explicit override, else a sibling of this exe.
fn resolve_hook_bin() -> PathBuf {
    if let Ok(p) = std::env::var("FLEETTERM_HOOK_BIN") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("fleetterm-hook");
        }
    }
    PathBuf::from("fleetterm-hook")
}
