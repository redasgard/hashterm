//! Clickable links: what counts as a URL, what may be opened, and the
//! Ctrl+click wiring for one terminal. Pure helpers first (tested), GTK after.

/// VTE (PCRE2) pattern for candidate URLs. Trailing punctuation is trimmed by
/// `clean_url`, because the regex alone cannot tell "see https://x.org." from
/// a URL ending in a dot.
pub const URL_PATTERN: &str = r#"(?:https?://|mailto:)[^\s<>"'`]+"#;

/// Schemes hashterm will hand to the desktop. Everything else (file:, ssh:,
/// javascript:, custom handlers…) is refused: terminal output is untrusted.
const ALLOWED: &[&str] = &["http", "https", "mailto"];

/// Lower-cased scheme of `uri` (letters, digits, `+-.` before the first `:`).
pub fn scheme_of(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    let valid = !scheme.is_empty()
        && scheme.chars().next()?.is_ascii_alphabetic()
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    valid.then(|| scheme.to_ascii_lowercase())
}

pub fn is_allowed(uri: &str) -> bool {
    scheme_of(uri).is_some_and(|s| ALLOWED.contains(&s.as_str()))
}

/// Strip what a sentence or markup adds after a URL: trailing `.,;:!?` and
/// quotes, and closing brackets that have no opener inside the URL.
pub fn clean_url(raw: &str) -> String {
    let mut s = raw;
    while let Some(last) = s.chars().last() {
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => s.matches(')').count() > s.matches('(').count(),
            ']' => s.matches(']').count() > s.matches('[').count(),
            '}' => s.matches('}').count() > s.matches('{').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
    s.to_string()
}

/// Zero-width and bidirectional-override characters: they let a link read as
/// one thing and point at another.
fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Text from program output made safe to show in a notification or OSD:
/// control and invisible characters dropped, markup escaped, length capped.
pub fn plain_text(s: &str, max_chars: usize) -> String {
    let clean: String = s
        .chars()
        .filter(|c| !c.is_control() && !is_invisible(*c))
        .take(max_chars)
        .collect();
    clean
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A control character in a link would let output smuggle a second
/// command-ish string into logs or the OSD; refuse such targets outright.
pub fn is_sane(uri: &str) -> bool {
    !uri.is_empty()
        && uri.len() <= 4096
        && !uri
            .chars()
            .any(|c| c.is_control() || c == ' ' || is_invisible(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_punctuation_and_brackets_are_trimmed() {
        let cases = [
            ("https://example.org.", "https://example.org"),
            ("https://example.org/a,", "https://example.org/a"),
            (
                "https://example.org/a?b=1&c=2!",
                "https://example.org/a?b=1&c=2",
            ),
            (
                "https://en.wikipedia.org/wiki/Rust_(language)",
                "https://en.wikipedia.org/wiki/Rust_(language)",
            ),
            ("https://example.org/x)", "https://example.org/x"),
            ("https://example.org/x).", "https://example.org/x"),
            ("https://example.org/a]", "https://example.org/a"),
            ("https://example.org/path/", "https://example.org/path/"),
            ("https://example.org/#frag", "https://example.org/#frag"),
            ("mailto:a@b.example.", "mailto:a@b.example"),
        ];
        for (raw, want) in cases {
            assert_eq!(clean_url(raw), want, "{raw}");
        }
    }

    #[test]
    fn only_web_and_mail_schemes_open() {
        for ok in ["http://a.b", "HTTPS://a.b/x", "mailto:me@a.b"] {
            assert!(is_allowed(ok), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ssh://host",
            "vnc://h",
            "data:text/html,x",
            "//host/x",
            "https//no-colon",
            "",
            "x-evil+scheme://a",
        ] {
            assert!(!is_allowed(bad), "{bad}");
        }
        assert_eq!(scheme_of("File:///x").as_deref(), Some("file"));
        assert_eq!(scheme_of("1http://x"), None);
    }

    #[test]
    fn program_text_is_made_inert() {
        assert_eq!(
            plain_text("a\x1b[31m<b>x</b> & y", 80),
            "a[31m&lt;b&gt;x&lt;/b&gt; &amp; y"
        );
        assert_eq!(plain_text(&"x".repeat(200), 80).len(), 80);
        assert_eq!(plain_text("ab\u{202E}cd", 80), "abcd");
    }

    #[test]
    fn lookalike_tricks_are_refused() {
        assert!(!is_sane("https://example.org/\u{202E}gpj.exe"));
        assert!(!is_sane("https://exa\u{200B}mple.org"));
    }

    #[test]
    fn insane_targets_are_refused() {
        assert!(is_sane("https://a.b/x?y=1"));
        assert!(!is_sane("https://a.b/\x1b[31m"));
        assert!(!is_sane("https://a.b/x y"));
        assert!(!is_sane(""));
        assert!(!is_sane(&format!("https://a.b/{}", "x".repeat(5000))));
    }

    #[test]
    fn url_pattern_compiles_as_a_pcre2_pattern_shape() {
        // Cheap structural check; the real compile happens in VTE at runtime.
        assert!(URL_PATTERN.starts_with("(?:https?://"));
    }
}
