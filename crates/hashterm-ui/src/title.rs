//! Automatic tab titles. tmux's default `pane_title` is the HOST NAME, so
//! every new tab used to say the same thing. A title a program sets itself
//! (OSC 2) wins; otherwise a tab is named after its folder, prefixed by the
//! foreground program when that is not a shell.

const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "csh", "tcsh", "ash",
];
const MAX_CHARS: usize = 64;

/// Inputs that do not change while the app runs.
pub struct Env {
    pub host: String,
    pub home: String,
    pub shell: String,
}

impl Env {
    pub fn current() -> &'static Env {
        static ENV: std::sync::OnceLock<Env> = std::sync::OnceLock::new();
        ENV.get_or_init(|| Env {
            host: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .unwrap_or_default()
                .trim()
                .to_string(),
            home: std::env::var("HOME").unwrap_or_default(),
            shell: std::env::var("SHELL")
                .ok()
                .and_then(|s| s.rsplit('/').next().map(str::to_string))
                .unwrap_or_default(),
        })
    }
}

fn is_host(title: &str, host: &str) -> bool {
    let short = host.split('.').next().unwrap_or(host);
    title.eq_ignore_ascii_case(host) || title.eq_ignore_ascii_case(short)
}

fn base(path: &str, home: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return "/".into();
    }
    if !home.is_empty() && path == home.trim_end_matches('/') {
        return "~".into();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// `vte_title`: what the terminal reports (empty until something is set).
pub fn auto_title(vte_title: &str, cmd: &str, path: &str, env: &Env) -> String {
    let vte_title = vte_title.trim();
    let title = if !vte_title.is_empty() && !is_host(vte_title, &env.host) {
        vte_title.to_string()
    } else if path.is_empty() {
        return if vte_title.is_empty() {
            String::new()
        } else {
            vte_title.to_string()
        };
    } else {
        let folder = base(path, &env.home);
        let is_shell = cmd.is_empty() || SHELLS.contains(&cmd) || cmd == env.shell;
        if is_shell {
            folder
        } else {
            format!("{cmd} · {folder}")
        }
    };
    title
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            host: "Atlas.example.org".into(),
            home: "/home/dev".into(),
            shell: "zsh".into(),
        }
    }

    #[test]
    fn host_name_title_is_replaced_by_the_folder() {
        let e = env();
        assert_eq!(auto_title("atlas", "zsh", "/home/dev/shop", &e), "shop");
        assert_eq!(auto_title("ATLAS.example.org", "bash", "/etc", &e), "etc");
        assert_eq!(auto_title("", "zsh", "/home/dev", &e), "~");
        assert_eq!(auto_title("atlas", "zsh", "/", &e), "/");
        assert_eq!(auto_title("atlas", "zsh", "/srv/www/", &e), "www");
    }

    #[test]
    fn program_title_wins() {
        let e = env();
        assert_eq!(auto_title("mytitle", "zsh", "/etc", &e), "mytitle");
        assert_eq!(
            auto_title("user@atlas: ~/x", "zsh", "/x", &e),
            "user@atlas: ~/x"
        );
    }

    #[test]
    fn foreground_program_is_named_unless_it_is_a_shell() {
        let e = env();
        assert_eq!(
            auto_title("atlas", "vim", "/home/dev/shop", &e),
            "vim · shop"
        );
        assert_eq!(auto_title("atlas", "ssh", "/home/dev", &e), "ssh · ~");
        assert_eq!(auto_title("atlas", "fish", "/tmp", &e), "tmp");
        assert_eq!(auto_title("atlas", "zsh", "/tmp", &e), "tmp");
    }

    #[test]
    fn no_facts_means_keep_what_the_terminal_says() {
        let e = env();
        assert_eq!(auto_title("atlas", "", "", &e), "atlas");
        assert_eq!(auto_title("", "", "", &e), "");
    }

    #[test]
    fn control_characters_are_stripped_and_length_capped() {
        let e = env();
        assert_eq!(auto_title("a\u{1b}[31mb", "zsh", "/x", &e), "a[31mb");
        let long = "x".repeat(200);
        assert_eq!(
            auto_title(&long, "zsh", "/x", &e).chars().count(),
            MAX_CHARS
        );
        assert_eq!(auto_title("atlas", "zsh", "/a\u{7}b", &e), "ab");
    }

    #[test]
    fn non_utf8_looking_paths_do_not_panic() {
        let e = env();
        assert_eq!(auto_title("atlas", "zsh", "/tmp/日本語", &e), "日本語");
    }
}
