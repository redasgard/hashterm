//! Small persisted UI state (NOT user configuration — the config file is
//! user-owned and never rewritten). Lives in the data dir as state.json.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Window height (logical px) at last hide/close; restored at startup
    /// unless the config pins an explicit dropdown.height.
    pub window_height_px: Option<i32>,
    /// Tab group shown when the app was last running.
    pub active_group: Option<String>,
    /// Display order of groups (names not listed sort after, first-seen).
    pub group_order: Vec<String>,
    /// Per-group last-selected tab (group name -> session name).
    pub last_active_tab: std::collections::HashMap<String, String>,
    /// Runtime whole-window opacity set via Ctrl+Alt+Shift+scroll; overrides
    /// appearance.window_opacity until that config value itself is edited.
    pub window_opacity: Option<f64>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("hashterm")
}

fn path() -> PathBuf {
    data_dir().join("state.json")
}

fn lock_path() -> PathBuf {
    data_dir().join("running.lock")
}

/// Browser-style unclean-exit detection. Returns true if the previous run did
/// NOT exit cleanly (the lock from that run is still present), then arms the
/// lock for this run. A clean exit calls `clear_session_lock`; a crash / kill
/// / reboot leaves the lock behind, so the next start sees it.
pub fn take_unclean_and_arm() -> bool {
    let lock = lock_path();
    let was_unclean = lock.exists();
    if let Some(dir) = lock.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&lock, b"running");
    was_unclean
}

/// Mark a clean shutdown so the next start does not offer to restore.
pub fn clear_session_lock() {
    let _ = std::fs::remove_file(lock_path());
}

impl UiState {
    pub fn load() -> Self {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Best-effort persist; UI state loss is never worth an error dialog.
    pub fn save(&self) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(&p, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip_via_serde() {
        let s = UiState {
            window_height_px: Some(640),
            active_group: Some("work".into()),
            group_order: vec!["main".into(), "work".into()],
            last_active_tab: [("work".into(), "ht-1".into())].into(),
            window_opacity: Some(0.85),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: UiState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.window_height_px, Some(640));
        // Unknown future fields must not break loading.
        let fwd: UiState = serde_json::from_str(r#"{"window_height_px":10,"later":1}"#).unwrap();
        assert_eq!(fwd.window_height_px, Some(10));
    }
}
