//! Stylesheet loading: bundled base CSS at APPLICATION priority, optional
//! ~/.config/hashterm/user.css at USER priority, plus a generated layer for
//! config-derived values (colors, opacity, padding) re-generated on live
//! reload. Transparency is done via an rgba() BACKGROUND on the window node —
//! never via `opacity`, which would fade the text too.

use gtk4::gdk;
use hashterm_core::config::Config;

const BASE_CSS: &str = include_str!("../../../assets/style.css");

pub struct Styles {
    generated: gtk4::CssProvider,
}

impl Styles {
    pub fn install(display: &gdk::Display, config: &Config) -> Self {
        let base = gtk4::CssProvider::new();
        base.load_from_string(BASE_CSS);
        gtk4::style_context_add_provider_for_display(
            display,
            &base,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let user_css = Config::config_dir().join("user.css");
        if user_css.is_file() {
            let user = gtk4::CssProvider::new();
            user.load_from_path(&user_css);
            gtk4::style_context_add_provider_for_display(
                display,
                &user,
                gtk4::STYLE_PROVIDER_PRIORITY_USER,
            );
        }

        let generated = gtk4::CssProvider::new();
        gtk4::style_context_add_provider_for_display(
            display,
            &generated,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        let styles = Self { generated };
        styles.apply(config);
        styles
    }

    /// Re-generate the config-derived layer (safe to call on live reload).
    pub fn apply(&self, config: &Config) {
        let a = &config.appearance;
        let alpha = a.opacity.clamp(0.05, 1.0);
        let bar_alpha = a.tabbar_opacity.clamp(0.0, 1.0);
        let (r, g, b) = parse_hex(&a.background).unwrap_or((0x1d, 0x1f, 0x21));
        // Tab bar sits slightly darker than the terminal, own alpha.
        let (tr, tg, tb) = (
            r.saturating_sub(7),
            g.saturating_sub(7),
            b.saturating_sub(7),
        );
        let css = format!(
            "window.hashterm {{ background-color: rgba({r},{g},{b},{alpha}); }}\n\
             .tabbar {{ background-color: rgba({tr},{tg},{tb},{bar_alpha}); }}\n\
             .terminal-page {{ padding: {}px; background: transparent; }}\n",
            a.padding
        );
        self.generated.load_from_string(&css);
    }
}

/// "#rrggbb" -> (r, g, b).
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_parse() {
        assert_eq!(super::parse_hex("#1d1f21"), Some((0x1d, 0x1f, 0x21)));
        assert_eq!(super::parse_hex("1d1f21"), None);
        assert_eq!(super::parse_hex("#fff"), None);
    }
}
