//! GTK-free foundation: config schema, shared error/IPC types, logging.

pub mod config;
pub mod ipc;
pub mod state;

pub const APP_ID: &str = "com.redasgard.Hashterm";
pub const SOCKET_NAME: &str = "hashterm";

/// tmux socket name; `HASHTERM_SOCKET` overrides for tests/parallel instances.
pub fn socket_name() -> String {
    std::env::var("HASHTERM_SOCKET").unwrap_or_else(|_| SOCKET_NAME.into())
}

/// GApplication id; `HASHTERM_APP_ID` overrides so a test instance can run
/// beside the real one without D-Bus single-instance collapsing them.
pub fn app_id() -> String {
    std::env::var("HASHTERM_APP_ID").unwrap_or_else(|_| APP_ID.into())
}

/// Initialize tracing to stderr, honoring `HASHTERM_LOG` (fallback: `RUST_LOG`, then "info").
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("HASHTERM_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_writer(std::io::stderr)
        .init();
}
