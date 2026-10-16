//! Colour schemes: terminal foreground/background, cursor, selection and the
//! 16-colour ANSI palette. Bundled files live in `assets/schemes/`; user files
//! in `~/.config/hashterm/schemes/NAME.toml` (data only, validated).
//!
//! The `default` scheme is the look hashterm always had: its colours come from
//! `[appearance] foreground/background` and it leaves VTE's built-in ANSI
//! palette alone, so nothing changes for anyone who never opts in.

use serde::Deserialize;
use std::sync::atomic::{AtomicU8, Ordering};

pub const DEFAULT_ID: &str = "default";
pub const DEFAULT_FG: &str = "#c5c8c6";
pub const DEFAULT_BG: &str = "#1d1f21";

const BUNDLED: &[(&str, &str)] = &[
    (
        "tomorrow-night",
        include_str!("../../../assets/schemes/tomorrow-night.toml"),
    ),
    (
        "dracula",
        include_str!("../../../assets/schemes/dracula.toml"),
    ),
    ("nord", include_str!("../../../assets/schemes/nord.toml")),
    (
        "solarized-dark",
        include_str!("../../../assets/schemes/solarized-dark.toml"),
    ),
    (
        "solarized-light",
        include_str!("../../../assets/schemes/solarized-light.toml"),
    ),
    (
        "gruvbox-dark",
        include_str!("../../../assets/schemes/gruvbox-dark.toml"),
    ),
    (
        "gruvbox-light",
        include_str!("../../../assets/schemes/gruvbox-light.toml"),
    ),
    (
        "catppuccin-mocha",
        include_str!("../../../assets/schemes/catppuccin-mocha.toml"),
    ),
    (
        "catppuccin-latte",
        include_str!("../../../assets/schemes/catppuccin-latte.toml"),
    ),
    (
        "tokyo-night",
        include_str!("../../../assets/schemes/tokyo-night.toml"),
    ),
    (
        "one-half-dark",
        include_str!("../../../assets/schemes/one-half-dark.toml"),
    ),
    (
        "atom-one-light",
        include_str!("../../../assets/schemes/atom-one-light.toml"),
    ),
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scheme {
    pub name: String,
    #[serde(default)]
    pub dark: bool,
    pub foreground: String,
    pub background: String,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    /// Empty (keep VTE's palette) or exactly 16 colours.
    #[serde(default)]
    pub palette: Vec<String>,
}

fn is_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

impl Scheme {
    pub fn validate(&self) -> Result<(), String> {
        for (what, v) in [
            ("foreground", &self.foreground),
            ("background", &self.background),
        ] {
            if !is_hex(v) {
                return Err(format!("{what} '{v}' is not #rrggbb"));
            }
        }
        for (what, v) in [("cursor", &self.cursor), ("selection", &self.selection)] {
            if let Some(v) = v
                && !is_hex(v)
            {
                return Err(format!("{what} '{v}' is not #rrggbb"));
            }
        }
        if !(self.palette.is_empty() || self.palette.len() == 16) {
            return Err(format!(
                "palette has {} colours, need 0 or 16",
                self.palette.len()
            ));
        }
        if let Some(bad) = self.palette.iter().find(|c| !is_hex(c)) {
            return Err(format!("palette colour '{bad}' is not #rrggbb"));
        }
        Ok(())
    }

    /// The built-in look; colours come from the config.
    pub fn default_look() -> Self {
        Scheme {
            name: "Default".into(),
            dark: true,
            foreground: DEFAULT_FG.into(),
            background: DEFAULT_BG.into(),
            cursor: None,
            selection: None,
            palette: Vec::new(),
        }
    }
}

/// Ids of the bundled schemes (excluding `default`).
pub fn bundled_ids() -> Vec<&'static str> {
    BUNDLED.iter().map(|(id, _)| *id).collect()
}

pub fn bundled(id: &str) -> Option<Scheme> {
    let (_, src) = BUNDLED.iter().find(|(i, _)| *i == id)?;
    toml::from_str(src).ok()
}

/// A user scheme: the id must be a plain file stem (no path tricks).
pub fn user(id: &str, dir: &std::path::Path) -> Option<Scheme> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // A scheme is a small data file. Refuse anything that is not a regular
    // file (a FIFO would block, /dev/zero would never end) and never read
    // more than the cap, whatever the file claims to be.
    use std::io::Read;
    const CAP: u64 = 16 * 1024;
    let file = std::fs::File::open(dir.join(format!("{id}.toml"))).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut src = String::new();
    file.take(CAP + 1).read_to_string(&mut src).ok()?;
    if src.len() as u64 > CAP {
        return None;
    }
    let s: Scheme = toml::from_str(&src).ok()?;
    s.validate().ok()?;
    Some(s)
}

/// 0 = unknown, 1 = dark, 2 = light (set by the desktop's colour-scheme watcher).
static SYSTEM: AtomicU8 = AtomicU8::new(0);

pub fn set_system_dark(dark: Option<bool>) {
    SYSTEM.store(
        match dark {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn system_dark() -> Option<bool> {
    match SYSTEM.load(Ordering::Relaxed) {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    }
}

/// Everything the UI needs to colour a terminal, after applying the scheme
/// choice, follow-system, and the explicit foreground/background overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub id: String,
    pub dark: bool,
    pub foreground: String,
    pub background: String,
    pub cursor: Option<String>,
    pub selection: Option<String>,
    pub palette: Vec<String>,
}

/// `fg`/`bg`: the config's `[appearance] foreground/background`. They override
/// the scheme only when they differ from the shipped defaults.
pub fn resolve(
    scheme_id: &str,
    follow_system: bool,
    dark_id: &str,
    light_id: &str,
    fg: &str,
    bg: &str,
    user_dir: &std::path::Path,
) -> Resolved {
    let id = if follow_system {
        match system_dark() {
            Some(false) => light_id,
            _ => dark_id,
        }
    } else {
        scheme_id
    };
    let (scheme, is_default) = if id == DEFAULT_ID {
        (Scheme::default_look(), true)
    } else if let Some(s) = bundled(id).or_else(|| user(id, user_dir)) {
        (s, false)
    } else {
        tracing::warn!("unknown or invalid colour scheme '{id}': using the default look");
        (Scheme::default_look(), true)
    };
    // An explicit colour must be #rrggbb: it also feeds CSS and tmux styles.
    let explicit = |value: &str, shipped: &str, from_scheme: &str| {
        if !is_hex(value) {
            tracing::warn!("colour '{value}' is not #rrggbb: ignored");
            return from_scheme.to_string();
        }
        if is_default || !value.eq_ignore_ascii_case(shipped) {
            value.to_string()
        } else {
            from_scheme.to_string()
        }
    };
    let foreground = explicit(fg, DEFAULT_FG, &scheme.foreground);
    let background = explicit(bg, DEFAULT_BG, &scheme.background);
    Resolved {
        id: id.to_string(),
        // The default look has no dark flag of its own: judge the colours.
        dark: if is_default {
            is_dark(&background)
        } else {
            scheme.dark
        },
        foreground,
        background,
        cursor: scheme.cursor,
        selection: scheme.selection,
        palette: scheme.palette,
    }
}

/// GNOME's `color-scheme`: only an explicit "prefer-dark" is dark. "default"
/// (no preference) renders light on stock GNOME.
pub fn dark_from_gnome(value: &str) -> bool {
    value == "prefer-dark"
}

/// Is `#rrggbb` a dark colour (relative luminance below 0.4)?
pub fn is_dark(hex: &str) -> bool {
    let c = |i: usize| {
        let v =
            u8::from_str_radix(hex.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0) as f64 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * c(1) + 0.7152 * c(3) + 0.0722 * c(5) < 0.4
}

/// tmux options that draw inside the terminal (copy-mode, search matches,
/// pane borders) in the scheme's colours: `(option, value)`.
pub fn tmux_styles(r: &Resolved) -> Vec<(&'static str, String)> {
    let pal = |i: usize, fallback: &str| {
        r.palette
            .get(i)
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    };
    let sel = r.selection.clone().unwrap_or_else(|| {
        if r.dark {
            "#373b41".into()
        } else {
            "#c8c8c8".into()
        }
    });
    vec![
        ("mode-style", format!("bg={sel},fg={}", r.foreground)),
        (
            "copy-mode-match-style",
            format!("bg={},fg={}", pal(3, "#f0c674"), r.background),
        ),
        (
            "copy-mode-current-match-style",
            format!("bg={},fg={}", pal(11, "#de935f"), r.background),
        ),
        ("pane-border-style", format!("fg={}", pal(8, "#707880"))),
        (
            "pane-active-border-style",
            format!("fg={}", pal(4, "#81a2be")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_scheme_parses_validates_and_has_a_full_palette() {
        let ids = bundled_ids();
        assert!(ids.len() >= 12);
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(id), "duplicate id {id}");
            let s = bundled(id).unwrap_or_else(|| panic!("{id} does not parse"));
            s.validate().unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(s.palette.len(), 16, "{id}");
            assert!(s.cursor.is_some() && s.selection.is_some(), "{id}");
        }
    }

    #[test]
    fn light_schemes_are_marked_light() {
        assert!(!bundled("solarized-light").unwrap().dark);
        assert!(!bundled("atom-one-light").unwrap().dark);
        assert!(bundled("dracula").unwrap().dark);
    }

    #[test]
    fn default_scheme_uses_the_configs_colours_and_keeps_vtes_palette() {
        let r = resolve(
            DEFAULT_ID,
            false,
            "default",
            "default",
            "#112233",
            "#445566",
            std::path::Path::new("/nonexistent"),
        );
        assert_eq!(r.foreground, "#112233");
        assert_eq!(r.background, "#445566");
        assert!(r.palette.is_empty());
    }

    #[test]
    fn explicit_colours_override_a_scheme_only_when_not_the_shipped_default() {
        let dir = std::path::Path::new("/nonexistent");
        let r = resolve(
            "dracula", false, "default", "default", DEFAULT_FG, DEFAULT_BG, dir,
        );
        assert_eq!(r.background, "#282a36");
        let r = resolve(
            "dracula", false, "default", "default", "#ffffff", DEFAULT_BG, dir,
        );
        assert_eq!(r.foreground, "#ffffff");
        assert_eq!(r.background, "#282a36");
    }

    #[test]
    fn follow_system_picks_by_the_desktop_preference() {
        let dir = std::path::Path::new("/nonexistent");
        set_system_dark(Some(false));
        let r = resolve(
            "dracula",
            true,
            "dracula",
            "solarized-light",
            DEFAULT_FG,
            DEFAULT_BG,
            dir,
        );
        assert_eq!(r.id, "solarized-light");
        set_system_dark(Some(true));
        let r = resolve(
            "nord",
            true,
            "dracula",
            "solarized-light",
            DEFAULT_FG,
            DEFAULT_BG,
            dir,
        );
        assert_eq!(r.id, "dracula");
        set_system_dark(None);
    }

    #[test]
    fn a_user_scheme_named_default_keeps_its_own_colours() {
        let d = std::env::temp_dir().join(format!("ht-scheme-def-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("mine.toml"),
            "name='Default'\nforeground='#010203'\nbackground='#040506'\n",
        )
        .unwrap();
        let r = resolve("mine", false, "x", "y", DEFAULT_FG, DEFAULT_BG, &d);
        assert_eq!(r.background, "#040506");
        assert_eq!(r.foreground, "#010203");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn invalid_explicit_colours_are_ignored_and_darkness_follows_the_background() {
        let none = std::path::Path::new("/nonexistent");
        let r = resolve(DEFAULT_ID, false, "x", "y", "white", "rgb(1,2,3)", none);
        assert_eq!(r.foreground, DEFAULT_FG);
        assert_eq!(r.background, DEFAULT_BG);
        let r = resolve(DEFAULT_ID, false, "x", "y", "#111111", "#fdf6e3", none);
        assert!(!r.dark, "a light background is not dark");
        assert!(is_dark("#1d1f21") && !is_dark("#ffffff"));
    }

    #[test]
    fn gnome_color_scheme_mapping() {
        assert!(dark_from_gnome("prefer-dark"));
        assert!(!dark_from_gnome("default"));
        assert!(!dark_from_gnome("prefer-light"));
    }

    #[test]
    fn oversized_or_special_scheme_files_are_refused() {
        let d = std::env::temp_dir().join(format!("ht-scheme-big-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("big.toml"), "#".repeat(20_000)).unwrap();
        assert!(user("big", &d).is_none());
        std::os::unix::fs::symlink("/dev/zero", d.join("zero.toml")).unwrap();
        assert!(user("zero", &d).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn unknown_scheme_falls_back_to_the_default_look() {
        let r = resolve(
            "no-such",
            false,
            "default",
            "default",
            DEFAULT_FG,
            DEFAULT_BG,
            std::path::Path::new("/nonexistent"),
        );
        assert!(r.palette.is_empty());
    }

    #[test]
    fn user_scheme_ids_cannot_escape_the_directory() {
        let d = std::env::temp_dir().join(format!("ht-scheme-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("mine.toml"),
            "name='Mine'\nforeground='#ffffff'\nbackground='#000000'\n",
        )
        .unwrap();
        assert!(user("mine", &d).is_some());
        assert!(user("../mine", &d).is_none());
        assert!(user("", &d).is_none());
        std::fs::write(
            d.join("bad.toml"),
            "name='x'\nforeground='red'\nbackground='#000000'\n",
        )
        .unwrap();
        assert!(user("bad", &d).is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn tmux_styles_follow_the_palette() {
        let r = resolve(
            "dracula",
            false,
            "default",
            "default",
            DEFAULT_FG,
            DEFAULT_BG,
            std::path::Path::new("/nonexistent"),
        );
        let styles = tmux_styles(&r);
        let get = |k: &str| styles.iter().find(|(o, _)| *o == k).unwrap().1.clone();
        assert_eq!(get("copy-mode-match-style"), "bg=#f1fa8c,fg=#282a36");
        assert_eq!(get("pane-active-border-style"), "fg=#bd93f9");
        assert!(get("mode-style").starts_with("bg=#44475a"));
        assert_eq!(styles.len(), 5);
    }
}
