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

fn uid() -> u32 {
    // getuid() never fails; the old /proc parse fell back to 0 (root) on any
    // read failure, collapsing every user onto /tmp/hashterm-0.
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
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
