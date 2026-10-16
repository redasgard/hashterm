//! GTK-free foundation: config schema, shared error/IPC types, logging.

pub mod config;
pub mod ipc;
pub mod logfile;
pub mod scheme;
pub mod settings;
pub mod state;

pub const APP_ID: &str = "com.redasgard.Hashterm";
pub const SOCKET_NAME: &str = "hashterm";

/// tmux socket name; `HASHTERM_SOCKET` overrides for tests/parallel instances.
/// The override is honored only in debug builds — in a release binary an
/// attacker-set env must not be able to redirect our control socket.
pub fn socket_name() -> String {
    if cfg!(debug_assertions)
        && let Ok(v) = std::env::var("HASHTERM_SOCKET")
    {
        return v;
    }
    SOCKET_NAME.into()
}

/// GApplication id; `HASHTERM_APP_ID` overrides so a test instance can run
/// beside the real one without D-Bus single-instance collapsing them.
/// Debug-only, so a release binary can't be pushed onto a different bus name
/// (which would let an impostor own the canonical name — see the name-squat
/// finding).
pub fn app_id() -> String {
    if cfg!(debug_assertions)
        && let Ok(v) = std::env::var("HASHTERM_APP_ID")
    {
        return v;
    }
    APP_ID.into()
}

/// Initialize tracing (stderr and the state-dir log file) and the panic hook.
pub fn init_logging() {
    logfile::init();
}
