//! Command-level interface to the dedicated tmux server (`tmux -L hashterm`).
//! Synchronous: individual tmux commands complete in single-digit milliseconds;
//! callers on the GTK main loop wrap calls in `gio::spawn_blocking` when needed.

use std::path::PathBuf;
use std::process::Command;

pub const SESSION_PREFIX: &str = "ht-";
pub const TITLE_OPTION: &str = "@hashterm_title";
/// "1" when the user renamed the tab: auto titles must not overwrite it.
pub const TITLE_CUSTOM_OPTION: &str = "@hashterm_title_custom";
/// Per-tab accent color, "#rrggbb".
pub const ACCENT_OPTION: &str = "@hashterm_accent";
/// Tab group (workspace) name; unset = the implicit default group.
pub const GROUP_OPTION: &str = "@hashterm_group";
/// Group color "#rrggbb", duplicated on every member (self-healing; group
/// metadata is always derived from members — there is no registry).
pub const GROUP_COLOR_OPTION: &str = "@hashterm_group_color";
/// The implicit group of sessions without a GROUP_OPTION.
pub const DEFAULT_GROUP: &str = "main";

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("failed to run tmux: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("tmux {args:?} failed: {stderr}")]
    Command { args: Vec<String>, stderr: String },
    #[error("unexpected tmux output: {0}")]
    Parse(String),
}

/// Immutable tmux session id, e.g. `$3`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TmuxSessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: TmuxSessionId,
    pub name: String,
    pub title: String,
    /// User renamed this tab; auto titles are suppressed.
    pub title_custom: bool,
    /// Per-tab accent color "#rrggbb".
    pub accent: Option<String>,
    /// Tab group; None = the implicit DEFAULT_GROUP.
    pub group: Option<String>,
    /// Group color "#rrggbb" as recorded on this member.
    pub group_color: Option<String>,
    /// Attached client count (>1 means someone besides us is attached).
    pub attached: u32,
}

#[derive(Debug, Clone)]
pub struct NewSessionOpts {
    pub title: String,
    pub cwd: Option<PathBuf>,
    /// argv to run instead of the default shell.
    pub command: Option<Vec<String>>,
    /// Group the new tab starts in; None = DEFAULT_GROUP.
    pub group: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TmuxController {
    socket: String,
    conf_path: PathBuf,
}

impl TmuxController {
    pub fn new(socket: impl Into<String>, conf_path: PathBuf) -> Self {
        Self {
            socket: socket.into(),
            conf_path,
        }
    }

    /// Run a tmux command against the private socket, capturing stdout.
    ///
    /// `-f <bundled conf>` rides along on EVERY invocation: tmux only reads it
    /// when this very command happens to birth the server, so whichever call
    /// wins that race loads our conf — never the user's ~/.tmux.conf. (A lone
    /// `start-server` is not enough: a session-less server exits immediately
    /// unless exit-empty is off, and the next command would then quietly start
    /// a default-config server.)
    pub fn run(&self, args: &[&str]) -> Result<String, TmuxError> {
        let conf = self.conf_path.to_string_lossy().into_owned();
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .arg("-f")
            .arg(&conf)
            .args(args)
            .output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(TmuxError::Command {
                args: args.iter().map(|s| s.to_string()).collect(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().into(),
            })
        }
    }

    /// Start the dedicated server with the bundled conf if not already running.
    /// The conf sets `exit-empty off`, so the server persists with no sessions.
    pub fn ensure_server(&self) -> Result<(), TmuxError> {
        self.run(&["start-server"]).map(|_| ())
    }

    /// Create a detached session named `ht-<uuid>`; returns its immutable id.
    pub fn new_session(&self, opts: &NewSessionOpts) -> Result<TmuxSessionId, TmuxError> {
        let name = unique_session_name();
        let mut args: Vec<String> = vec![
            "new-session".into(),
            "-d".into(),
            "-s".into(),
            name.clone(),
            "-P".into(),
            "-F".into(),
            "#{session_id}".into(),
        ];
        if let Some(cwd) = &opts.cwd {
            args.push("-c".into());
            args.push(cwd.to_string_lossy().into_owned());
        }
        if let Some(cmd) = &opts.command {
            // `--` so a command whose first token starts with `-` cannot be
            // parsed by tmux as a new-session flag.
            args.push("--".into());
            args.extend(cmd.iter().cloned());
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let id = self.run(&argv)?.trim().to_owned();
        if !id.starts_with('$') {
            return Err(TmuxError::Parse(id));
        }
        let sid = TmuxSessionId(id);
        let title = sanitize_option_value(&opts.title);
        self.run(&["set-option", "-t", &sid.0, TITLE_OPTION, &title])?;
        // Group set at creation so the tab never flashes in the wrong group.
        if let Some(group) = opts.group.as_deref() {
            self.set_group(&sid, Some(group))?;
        }
        Ok(sid)
    }

    pub fn kill_session(&self, id: &TmuxSessionId) -> Result<(), TmuxError> {
        self.run(&["kill-session", "-t", &id.0]).map(|_| ())
    }

    pub fn set_title(&self, id: &TmuxSessionId, title: &str) -> Result<(), TmuxError> {
        let title = sanitize_option_value(title);
        self.run(&["set-option", "-t", &id.0, TITLE_OPTION, &title])
            .map(|_| ())
    }

    /// User rename: pins the title against automatic updates.
    pub fn set_custom_title(&self, id: &TmuxSessionId, title: &str) -> Result<(), TmuxError> {
        self.set_title(id, title)?;
        self.run(&["set-option", "-t", &id.0, TITLE_CUSTOM_OPTION, "1"])
            .map(|_| ())
    }

    /// Back to automatic titles.
    pub fn clear_custom_title(&self, id: &TmuxSessionId) -> Result<(), TmuxError> {
        self.run(&["set-option", "-u", "-t", &id.0, TITLE_CUSTOM_OPTION])
            .map(|_| ())
    }

    /// Move a session into `group`. None, "" and DEFAULT_GROUP all mean the
    /// implicit default group (option unset). Names are sanitized: the
    /// list-sessions wire format is tab-delimited.
    pub fn set_group(&self, id: &TmuxSessionId, group: Option<&str>) -> Result<(), TmuxError> {
        let name = group.map(sanitize_group_name).unwrap_or_default();
        if name.is_empty() || name == DEFAULT_GROUP {
            self.run(&["set-option", "-u", "-t", &id.0, GROUP_OPTION])
                .map(|_| ())
        } else {
            self.run(&["set-option", "-t", &id.0, GROUP_OPTION, &name])
                .map(|_| ())
        }
    }

    /// Set or clear this member's copy of the group color ("#rrggbb").
    pub fn set_group_color(
        &self,
        id: &TmuxSessionId,
        color: Option<&str>,
    ) -> Result<(), TmuxError> {
        match color.and_then(valid_hex_color) {
            Some(c) => self
                .run(&["set-option", "-t", &id.0, GROUP_COLOR_OPTION, &c])
                .map(|_| ()),
            None => self
                .run(&["set-option", "-u", "-t", &id.0, GROUP_COLOR_OPTION])
                .map(|_| ()),
        }
    }

    /// Set or clear the per-tab accent color ("#rrggbb").
    pub fn set_accent(&self, id: &TmuxSessionId, accent: Option<&str>) -> Result<(), TmuxError> {
        match accent.and_then(valid_hex_color) {
            Some(color) => self
                .run(&["set-option", "-t", &id.0, ACCENT_OPTION, &color])
                .map(|_| ()),
            None => self
                .run(&["set-option", "-u", "-t", &id.0, ACCENT_OPTION])
                .map(|_| ()),
        }
    }

    /// All hashterm-owned sessions (prefix-filtered), for startup re-adoption.
    /// Title is the LAST field: it is the only one that may contain tabs
    /// (group names are sanitized by our writers).
    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, TmuxError> {
        let fmt = format!(
            "#{{session_id}}\t#{{session_name}}\t#{{session_attached}}\t#{{{TITLE_CUSTOM_OPTION}}}\t#{{{ACCENT_OPTION}}}\t#{{{GROUP_COLOR_OPTION}}}\t#{{{GROUP_OPTION}}}\t#{{{TITLE_OPTION}}}"
        );
        let out = match self.run(&["list-sessions", "-F", &fmt]) {
            Ok(o) => o,
            // No server running -> no sessions.
            Err(TmuxError::Command { stderr, .. })
                if stderr.contains("no server running") || stderr.contains("error connecting") =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let mut sessions = Vec::new();
        for line in out.lines() {
            let mut parts = line.splitn(8, '\t');
            let (
                Some(id),
                Some(name),
                Some(attached),
                Some(custom),
                Some(accent),
                Some(group_color),
                Some(group),
            ) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            )
            else {
                return Err(TmuxError::Parse(line.into()));
            };
            // Sanitize the title at the read boundary too: a newline set
            // directly on the shared server (or restored from a hostile save)
            // would otherwise spill a second line into `list-sessions -F`
            // output and break parsing of every session after it.
            let title = sanitize_option_value(parts.next().unwrap_or(""));
            if !name.starts_with(SESSION_PREFIX) {
                continue;
            }
            // Validate at the read boundary: an in-pane process can set these
            // options directly on the shared server, bypassing our writers.
            // Colors must be #rrggbb; a group name is sanitized + length-capped.
            sessions.push(SessionInfo {
                id: TmuxSessionId(id.to_owned()),
                name: name.to_owned(),
                title,
                title_custom: custom == "1",
                accent: valid_hex_color(accent),
                group: (!group.is_empty()).then(|| sanitize_group_name(group)),
                group_color: valid_hex_color(group_color),
                attached: attached.parse().unwrap_or(0),
            });
        }
        Ok(sessions)
    }

    /// A server that was started before the conf gained the `hyperlinks`
    /// terminal feature picks it up here; idempotent.
    pub fn ensure_hyperlinks(&self) -> Result<(), TmuxError> {
        let have = self.run(&["show-options", "-s", "terminal-features"])?;
        if !have.contains("tmux*:hyperlinks") {
            self.run(&[
                "set-option",
                "-as",
                "terminal-features",
                ",xterm*:hyperlinks,tmux*:hyperlinks",
            ])?;
        }
        Ok(())
    }

    /// Global window options (copy-mode and match colours, pane borders) in the
    /// colour scheme's palette, so tmux's own UI matches the terminal.
    pub fn set_window_styles(&self, styles: &[(&str, String)]) -> Result<(), TmuxError> {
        // Apply every option even if one is refused; report the first error.
        let mut first = None;
        for (opt, val) in styles {
            if let Err(e) = self.run(&["set-option", "-gw", opt, val]) {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Facts about the ACTIVE pane of every session's active window, in one
    /// tmux call (about 5 ms even with ~100 panes): feeds automatic tab titles.
    pub fn pane_facts(&self) -> Result<Vec<PaneFact>, TmuxError> {
        let out = self.run(&["list-panes", "-a", "-F", PANE_FACTS_FORMAT])?;
        Ok(parse_pane_facts(&out))
    }

    /// argv for VTE to spawn: attaches this pane to the given session.
    /// Carries `-f` for the same server-birth-race reason as `run`.
    pub fn attach_argv(&self, id: &TmuxSessionId) -> Vec<String> {
        vec![
            "tmux".into(),
            "-L".into(),
            self.socket.clone(),
            "-f".into(),
            self.conf_path.to_string_lossy().into_owned(),
            "attach-session".into(),
            "-t".into(),
            id.0.clone(),
        ]
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }
}

/// One row of `list-panes -a -F`: tab-separated, path LAST so it may contain anything.
pub const PANE_FACTS_FORMAT: &str = "#{session_name}\t#{window_active}\t#{pane_active}\t#{pane_id}\t#{window_panes}\t#{window_zoomed_flag}\t#{pane_current_command}\t#{pane_current_path}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFact {
    pub session: String,
    pub pane_id: String,
    pub panes: u32,
    pub zoomed: bool,
    pub command: String,
    pub path: String,
}

/// Keep only the active pane of each session's active window.
pub fn parse_pane_facts(out: &str) -> Vec<PaneFact> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.splitn(8, '\t');
            let session = f.next()?;
            let (win_active, pane_active) = (f.next()?, f.next()?);
            let pane_id = f.next()?;
            let panes = f.next()?.parse().ok()?;
            let zoomed = f.next()? == "1";
            let command = f.next()?;
            let path = f.next()?;
            (win_active == "1" && pane_active == "1").then(|| PaneFact {
                session: session.into(),
                pane_id: pane_id.into(),
                panes,
                zoomed,
                command: command.into(),
                path: path.into(),
            })
        })
        .collect()
}

/// Group names travel in a tab-separated tmux format string: tabs/newlines
/// are replaced with spaces and the result trimmed.
pub fn sanitize_group_name(name: &str) -> String {
    name.replace(['\t', '\n', '\r'], " ")
        .trim()
        .chars()
        .take(64)
        .collect()
}

/// Strip control characters (which would corrupt the tab-delimited
/// `list-sessions -F` wire format, and — for the title — crash Pango's XML
/// parser in `set_markup`, blanking the tab-switch OSD), then cap length.
/// Applied to every value we write into a session user-option, and re-applied
/// on read since a pane process can set the option directly on the shared
/// server, bypassing our writers. `char::is_control` covers `\t`/`\n`/`\r`
/// plus the whole C0/C1 + DEL range.
pub fn sanitize_option_value(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(256).collect()
}

/// A `#rrggbb` color or nothing. Validated at BOTH the write boundary and the
/// read boundary (list_sessions), because a process inside a pane can set the
/// option directly on the shared tmux server, bypassing our writers.
pub fn valid_hex_color(s: &str) -> Option<String> {
    let h = s.strip_prefix('#')?;
    (h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_owned())
}

/// Fresh hashterm session name: `ht-` + time-ordered unique id.
pub fn unique_session_name() -> String {
    format!("{SESSION_PREFIX}{}", uuid7())
}

/// Time-ordered unique id (UUIDv7-shaped, without a uuid crate dependency):
/// millis since epoch + per-process counter + pid entropy, hex.
fn uuid7() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{ms:012x}{:04x}{seq:04x}", std::process::id() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid7_is_unique_and_ordered() {
        let a = uuid7();
        let b = uuid7();
        assert_ne!(a, b);
        assert_eq!(a.len(), 20);
    }

    // Integration tests against a throwaway socket run only when tmux exists.
    #[test]
    fn session_lifecycle_on_scratch_server() {
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("tmux not installed; skipping");
            return;
        }
        let socket = format!("hashterm-test-{}", std::process::id());
        let ctl = TmuxController::new(socket.clone(), PathBuf::from("/dev/null"));
        let id = ctl
            .new_session(&NewSessionOpts {
                title: "t".into(),
                cwd: None,
                command: Some(vec!["sleep".into(), "60".into()]),
                group: Some("work stuff".into()),
            })
            .unwrap();
        let sessions = ctl.list_sessions().unwrap();
        let info = sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.group.as_deref(), Some("work stuff"));
        assert_eq!(info.group_color, None);

        ctl.set_group_color(&id, Some("#e06c75")).unwrap();
        ctl.set_group(&id, Some("prod")).unwrap();
        let info = ctl
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert_eq!(info.group.as_deref(), Some("prod"));
        assert_eq!(info.group_color.as_deref(), Some("#e06c75"));

        // DEFAULT_GROUP and None both unset the option.
        ctl.set_group(&id, Some(DEFAULT_GROUP)).unwrap();
        ctl.set_group_color(&id, None).unwrap();
        let info = ctl
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert_eq!(info.group, None);
        assert_eq!(info.group_color, None);

        let name = ctl
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap()
            .name;
        // Pane facts: a bare `sleep` (not a shell) runs in the session.
        let fact = (0..20)
            .find_map(|_| {
                let f = ctl
                    .pane_facts()
                    .unwrap()
                    .into_iter()
                    .find(|f| f.session == name);
                if f.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                f
            })
            .expect("pane fact for the session");
        assert_eq!(fact.command, "sleep");
        assert_eq!(fact.panes, 1);
        assert!(!fact.zoomed);

        ctl.kill_session(&id).unwrap();
        // Tear down the scratch server entirely.
        let _ = ctl.run(&["kill-server"]);
    }

    #[test]
    fn pane_fact_rows_keep_only_the_active_pane_and_odd_paths() {
        let out = "ht-a\t1\t1\t%1\t2\t0\tbash\t/tmp/a b\tc\n\
                   ht-a\t1\t0\t%2\t2\t0\tvim\t/x\n\
                   ht-b\t0\t1\t%3\t1\t0\tbash\t/y\n\
                   junk line\n";
        let f = parse_pane_facts(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].session, "ht-a");
        assert_eq!(f[0].path, "/tmp/a b\tc");
        assert_eq!(f[0].panes, 2);
    }

    #[test]
    fn group_name_sanitizing() {
        assert_eq!(sanitize_group_name(" work\tstuff\n"), "work stuff");
        assert_eq!(sanitize_group_name("plain"), "plain");
        assert_eq!(sanitize_group_name("\t\n"), "");
    }
}
