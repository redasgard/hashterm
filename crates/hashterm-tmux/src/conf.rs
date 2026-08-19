//! Bundled tmux configuration management. The conf is embedded in the binary
//! and written to the data dir at startup; a content hash in the filename-adjacent
//! marker avoids rewriting (and lets us detect upgrades).

use std::path::{Path, PathBuf};

pub const BUNDLED_CONF: &str = include_str!("../../../assets/hashterm.tmux.conf");

/// Write the bundled conf (plus dynamic overrides) into `data_dir`, returning
/// its path. `history_limit` comes from user config; `native_windows` removes
/// the GUI tab keybinding remaps and re-enables the tmux status line.
pub fn install_conf(
    data_dir: &Path,
    history_limit: u32,
    native_windows: bool,
    extra_conf: Option<&Path>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    let mut conf = String::from(BUNDLED_CONF);
    // Hooks reference `hashterm-ctl` by name; during development (cargo run)
    // it is not on PATH, so point them at the binary next to our own exe.
    if let Some(ctl) = sibling_ctl_path() {
        conf = conf.replace("hashterm-ctl ", &format!("{} ", ctl.display()));
    }
    // Bridge tmux copy-mode (mouse drags, y in copy-mode) to the SYSTEM
    // clipboard: tmux >= 3.2 pipes copies through copy-command when set.
    if let Some(tool) = clipboard_tool() {
        conf.push_str(&format!("\nset -s copy-command '{tool}'\n"));
    }
    conf.push_str(&format!("\nset -g history-limit {history_limit}\n"));
    if native_windows {
        conf.push_str(
            "\n# user override: native tmux windows\n\
             set -g status on\n\
             unbind-key c\nunbind-key n\nunbind-key p\n\
             bind-key c new-window\nbind-key n next-window\nbind-key p previous-window\n",
        );
    }
    if let Some(extra) = extra_conf {
        // Single-quote the path so spaces / tmux specials don't misparse.
        let quoted = extra.display().to_string().replace('\'', "'\\''");
        conf.push_str(&format!("\nsource-file '{quoted}'\n"));
    }
    let path = data_dir.join("hashterm.tmux.conf");
    // Rewrite only on content change so tmux never sees a partial file.
    if std::fs::read_to_string(&path).ok().as_deref() != Some(conf.as_str()) {
        let tmp = data_dir.join(".hashterm.tmux.conf.tmp");
        std::fs::write(&tmp, &conf)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path)
}

/// First available system clipboard writer, preferring the Wayland-native one
/// (works from XWayland apps too — it talks to the compositor directly).
pub fn clipboard_tool() -> Option<&'static str> {
    let candidates = [
        ("wl-copy", "wl-copy"),
        ("xclip", "xclip -selection clipboard -i"),
        ("xsel", "xsel --clipboard --input"),
    ];
    let path = std::env::var_os("PATH")?;
    for (bin, cmdline) in candidates {
        if std::env::split_paths(&path).any(|d| d.join(bin).is_file()) {
            return Some(cmdline);
        }
    }
    None
}

/// `hashterm-ctl` living beside the current executable, if any (dev builds,
/// installs to the same bindir). None -> rely on PATH.
fn sibling_ctl_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("hashterm-ctl");
    candidate.is_file().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_written_and_stable() {
        let dir = std::env::temp_dir().join(format!("hashterm-conf-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p1 = install_conf(&dir, 10000, false, None).unwrap();
        let m1 = std::fs::metadata(&p1).unwrap().modified().unwrap();
        let p2 = install_conf(&dir, 10000, false, None).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(m1, std::fs::metadata(&p2).unwrap().modified().unwrap());
        let text = std::fs::read_to_string(&p1).unwrap();
        assert!(text.contains("history-limit 10000"));
        assert!(text.contains("status off"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_windows_appends_overrides() {
        let dir =
            std::env::temp_dir().join(format!("hashterm-conf-test-nw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = install_conf(&dir, 5000, true, None).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("set -g status on"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
