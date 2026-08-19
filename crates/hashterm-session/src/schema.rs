//! Versioned dump manifest. Additive changes use `#[serde(default)]`; breaking
//! changes bump SCHEMA_VERSION and add a chained migration. Loading a manifest
//! newer than this binary understands is a hard error, never a partial restore.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DumpManifest {
    pub schema_version: u32,
    /// RFC3339 UTC (sort key / machine-readable).
    pub created_at: String,
    /// Local-time display string ("YYYY-MM-DD HH:MM"); empty on old saves.
    #[serde(default)]
    pub created_local: String,
    pub tmux_version: String,
    /// Largest attached client at dump time, for `new-session -x/-y`.
    pub client_size: (u16, u16),
    pub sessions: Vec<SessionDump>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDump {
    pub name: String,
    /// Display title (@hashterm_title).
    #[serde(default)]
    pub title: String,
    /// The user renamed this tab (title pinned against auto updates).
    #[serde(default)]
    pub title_custom: bool,
    /// Per-tab accent color "#rrggbb".
    #[serde(default)]
    pub accent: Option<String>,
    /// Tab group (workspace); None = the implicit default group.
    #[serde(default)]
    pub group: Option<String>,
    /// This member's copy of the group color "#rrggbb".
    #[serde(default)]
    pub group_color: Option<String>,
    pub active_window: u32,
    pub windows: Vec<WindowDump>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDump {
    pub index: u32,
    pub name: String,
    /// Verbatim #{window_layout} including tmux's own leading checksum.
    pub layout: String,
    pub active: bool,
    pub panes: Vec<PaneDump>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneDump {
    pub index: u32,
    pub active: bool,
    pub title: String,
    pub cwd: PathBuf,
    /// Pane was showing the alternate screen (vim, htop, ...) at dump time;
    /// scrollback replay is skipped for these, program restart used instead.
    pub alternate_on: bool,
    pub in_mode: bool,
    /// Deepest foreground process, if the pane wasn't idle at a prompt.
    pub fg: Option<FgProc>,
    /// Relative path (within the save dir) of the zstd scrollback dump.
    pub scrollback: Option<ScrollbackRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FgProc {
    /// #{pane_current_command} (comm, 15-char truncated) — fallback only.
    pub name: String,
    /// Exact argv from /proc/<pid>/cmdline.
    pub argv: Vec<String>,
    /// /proc/<pid>/cwd — where the program itself lives, may differ from pane cwd.
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollbackRef {
    pub file: PathBuf,
    pub lines: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_roundtrip() {
        let m = DumpManifest {
            schema_version: SCHEMA_VERSION,
            created_at: "2026-08-14T00:00:00Z".into(),
            created_local: "2026-08-14 00:00".into(),
            tmux_version: "3.5a".into(),
            client_size: (220, 50),
            sessions: vec![SessionDump {
                name: "ht-abc".into(),
                title: "work".into(),
                title_custom: true,
                accent: Some("#e06c75".into()),
                group: Some("work".into()),
                group_color: Some("#61afef".into()),
                active_window: 0,
                windows: vec![WindowDump {
                    index: 0,
                    name: "zsh".into(),
                    layout: "c3d4,220x50,0,0,1".into(),
                    active: true,
                    panes: vec![PaneDump {
                        index: 0,
                        active: true,
                        title: "".into(),
                        cwd: "/home".into(),
                        alternate_on: false,
                        in_mode: false,
                        fg: None,
                        scrollback: None,
                    }],
                }],
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: DumpManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sessions[0].windows[0].layout, "c3d4,220x50,0,0,1");
    }
}
