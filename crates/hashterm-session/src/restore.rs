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
                    && !DENYLIST.contains(&fg.name.as_str())
                {
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
        argv.push("--scrollback".into());
        argv.push(save_dir.join(&sb.file).to_string_lossy().into_owned());
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
            if matches!(fg.name.as_str(), "vim" | "nvim" | "vi")
                && !exec_argv.iter().any(|a| a == "-S")
            {
                let session_file = fg.cwd.join("Session.vim");
                if session_file.is_file() {
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

fn restartable(fg: &FgProc, opts: &RestoreOptions) -> bool {
    if fg.argv.is_empty() || DENYLIST.contains(&fg.name.as_str()) {
        return false;
    }
    opts.restart_arbitrary || opts.programs.iter().any(|p| p == &fg.name)
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
}
