//! Headless host: socket transport, plugin supervisor, action registry.
//! Wire protocol: see `docs/gui-daemon-protocol.md`.

pub mod gui_registry;
pub mod plugin_exec;
pub mod service_supervisor;
pub mod socket;
pub mod trigger_pump;
pub mod trigger_sink;
