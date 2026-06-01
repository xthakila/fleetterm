//! FleetTerm daemon library: owns PTYs + session state, enforces autonomy, and serves
//! the UI/hook unix socket. See `~/.claude/plans/lively-puzzling-pretzel.md`.

pub mod claude;
pub mod daemon;
pub mod framed;
pub mod hooks;
pub mod pty;
pub mod registry;
pub mod server;
pub mod tools;
