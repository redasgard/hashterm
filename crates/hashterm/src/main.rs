use clap::{Parser, Subcommand};
use hashterm_core::config::Config;
use hashterm_core::socket_name;
use hashterm_session::{DumpOptions, OnConflict, RestoreOptions, SessionStore};
use hashterm_tmux::TmuxController;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "hashterm", version, about = "tmux-native GUI terminal")]
struct Cli {
    /// Alternative config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Toggle the drop-down terminal of the running instance (or start one).
    #[arg(long)]
    toggle: bool,

    /// Replace the running GUI with a fresh instance (e.g. after an upgrade).
    /// Your terminals live on the tmux server, so they survive and are
    /// re-adopted; nothing is lost.
    #[arg(long)]
    restart: bool,

    /// Open a new tab in the running instance.
    #[arg(long)]
    new_tab: bool,

    /// Working directory for --new-tab.
    #[arg(long, requires = "new_tab")]
    cwd: Option<PathBuf>,

    /// Restore a named saved session in the GUI.
    #[arg(long, value_name = "NAME")]
    restore: Option<String>,

    /// Command to run in the new tab; consumes all following arguments.
    #[arg(short = 'e', long = "exec", num_args = 1.., allow_hyphen_values = true)]
    exec: Vec<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Headless full dump of the dedicated tmux server (also used by tmux hooks).
    Dump {
        /// Save name; defaults to a timestamped autosave.
        #[arg(long)]
        name: Option<String>,
        /// Write into the rotated autosave area instead of named saves.
        #[arg(long)]
        auto: bool,
        /// Suppress non-error output (for run-shell hooks).
        #[arg(long)]
        quiet: bool,
    },
    /// Headless restore of a named save onto the dedicated tmux server.
    Restore {
        /// Save name, or "latest" for the newest autosave.
        name: String,
        #[arg(long, value_parser = ["skip", "rename", "replace"], default_value = "rename")]
        on_conflict: String,
        /// The save lives in the autosave area.
        #[arg(long)]
        auto: bool,
    },
    /// List saved sessions.
    Saves,
    /// Internal: runs inside a restored pane; replays scrollback then execs.
    #[command(hide = true)]
    RestorePane {
        #[arg(long)]
        scrollback: Option<PathBuf>,
        #[arg(long)]
        shell: Option<String>,
        #[arg(long, num_args = 1.., allow_hyphen_values = true)]
        exec: Vec<String>,
        /// Like --exec but trusted (from user config, not a save file): runs
        /// without the exec-dir allow-list. Mutually exclusive with --exec.
        #[arg(long, num_args = 1.., allow_hyphen_values = true, conflicts_with = "exec")]
        exec_trusted: Vec<String>,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();

    // restore-pane runs inside a pane pty: no logging setup, no config, exec ASAP.
    if let Some(Cmd::RestorePane {
        scrollback,
        shell,
        exec,
        exec_trusted,
    }) = &cli.cmd
    {
        let (exec_argv, trusted) = if !exec_trusted.is_empty() {
            (exec_trusted.as_slice(), true)
        } else {
            (exec.as_slice(), false)
        };
        let err = hashterm_session::replay::run_restore_pane(
            scrollback.as_deref(),
            shell.as_deref(),
            Some(exec_argv),
            trusted,
        );
        eprintln!("hashterm restore-pane: {err}");
        return std::process::ExitCode::FAILURE;
    }

    // Restart is handled before any GTK/GApplication setup: terminate the
    // running instance and re-launch a detached fresh one.
    if cli.restart {
        return run_restart(cli.config.as_deref());
    }

    hashterm_core::init_logging();
    // First run: materialize the commented reference config so users can
    // discover every knob by opening the file.
    if cli.config.is_none()
        && let Some(p) = Config::install_default_file()
    {
        tracing::info!("wrote default config to {}", p.display());
    }
    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    tracing::info!("config: {}", config_path.display());
    let mut startup_warning = None;
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // Startup must not die on a broken config: log loudly, use
            // defaults, and let the GUI show the problem as an OSD.
            tracing::error!("{e}; starting with default configuration");
            startup_warning = Some(format!("⚠ config ignored — using defaults\n{e}"));
            Config::default()
        }
    };

    match cli.cmd {
        Some(Cmd::Dump { name, auto, quiet }) => run_dump(&config, name, auto, quiet),
        Some(Cmd::Restore {
            name,
            on_conflict,
            auto,
        }) => run_restore(&config, &name, &on_conflict, auto),
        Some(Cmd::Saves) => run_saves(),
        Some(Cmd::RestorePane { .. }) => unreachable!("handled above"),
        None => {
            apply_backend_preference(&config);
            // Full argv goes to GApplication: with HANDLES_COMMAND_LINE it is
            // forwarded verbatim to the primary instance, whose command-line
            // handler dispatches --toggle/--new-tab/--restore (clap validated
            // them here first).
            let gtk_args: Vec<String> = std::env::args().collect();
            let code = hashterm_ui::run(config, gtk_args, startup_warning);
            std::process::ExitCode::from(code as u8)
        }
    }
}

/// GNOME's mutter refuses always-on-top for native Wayland clients but honors
/// EWMH keep-above for XWayland windows (which is how tilda/guake stay on
/// top). When the dropdown wants that and we're on GNOME Wayland, steer GTK
/// onto the X11 backend BEFORE it initializes. Explicit GDK_BACKEND wins.
fn apply_backend_preference(config: &Config) {
    if !config.dropdown.enabled
        || !config.dropdown.prefer_xwayland
        || std::env::var_os("GDK_BACKEND").is_some()
        || std::env::var("XDG_SESSION_TYPE").as_deref() != Ok("wayland")
        || std::env::var_os("DISPLAY").is_none()
    {
        return;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    // GNOME is the one major compositor with neither layer-shell nor
    // keep-above; everything else stays native Wayland.
    if desktop.to_uppercase().contains("GNOME") {
        // SAFETY: called before GTK init on the main thread, no other threads.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
        tracing::info!(
            "GNOME Wayland: using XWayland for always-on-top (dropdown.prefer_xwayland)"
        );
    }
}

fn controller(config: &Config) -> std::io::Result<TmuxController> {
    let data_dir = SessionStore::default_root();
    let conf = hashterm_tmux::conf::install_conf(
        &data_dir,
        config.scrollback.lines,
        config.tmux.native_windows,
        config.tmux.extra_conf.as_deref(),
    )?;
    Ok(TmuxController::new(socket_name(), conf))
}

fn run_dump(
    config: &Config,
    name: Option<String>,
    auto: bool,
    quiet: bool,
) -> std::process::ExitCode {
    let ctl = match controller(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hashterm dump: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let store = SessionStore::new(SessionStore::default_root());
    let auto = auto || name.is_none();
    let name = name.unwrap_or_else(hashterm_session::dump::now_stamp);
    let opts = DumpOptions {
        exclude_panes: config.restore.exclude_panes.clone(),
        max_scrollback_lines: config.restore.max_scrollback_lines,
    };
    match hashterm_session::dump_server(&ctl, &store, &name, auto, &opts) {
        Ok(manifest) => {
            if auto {
                let _ = store.prune_autosaves(config.session.autosave_keep as usize);
            }
            if !quiet {
                println!(
                    "saved '{name}'{}: {} session(s)",
                    if auto { " (autosave)" } else { "" },
                    manifest.sessions.len()
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(hashterm_session::dump::DumpError::Empty) => {
            if !quiet {
                eprintln!("hashterm dump: no hashterm sessions running; nothing saved");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hashterm dump: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_restore(
    config: &Config,
    name: &str,
    on_conflict: &str,
    auto: bool,
) -> std::process::ExitCode {
    let ctl = match controller(config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hashterm restore: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(e) = ctl.ensure_server() {
        eprintln!("hashterm restore: cannot start tmux server: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let store = SessionStore::new(SessionStore::default_root());
    let Some((name, auto)) = store
        .resolve(name)
        .or_else(|| auto.then(|| (name.to_owned(), true)))
    else {
        eprintln!("hashterm restore: no save named '{name}'");
        return std::process::ExitCode::FAILURE;
    };

    let helper = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hashterm"));
    let opts = RestoreOptions {
        on_conflict: match on_conflict {
            "skip" => OnConflict::Skip,
            "replace" => OnConflict::Replace,
            _ => OnConflict::Rename,
        },
        programs: config.restore.programs.clone(),
        restart_arbitrary: config.restore.restart_arbitrary,
        pretype_unrestored: config.restore.pretype_unrestored,
        shell: config.general.shell.clone(),
        helper,
        resume: config.restore.resume.clone(),
    };
    match hashterm_session::restore_save(&ctl, &store, &name, auto, &opts) {
        Ok(report) => {
            for (saved, live) in &report.restored {
                if saved == live {
                    println!("restored {saved}");
                } else {
                    println!("restored {saved} as {live}");
                }
            }
            for s in &report.skipped {
                println!("skipped {s} (already running)");
            }
            for n in &report.notes {
                println!("note: {n}");
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hashterm restore: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `--restart`: stop the running GUI (gracefully, so it snapshots) and launch
/// a fresh detached instance. Terminals live on the tmux server, which stays
/// up, so the new instance simply re-adopts them.
fn run_restart(config: Option<&std::path::Path>) -> std::process::ExitCode {
    use std::os::unix::process::CommandExt;

    let alive = |pid: u32| unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    // Only signal a PID that is actually a hashterm: a stale lock's PID may have
    // been reused by an unrelated process, and we must never SIGTERM that. Verify
    // via /proc/<pid>/exe (which resolves to the on-disk binary and cannot be
    // forged with prctl(PR_SET_NAME), unlike /proc/<pid>/comm) against our own.
    let our_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let is_hashterm = |pid: u32| {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .zip(our_exe.clone())
            .is_some_and(|(theirs, ours)| theirs == ours)
    };
    let self_pid = std::process::id();
    if let Some(pid) = hashterm_core::state::running_pid()
        && pid != self_pid
        && alive(pid)
        && is_hashterm(pid)
    {
        eprintln!("hashterm: stopping running instance (pid {pid})");
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        // Wait for it to exit and release the session-bus name (up to ~6s).
        let mut waited = 0;
        while alive(pid) && waited < 60 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            waited += 1;
        }
        if alive(pid) {
            eprintln!("hashterm: instance did not exit in time; forcing");
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        // A deliberate restart is a CLEAN stop: clear the lock the SIGTERM
        // handler intentionally leaves behind, so the fresh instance doesn't
        // greet the user with a spurious "restore previous session?" prompt.
        // (If no instance was running, a stale lock from a real crash is left
        // in place so genuine crash-recovery still offers a restore.)
        hashterm_core::state::clear_session_lock();
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hashterm"));
    let mut cmd = std::process::Command::new(exe);
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach into a new session so the fresh GUI outlives this CLI and its
    // controlling terminal (which is a tmux pane of the instance we just
    // killed).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            eprintln!("hashterm: started fresh instance (pid {})", child.id());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hashterm: could not start new instance: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_saves() -> std::process::ExitCode {
    let store = SessionStore::new(SessionStore::default_root());
    match store.list() {
        Ok(saves) if saves.is_empty() => {
            println!("no saved sessions");
            std::process::ExitCode::SUCCESS
        }
        Ok(saves) => {
            for s in saves {
                println!(
                    "{}  {}  {} session(s){}",
                    s.created_at,
                    s.name,
                    s.session_count,
                    if s.auto { "  [autosave]" } else { "" }
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hashterm saves: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
