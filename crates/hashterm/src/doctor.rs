//! `hashterm --doctor`: one screen that answers "why doesn't it work?".
//! Runs before any GTK/GApplication setup, so it works when the GUI will not
//! start. `$HOME` is redacted so the output can be pasted into a bug report.

use hashterm_core::config::Config;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

pub struct Check {
    pub level: Level,
    pub label: &'static str,
    pub detail: String,
}

fn check(level: Level, label: &'static str, detail: impl Into<String>) -> Check {
    Check {
        level,
        label,
        detail: detail.into(),
    }
}

/// Replace the home directory prefix with `~`.
pub fn redact_home(s: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.len() < 2 {
        return s.to_string();
    }
    // Only at a path boundary: "/home/dev" must not eat "/home/devops/x".
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(home) {
        let after = &rest[i + home.len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || "-_.".contains(c)));
        out.push_str(&rest[..i]);
        if boundary {
            out.push('~');
        } else {
            out.push_str(home);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// "tmux 3.5a" / "tmux next-3.6" -> (3, 5). Anything unparseable -> None.
pub fn tmux_version(s: &str) -> Option<(u32, u32)> {
    let v = s.trim().strip_prefix("tmux ")?;
    let v = v.trim_start_matches("next-");
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor: String = it
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor.parse().ok()?))
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn size_of(p: &Path) -> String {
    match std::fs::metadata(p) {
        Ok(m) => format!("{} KiB", m.len().div_ceil(1024)),
        Err(_) => "absent".into(),
    }
}

pub fn run_checks(config_path: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(check(
        Level::Ok,
        "hashterm",
        format!("version {}", env!("CARGO_PKG_VERSION")),
    ));
    out.push(check(Level::Ok, "toolkit", hashterm_ui::toolkit_versions()));

    match std::process::Command::new("tmux").arg("-V").output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            match tmux_version(&v) {
                Some(ver) if ver >= (3, 5) => out.push(check(Level::Ok, "tmux", v)),
                Some(_) => out.push(check(
                    Level::Warn,
                    "tmux",
                    format!("{v}: tab search and links need tmux 3.5 or newer"),
                )),
                None => out.push(check(
                    Level::Warn,
                    "tmux",
                    format!("{v} (version not recognised)"),
                )),
            }
        }
        _ => out.push(check(
            Level::Fail,
            "tmux",
            "not found on PATH: install tmux",
        )),
    }

    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| "-".into());
    out.push(check(
        Level::Ok,
        "session",
        format!(
            "type={} desktop={} GDK_BACKEND={} display={} wayland={}",
            var("XDG_SESSION_TYPE"),
            var("XDG_CURRENT_DESKTOP"),
            var("GDK_BACKEND"),
            var("DISPLAY"),
            var("WAYLAND_DISPLAY")
        ),
    ));

    match hashterm_platform::probe_global_shortcuts_portal() {
        Ok(v) => out.push(check(
            Level::Ok,
            "hotkey portal",
            format!("GlobalShortcuts v{v} answers on the session bus"),
        )),
        Err(e) if hashterm_platform::external_toggle_binding().is_some() => out.push(check(
            Level::Ok,
            "hotkey",
            format!(
                "portal unavailable ({e}), but your desktop runs `hashterm --toggle` on {}",
                hashterm_platform::external_toggle_binding().unwrap_or_default()
            ),
        )),
        Err(e) => out.push(check(
            Level::Warn,
            "hotkey portal",
            format!(
                "{e}; the global key needs an X11 grab or `hashterm --toggle` bound in your desktop"
            ),
        )),
    }

    match Config::load(config_path) {
        Ok(_) => out.push(check(
            Level::Ok,
            "config",
            format!("{} parses", config_path.display()),
        )),
        Err(e) if !config_path.exists() => out.push(check(
            Level::Ok,
            "config",
            format!(
                "{} not written yet (defaults in use): {e}",
                config_path.display()
            ),
        )),
        Err(e) => out.push(check(Level::Fail, "config", e.to_string())),
    }

    let log = hashterm_core::logfile::log_path();
    out.push(check(
        Level::Ok,
        "log file",
        format!("{} ({})", log.display(), size_of(&log)),
    ));
    out.push(check(
        Level::Ok,
        "state",
        hashterm_core::logfile::state_dir().display().to_string(),
    ));

    let socket = hashterm_core::socket_name();
    let sessions = std::process::Command::new("tmux")
        .args(["-L", &socket, "list-sessions"])
        .output();
    out.push(match sessions {
        Ok(o) if o.status.success() => check(
            Level::Ok,
            "tmux server",
            format!(
                "socket '{socket}', {} sessions",
                String::from_utf8_lossy(&o.stdout).lines().count()
            ),
        ),
        _ => check(
            Level::Ok,
            "tmux server",
            format!("socket '{socket}' not running (starts with the first tab)"),
        ),
    });

    out.push(match hashterm_tmux::conf::clipboard_tool() {
        Some(t) => check(Level::Ok, "clipboard", t),
        None => check(
            Level::Warn,
            "clipboard",
            "none of wl-copy, xclip, xsel: copy from tmux's own selection will not reach other apps",
        ),
    });
    out.push(if on_path("xdg-open") {
        check(Level::Ok, "xdg-open", "found")
    } else {
        check(Level::Warn, "xdg-open", "not found: links cannot be opened")
    });
    out
}

pub fn render(checks: &[Check], home: &str) -> String {
    let mut s = String::from("hashterm doctor\n");
    for c in checks {
        let tag = match c.level {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        s.push_str(&format!(
            "  [{tag}] {:<14} {}\n",
            c.label,
            redact_home(&c.detail, home)
        ));
    }
    s
}

pub fn run(config: Option<&Path>, with_log: bool) -> std::process::ExitCode {
    let path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(Config::default_path);
    let home = std::env::var("HOME").unwrap_or_default();
    let checks = run_checks(&path);
    print!("{}", render(&checks, &home));
    if with_log {
        println!("\nlast log lines:");
        let lines = hashterm_core::logfile::tail(40);
        if lines.is_empty() {
            println!("  (log is empty or absent)");
        }
        for l in lines {
            println!("  {}", redact_home(&l, &home));
        }
    }
    if checks.iter().any(|c| c.level == Level::Fail) {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_versions_parse() {
        assert_eq!(tmux_version("tmux 3.5a"), Some((3, 5)));
        assert_eq!(tmux_version("tmux 3.10"), Some((3, 10)));
        assert_eq!(tmux_version("tmux next-3.6"), Some((3, 6)));
        assert_eq!(tmux_version("screen"), None);
        assert!((3, 10) > (3, 5));
    }

    #[test]
    fn home_is_redacted_but_a_bare_slash_is_not() {
        assert_eq!(redact_home("/home/dev/x /home/dev", "/home/dev"), "~/x ~");
        assert_eq!(redact_home("/a/b", "/"), "/a/b");
        assert_eq!(redact_home("/a/b", ""), "/a/b");
        assert_eq!(redact_home("/home/devops/x", "/home/dev"), "/home/devops/x");
        assert_eq!(redact_home("/home/dev/x", "/home/dev/"), "~/x");
    }

    #[test]
    fn render_marks_failures() {
        let c = vec![
            check(Level::Ok, "tmux", "tmux 3.5a"),
            check(Level::Fail, "config", "bad /home/dev/c.toml"),
        ];
        let out = render(&c, "/home/dev");
        assert!(out.contains("[ok  ] tmux"));
        assert!(out.contains("[FAIL] config"));
        assert!(out.contains("bad ~/c.toml"));
    }
}
