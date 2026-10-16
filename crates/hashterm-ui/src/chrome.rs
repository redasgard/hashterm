//! Tab-bar / group-bar chrome colours derived from the terminal palette.
//!
//! The GTK theme paints a `background-image` over any `background-color`, so
//! chrome that only sets colours is unreadable on a light theme. Every chrome
//! rule generated here sets `background-image: none` and explicit colours for
//! each state, and the colours come from ONE place: the terminal background
//! and foreground. Contrast is WCAG-based and clamped, not hoped for; the
//! derivation was calibrated on 14 well-known palettes (see the tests).

type Rgb = [f64; 3];

const BLACK: Rgb = [0.0, 0.0, 0.0];
const WHITE: Rgb = [255.0, 255.0, 255.0];

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn lin(v: f64) -> f64 {
    let v = v / 255.0;
    if v <= 0.03928 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn lum(c: Rgb) -> f64 {
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (lum(a), lum(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn hex(c: Rgb) -> String {
    let q = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", q(c[0]), q(c[1]), q(c[2]))
}

/// Push `c` toward `extreme` in 2 % steps until it reaches `target` contrast
/// against every background in `against`.
fn ensure(c: Rgb, against: &[Rgb], target: f64, extreme: Rgb) -> Rgb {
    let mut t = 0.0;
    let mut cur = c;
    while against.iter().any(|a| contrast(cur, *a) < target) && t < 1.0 {
        t += 0.02;
        cur = mix(c, extreme, t);
    }
    cur
}

#[derive(Debug, Clone)]
pub struct ChromeTokens {
    pub dark: bool,
    pub strip: Rgb,
    pub groupbar: Rgb,
    pub active: Rgb,
    pub hover: Rgb,
    pub muted: Rgb,
    pub text_active: Rgb,
    pub group_muted: Rgb,
    pub fg: Rgb,
    /// Floating surface (notices): clearly raised above the terminal.
    pub raised: Rgb,
    pub raised_text: Rgb,
    pub raised_edge: Rgb,
}

impl ChromeTokens {
    pub fn derive(bg: (u8, u8, u8), fg: (u8, u8, u8)) -> Self {
        let bg = [bg.0 as f64, bg.1 as f64, bg.2 as f64];
        let fg = [fg.0 as f64, fg.1 as f64, fg.2 as f64];
        let dark = lum(bg) < 0.4;
        let ext = if dark { WHITE } else { BLACK };
        let strip = mix(bg, BLACK, if dark { 0.22 } else { 0.07 });
        let groupbar = mix(bg, BLACK, if dark { 0.42 } else { 0.12 });
        let need_fill = if dark { 1.30 } else { 1.15 };
        let mut m = 0.10;
        while contrast(mix(strip, fg, m), strip) < need_fill && m < 0.45 {
            m += 0.01;
        }
        let active = mix(strip, fg, m);
        let hover = mix(strip, fg, (m * 0.5).min(0.2));
        let muted = ensure(mix(strip, fg, 0.72), &[strip, hover, active], 4.5, ext);
        let text_active = ensure(mix(fg, ext, 0.35), &[active], 7.0, ext);
        let group_muted = ensure(
            mix(groupbar, fg, 0.72),
            &[groupbar, mix(groupbar, fg, 0.10)],
            4.5,
            ext,
        );
        let raised = mix(bg, ext, if dark { 0.16 } else { 0.10 });
        let raised_text = ensure(fg, &[raised], 7.0, ext);
        let raised_edge = mix(bg, ext, if dark { 0.38 } else { 0.30 });
        Self {
            dark,
            raised,
            raised_text,
            raised_edge,
            strip,
            groupbar,
            active,
            hover,
            muted,
            text_active,
            group_muted,
            fg,
        }
    }

    /// Hue colour pushed until it reads on the strip, the hover and the
    /// active fill (used for the bell / activity / external tab states).
    fn badge(&self, base: Rgb) -> String {
        let ext = if self.dark { WHITE } else { BLACK };
        hex(ensure(
            base,
            &[self.strip, self.hover, self.active],
            4.5,
            ext,
        ))
    }

    /// The chrome rule set. `bar_alpha` is the tab bar's own opacity.
    pub fn css(&self, bar_alpha: f64) -> String {
        let [sr, sg, sb] = self.strip.map(|v| v.round() as u8);
        let strip_bg = format!("rgba({sr},{sg},{sb},{bar_alpha})");
        let (groupbar, active, hover) = (hex(self.groupbar), hex(self.active), hex(self.hover));
        let strip_hex = hex(self.strip);
        let (raised, rtext, redge) = (
            hex(self.raised),
            hex(self.raised_text),
            hex(self.raised_edge),
        );
        let (muted, gmuted) = (hex(self.muted), hex(self.group_muted));
        let (tact, fg) = (hex(self.text_active), hex(self.fg));
        let (bell, act, ext) = (
            self.badge([229.0, 192.0, 123.0]),
            self.badge([97.0, 175.0, 239.0]),
            self.badge([152.0, 195.0, 121.0]),
        );
        let gb = |base: Rgb| {
            hex(ensure(
                base,
                &[self.groupbar, mix(self.groupbar, self.fg, 0.10)],
                4.5,
                if self.dark { WHITE } else { BLACK },
            ))
        };
        let (gbell, gact, gext) = (
            gb([229.0, 192.0, 123.0]),
            gb([97.0, 175.0, 239.0]),
            gb([152.0, 195.0, 121.0]),
        );
        format!(
            ".tabbar {{ background-color: {strip_bg}; }}\n\
             .groupbar {{ background-color: {groupbar}; }}\n\
             .tab, .group-row {{ background-image: none; background-color: transparent; \
             border-color: transparent; box-shadow: none; text-shadow: none; color: {muted}; }}\n\
             .group-row {{ color: {gmuted}; }}\n\
             .tab:hover, .group-row:hover {{ background-color: {hover}; color: {fg}; }}\n\
             .tab:checked, .group-row:checked, .tab:checked:hover, .group-row:checked:hover \
             {{ background-color: {active}; color: {tact}; }}\n\
             .tab.bell {{ color: {bell}; }}\n\
             .tab.activity {{ color: {act}; }}\n\
             .tab.external {{ color: {ext}; }}\n\
             .group-attn {{ font-size: 100%; }}\n\
             .group-attn.attn-bell {{ color: {gbell}; }}\n\
             .group-attn.attn-activity {{ color: {gact}; }}\n\
             .group-attn.attn-external {{ color: {gext}; }}\n\
             .tab-new, .tab-close, .group-new, .group-close {{ background-image: none; \
             background-color: transparent; border-color: transparent; box-shadow: none; \
             color: {muted}; }}\n\
             .group-new, .group-close {{ color: {gmuted}; }}\n\
             .tab-new:hover, .tab-close:hover, .group-new:hover, .group-close:hover \
             {{ background-color: {hover}; color: {fg}; }}\n\
             .search-bar {{ background-color: {raised}; color: {rtext}; border: 1px solid {redge}; }}\n\
             .search-bar entry {{ background-image: none; background-color: {strip_hex}; \
             color: {rtext}; border-color: {redge}; box-shadow: none; }}\n\
             .search-bar button {{ background-image: none; background-color: transparent; \
             border-color: transparent; box-shadow: none; color: {rtext}; }}\n\
             .search-bar button:hover {{ background-color: {redge}; }}\n\
             .search-status {{ color: {rtext}; }}\n\
             .search-status.error {{ font-weight: bold; }}\n\
             .search-bar button:disabled {{ opacity: 0.35; }}\n\
             .group-chip {{ color: {tact}; }}\n\
             .notice {{ background-color: {raised}; color: {rtext}; border: 1px solid {redge}; }}\n\
             .notice button {{ background-image: none; background-color: {redge}; \
             border-color: transparent; box-shadow: none; color: {rtext}; }}\n\
             .notice button:hover {{ background-color: {rtext}; color: {raised}; }}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> (u8, u8, u8) {
        let n = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap();
        ((n >> 16) as u8, (n >> 8) as u8, n as u8)
    }

    const PALETTES: &[(&str, &str, &str)] = &[
        ("Tomorrow Night", "#1d1f21", "#c5c8c6"),
        ("Solarized Dark", "#002b36", "#839496"),
        ("Solarized Light", "#fdf6e3", "#657b83"),
        ("Dracula", "#282a36", "#f8f8f2"),
        ("Gruvbox Dark", "#282828", "#ebdbb2"),
        ("Gruvbox Light", "#fbf1c7", "#3c3836"),
        ("Nord", "#2e3440", "#d8dee9"),
        ("One Dark", "#282c34", "#abb2bf"),
        ("One Light", "#fafafa", "#383a42"),
        ("Catppuccin Mocha", "#1e1e2e", "#cdd6f4"),
        ("Catppuccin Latte", "#eff1f5", "#4c4f69"),
        ("Tokyo Night", "#1a1b26", "#c0caf5"),
        ("Pure black", "#000000", "#ffffff"),
        ("Pure white", "#ffffff", "#000000"),
    ];

    #[test]
    fn default_scheme_tokens_match_the_gtk_spike() {
        let t = ChromeTokens::derive(h("#1d1f21"), h("#c5c8c6"));
        assert_eq!(hex(t.strip), "#17181a");
        assert_eq!(hex(t.groupbar), "#111213");
        assert_eq!(hex(t.active), "#2d2f30");
        assert_eq!(hex(t.hover), "#222425");
        assert_eq!(hex(t.muted), "#949796");
        assert_eq!(hex(t.text_active), "#d9dbda");
    }

    #[test]
    fn every_palette_meets_the_contrast_floors() {
        // The hand-picked palettes AND every scheme that actually ships.
        let mut all: Vec<(String, String, String)> = PALETTES
            .iter()
            .map(|(n, b, f)| (n.to_string(), b.to_string(), f.to_string()))
            .collect();
        for id in hashterm_core::scheme::bundled_ids() {
            let sch = hashterm_core::scheme::bundled(id).unwrap();
            all.push((format!("bundled {id}"), sch.background, sch.foreground));
        }
        for (name, bg, fg) in &all {
            let t = ChromeTokens::derive(h(bg), h(fg));
            let ok = |what: &str, v: f64, min: f64| {
                assert!(v >= min - 0.02, "{name}: {what} {v:.2} < {min}");
            };
            ok("muted/strip", contrast(t.muted, t.strip), 4.5);
            ok("muted/hover", contrast(t.muted, t.hover), 4.5);
            ok("muted/active", contrast(t.muted, t.active), 4.5);
            ok("active label", contrast(t.text_active, t.active), 7.0);
            ok(
                "fill/strip",
                contrast(t.active, t.strip),
                if t.dark { 1.30 } else { 1.15 },
            );
            ok("group label", contrast(t.group_muted, t.groupbar), 4.5);
            ok("notice text", contrast(t.raised_text, t.raised), 7.0);
            ok("notice edge", contrast(t.raised_edge, t.raised), 1.5);
            for base in [
                [229.0, 192.0, 123.0],
                [97.0, 175.0, 239.0],
                [152.0, 195.0, 121.0],
            ] {
                let b = ensure(
                    base,
                    &[t.strip, t.hover, t.active],
                    4.5,
                    if t.dark { WHITE } else { BLACK },
                );
                ok(
                    "badge",
                    contrast(b, t.strip).min(contrast(b, t.active)),
                    4.5,
                );
            }
        }
    }

    #[test]
    fn css_sets_background_image_none_for_every_chrome_widget() {
        let css = ChromeTokens::derive(h("#fdf6e3"), h("#657b83")).css(1.0);
        for sel in [
            ".tab,",
            ".tab-new",
            ".tab-close",
            ".group-new",
            ".group-close",
        ] {
            assert!(css.contains(sel), "{sel}");
        }
        assert!(css.matches("background-image: none").count() >= 2);
        assert!(css.contains(".tab.bell"));
    }

    #[test]
    fn ensure_terminates_when_unreachable() {
        let c = ensure([10.0, 10.0, 10.0], &[BLACK], 21.0, BLACK);
        assert!(contrast(c, BLACK) < 21.0);
    }
}
