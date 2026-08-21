//! Layered configuration: defaults <- ~/.config/hashterm/config.toml <- CLI overrides.
//! Every field has a serde default so an empty (or absent) file is valid.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SideStyle {
    /// ~180px ellipsized title cards (VS Code-like).
    Cards,
    /// Narrow icon/index rail.
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorPick {
    /// Monitor containing the pointer at toggle time.
    Focused,
    Primary,
    /// Connector name, e.g. "DP-1".
    Connector(String),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub tabbar: TabBarCfg,
    pub groupbar: GroupBarCfg,
    pub scrollback: Scrollback,
    pub dropdown: Dropdown,
    pub hotkeys: Hotkeys,
    pub session: SessionCfg,
    pub restore: RestoreCfg,
    pub tmux: TmuxCfg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Shell to spawn in new panes; None = user's login shell.
    pub shell: Option<String>,
    /// Command run instead of the shell for new tabs, argv form.
    pub default_command: Option<Vec<String>>,
    /// Closing a tab kills its tmux session (saved sessions exempt).
    pub close_kills_session: bool,
    /// Prompt before closing a tab whose pane runs a foreground process.
    pub confirm_close_running: bool,
    /// New tabs inherit the cwd of the active tab (else $HOME).
    pub inherit_cwd: bool,
    /// Launch without showing the console; the first hotkey press summons it.
    pub start_hidden: bool,
    /// Maintain an XDG autostart entry so hashterm launches with the session.
    pub autostart: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            shell: None,
            default_command: None,
            close_kills_session: true,
            confirm_close_running: true,
            inherit_cwd: true,
            start_hidden: false,
            autostart: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    /// Pango font description, e.g. "JetBrains Mono 11".
    pub font: String,
    pub padding: u16,
    /// 0.0..=1.0 terminal BACKGROUND opacity (text stays opaque). X11 needs
    /// a compositor; Wayland always composites.
    pub opacity: f64,
    /// 0.0..=1.0 tab bar background opacity, independent of the terminal.
    pub tabbar_opacity: f64,
    /// 0.0..=1.0 WHOLE-WINDOW opacity — fades everything, text included.
    pub window_opacity: f64,
    /// Width (px) of the accent border drawn around the terminal for tabs
    /// with an accent color; 0 disables the border (tab underline remains).
    pub tab_border_width: u16,
    /// Terminal foreground color, "#rrggbb".
    pub foreground: String,
    /// Terminal/window background color, "#rrggbb".
    pub background: String,
    pub bold_is_bright: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            font: "monospace 11".into(),
            padding: 6,
            opacity: 1.0,
            tabbar_opacity: 1.0,
            window_opacity: 1.0,
            tab_border_width: 2,
            foreground: "#c5c8c6".into(),
            background: "#1d1f21".into(),
            bold_is_bright: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabBarCfg {
    pub edge: Edge,
    pub auto_hide: bool,
    /// Reveal/conceal animation duration.
    pub reveal_ms: u32,
    pub side_style: SideStyle,
    pub show_close: bool,
    /// Keep the bar revealed while any tab carries a badge.
    pub reveal_on_badge: bool,
    /// Centered translucent popup with the tab name after switching tabs.
    pub switch_osd: bool,
}

impl Default for TabBarCfg {
    fn default() -> Self {
        Self {
            edge: Edge::Top,
            auto_hide: false,
            reveal_ms: 180,
            side_style: SideStyle::Cards,
            show_close: true,
            reveal_on_badge: true,
            switch_osd: true,
        }
    }
}

/// The group (workspace) bar — the second axis of the tab matrix: groups on
/// one edge, the active group's tabs on the perpendicular edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GroupBarCfg {
    /// Edge for the group bar; None = automatically perpendicular to the tab
    /// bar (tabs top/bottom -> groups left; tabs left/right -> groups top).
    pub edge: Option<Edge>,
    /// Hide the group bar while only one group exists.
    pub hide_when_single: bool,
    /// Show tab counts next to group names.
    pub show_counts: bool,
    /// Reveal/conceal animation duration (ms).
    pub reveal_ms: u32,
}

impl Default for GroupBarCfg {
    fn default() -> Self {
        Self {
            edge: None,
            hide_when_single: true,
            show_counts: true,
            reveal_ms: 150,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scrollback {
    /// Forwarded into tmux history-limit on the dedicated server.
    pub lines: u32,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self { lines: 50_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dropdown {
    pub enabled: bool,
    pub edge: Edge,
    pub width_pct: f64,
    pub height_pct: f64,
    /// Explicit console height: "600px" or "45%". Overrides height_pct AND
    /// the remembered last-session height.
    pub height: Option<String>,
    pub animation_ms: u32,
    pub monitor: MonitorPick,
    pub hide_on_focus_loss: bool,
    /// Layer-shell only: "top" (default) or "overlay" (above fullscreen).
    pub layer: DropdownLayer,
    /// Respect panel exclusive zones (layer-shell exclusive-zone 0 vs -1).
    pub respect_panels: bool,
    /// On GNOME Wayland (no layer-shell, no client keep-above) run via
    /// XWayland so EWMH always-on-top works — the tilda/guake approach.
    pub prefer_xwayland: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DropdownLayer {
    Top,
    Overlay,
}

impl Default for Dropdown {
    fn default() -> Self {
        Self {
            enabled: true,
            edge: Edge::Top,
            width_pct: 1.0,
            height_pct: 0.45,
            height: None,
            animation_ms: 150,
            monitor: MonitorPick::Focused,
            hide_on_focus_loss: false,
            layer: DropdownLayer::Top,
            respect_panels: true,
            prefer_xwayland: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hotkeys {
    /// System-wide dropdown toggle (portal/XGrabKey/external bind).
    pub toggle_dropdown: String,
    pub new_tab: String,
    pub close_tab: String,
    pub next_tab: String,
    pub prev_tab: String,
    pub toggle_tabbar: String,
    pub session_picker: String,
    pub open_config: String,
    /// Copy the terminal selection to the clipboard.
    pub copy: String,
    /// Paste the clipboard into the terminal (Shift+Insert also works).
    pub paste: String,
    /// Modifier for direct tab switching: <mod>+1..9 and <mod>+0 for tab 10.
    /// Empty string disables.
    pub goto_tab_modifier: String,
    /// Cycle tab groups (workspaces); the bar shows only the active group.
    pub next_group: String,
    pub prev_group: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle_dropdown: "F12".into(),
            new_tab: "<Ctrl><Shift>t".into(),
            close_tab: "<Ctrl><Shift>w".into(),
            next_tab: "<Ctrl>End".into(),
            prev_tab: "<Ctrl>Home".into(),
            toggle_tabbar: "<Ctrl><Shift>b".into(),
            session_picker: "<Ctrl><Shift>s".into(),
            open_config: "<Ctrl>comma".into(),
            copy: "<Ctrl><Shift>c".into(),
            paste: "<Ctrl><Shift>v".into(),
            goto_tab_modifier: "<Alt>".into(),
            next_group: "<Ctrl>Page_Down".into(),
            prev_group: "<Ctrl>Page_Up".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionCfg {
    /// Seconds between autosaves; 0 disables.
    pub autosave_interval_secs: u64,
    /// Autosave snapshots retained.
    pub autosave_keep: u32,
    /// Silently restore the newest save at startup when the server is empty.
    /// Superseded by `prompt_on_start` unless that is false.
    pub restore_on_start: bool,
    /// Browser-style: when the previous run ended UNCLEANLY (crash, kill,
    /// reboot) and a recent save exists, ask whether to restore it at startup.
    pub prompt_on_start: bool,
}

impl Default for SessionCfg {
    fn default() -> Self {
        Self {
            autosave_interval_secs: 300,
            autosave_keep: 12,
            restore_on_start: false,
            prompt_on_start: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestoreCfg {
    /// basenames of programs auto-restarted on restore.
    pub programs: Vec<String>,
    /// Restart anything found running (dangerous; safe-list ignored).
    pub restart_arbitrary: bool,
    /// Browser-style: panes whose program was NOT auto-restarted get the
    /// captured command pre-typed at the prompt — Enter re-runs it.
    pub pretype_unrestored: bool,
    /// Patterns ("title:...", "cmd:...") whose panes skip scrollback capture.
    pub exclude_panes: Vec<String>,
    /// Cap captured scrollback lines per pane; 0 = unlimited.
    pub max_scrollback_lines: u32,
    /// Resume recipes: program basename -> the argv to run on restore instead
    /// of the captured command, so a program that persists its own state
    /// resumes it (e.g. claude -> `claude --continue`) rather than starting
    /// fresh. TRUSTED (user config): unlike a captured command from an
    /// untrusted save, a recipe may run a program from anywhere on your PATH.
    pub resume: std::collections::HashMap<String, Vec<String>>,
}

impl Default for RestoreCfg {
    fn default() -> Self {
        Self {
            programs: [
                "vim",
                "nvim",
                "htop",
                "btop",
                "top",
                "tail",
                "watch",
                "journalctl",
                "man",
                "less",
                "ssh",
                "lazygit",
                "k9s",
                "psql",
                "jupyter",
                "ipython",
                "mysql",
                "sqlite3",
                "redis-cli",
                "mongosh",
                "tig",
                "gitui",
                "ranger",
                "nnn",
                "lf",
                "yazi",
                "glances",
                "bpytop",
                "weechat",
                "irssi",
                "aider",
                "ncdu",
                "mosh",
                "mosh-client",
            ]
            .map(String::from)
            .to_vec(),
            restart_arbitrary: false,
            pretype_unrestored: true,
            exclude_panes: Vec::new(),
            max_scrollback_lines: 0,
            resume: [("claude".to_string(), vec![
                "claude".to_string(),
                "--continue".to_string(),
            ])]
            .into_iter()
            .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TmuxCfg {
    /// Stock tmux window keybindings inside a tab (re-enables tmux status line).
    pub native_windows: bool,
    /// Extra conf sourced after the bundled one.
    pub extra_conf: Option<PathBuf>,
}

/// Humanize accelerator strings: users type "<Ctrl><Shift>`" but GTK only
/// accepts keysym names ("<Ctrl><Shift>grave"). Maps a single trailing
/// punctuation character to its X11 keysym name; everything else untouched.
pub fn normalize_accel(accel: &str) -> String {
    let split = accel.rfind('>').map(|i| i + 1).unwrap_or(0);
    let (mods, key) = accel.split_at(split);
    let named = match key {
        "`" => "grave",
        "~" => "asciitilde",
        "!" => "exclam",
        "@" => "at",
        "#" => "numbersign",
        "$" => "dollar",
        "%" => "percent",
        "^" => "asciicircum",
        "&" => "ampersand",
        "*" => "asterisk",
        "(" => "parenleft",
        ")" => "parenright",
        "-" => "minus",
        "_" => "underscore",
        "=" => "equal",
        "[" => "bracketleft",
        "]" => "bracketright",
        "{" => "braceleft",
        "}" => "braceright",
        "\\" => "backslash",
        "|" => "bar",
        ";" => "semicolon",
        ":" => "colon",
        "'" => "apostrophe",
        "\"" => "quotedbl",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        "?" => "question",
        " " => "space",
        _ => return accel.to_owned(),
    };
    format!("{mods}{named}")
}

/// A size given either in pixels or as a fraction of the monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SizeSpec {
    Px(i32),
    Pct(f64),
}

/// Parse "600px", "600", or "45%" (percent of the monitor dimension).
pub fn parse_size(s: &str) -> Option<SizeSpec> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let v: f64 = pct.trim().parse().ok()?;
        (0.0..=100.0)
            .contains(&v)
            .then_some(SizeSpec::Pct(v / 100.0))
    } else {
        let px = s.strip_suffix("px").unwrap_or(s).trim();
        let v: i32 = px.parse().ok()?;
        (v > 0).then_some(SizeSpec::Px(v))
    }
}

impl Config {
    pub fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("hashterm")
    }

    pub fn default_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    /// The commented reference config shipped in the binary.
    pub const DEFAULT_TOML: &str = include_str!("../../../assets/default-config.toml");

    /// Write the commented default config if none exists yet, so users have a
    /// discoverable, documented file to edit. Errors are non-fatal.
    pub fn install_default_file() -> Option<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = Self::default_path();
        std::fs::create_dir_all(Self::config_dir()).ok()?;
        // create_new + O_NOFOLLOW: only ever create a fresh regular file. If
        // something (a symlink to ~/.bashrc, an existing config) is already
        // there, do nothing — never clobber or write through a symlink.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .ok()?;
        f.write_all(Self::DEFAULT_TOML.as_bytes()).ok()?;
        Some(path)
    }

    /// Load from `path`; a missing file yields defaults, a broken file errors.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(ConfigError::Io {
                    path: path.into(),
                    source: e,
                });
            }
        };
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.into(),
            source: Box::new(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_is_valid() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_section_keeps_other_defaults() {
        let cfg: Config = toml::from_str("[tabbar]\nedge = \"left\"\nauto_hide = true\n").unwrap();
        assert_eq!(cfg.tabbar.edge, Edge::Left);
        assert!(cfg.tabbar.auto_hide);
        assert_eq!(cfg.appearance, Appearance::default());
    }

    #[test]
    fn groupbar_section_parses() {
        let cfg: Config =
            toml::from_str("[groupbar]\nedge = \"top\"\nhide_when_single = false\n").unwrap();
        assert_eq!(cfg.groupbar.edge, Some(Edge::Top));
        assert!(!cfg.groupbar.hide_when_single);
        assert!(cfg.groupbar.show_counts);
        // Unset edge = automatic perpendicular placement (resolved in the UI).
        assert_eq!(GroupBarCfg::default().edge, None);
    }

    #[test]
    fn unknown_key_rejected() {
        assert!(toml::from_str::<Config>("[general]\nnope = 1\n").is_err());
    }

    #[test]
    fn shipped_default_config_matches_code_defaults() {
        // Guards the commented reference file against schema drift: it must
        // parse cleanly AND produce exactly the built-in defaults.
        let parsed: Config = toml::from_str(Config::DEFAULT_TOML).expect("default toml parses");
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn accel_normalization() {
        assert_eq!(normalize_accel("<Ctrl><Shift>`"), "<Ctrl><Shift>grave");
        assert_eq!(normalize_accel("<Ctrl><Shift>~"), "<Ctrl><Shift>asciitilde");
        assert_eq!(normalize_accel("<Ctrl>,"), "<Ctrl>comma");
        assert_eq!(normalize_accel("F12"), "F12");
        assert_eq!(normalize_accel("<Ctrl><Shift>t"), "<Ctrl><Shift>t");
        assert_eq!(normalize_accel("<Super>space"), "<Super>space");
    }

    #[test]
    fn size_spec_parsing() {
        assert_eq!(parse_size("600px"), Some(SizeSpec::Px(600)));
        assert_eq!(parse_size("600"), Some(SizeSpec::Px(600)));
        assert_eq!(parse_size("45%"), Some(SizeSpec::Pct(0.45)));
        assert_eq!(parse_size(" 33 % "), Some(SizeSpec::Pct(0.33)));
        assert_eq!(parse_size("150%"), None);
        assert_eq!(parse_size("-5px"), None);
        assert_eq!(parse_size("abc"), None);
    }

    #[test]
    fn roundtrip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }
}
