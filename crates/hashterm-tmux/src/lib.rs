//! tmux integration for the dedicated `-L hashterm` server: controller (commands),
//! event bus (hook-fed unix socket), server-state reconciliation. GTK-free.

pub mod conf;
pub mod controller;
pub mod events;

pub use controller::{TmuxController, TmuxError, TmuxSessionId, unique_session_name};
pub use events::TmuxEventBus;
