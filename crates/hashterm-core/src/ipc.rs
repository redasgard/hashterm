//! Event types shared between the GUI's unix-socket listener and `hashterm-ctl`
//! (which tmux `run-shell` hooks invoke). Wire format: one JSON object per line.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum TmuxEvent {
    SessionCreated {
        session: String,
    },
    SessionClosed {
        session: String,
    },
    SessionRenamed {
        session: String,
    },
    ClientAttached {
        session: String,
    },
    ClientDetached {
        session: String,
    },
    Bell {
        session: String,
    },
    Activity {
        session: String,
    },
    /// GUI actions driven from tmux keybindings (rebound prefix c/n/p).
    NewTab,
    NextTab,
    PrevTab,
}

/// Socket the GUI listens on: $XDG_RUNTIME_DIR/hashterm/ipc.sock (fallback /tmp/hashterm-$UID).
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("hashterm-{}", uid())))
        .join("hashterm");
    dir.join("ipc.sock")
}

// Minimal uid read without a libc dependency: parse /proc/self/status Uid line.
fn uid() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned))
        })
        .and_then(|u| u.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrip() {
        let ev = TmuxEvent::SessionClosed {
            session: "ht-0198".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<TmuxEvent>(&json).unwrap(), ev);
    }
}
