use gtk4::gio::{self, ApplicationFlags};
use gtk4::glib;
use gtk4::prelude::*;
use hashterm_core::config::Config;
use hashterm_core::ipc::{TmuxEvent, socket_path};
use hashterm_core::{app_id, socket_name};
use hashterm_session::SessionStore;
use hashterm_tmux::{TmuxController, TmuxEventBus};
use std::cell::RefCell;
use std::rc::Rc;

use crate::css::Styles;
use crate::tabbar::Badge;
use crate::window::MainWindow;

/// Build and run the GtkApplication. Single-instance: a second invocation's
/// command line is forwarded to the primary via GApplication D-Bus activation.
/// `startup_warning` (e.g. a config parse failure) is shown as an OSD once
/// the window exists — silent fallback to defaults confuses users.
pub fn run(config: Config, gtk_args: Vec<String>, startup_warning: Option<String>) -> i32 {
    let app = gtk4::Application::builder()
        .application_id(app_id())
        .flags(ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let config = Rc::new(RefCell::new(config));
    let window: Rc<RefCell<Option<Rc<MainWindow>>>> = Rc::new(RefCell::new(None));
    let startup_warning = Rc::new(RefCell::new(startup_warning));

    register_actions(&app, &window);
    // Startup conflicts land in the startup-warning OSD once the window is up.
    if let Some(conflict) = apply_accels(&app, &config.borrow())
        && startup_warning.borrow().is_none()
    {
        *startup_warning.borrow_mut() = Some(conflict);
    }

    app.connect_command_line({
        let config = config.clone();
        let window = window.clone();
        move |app, cmdline| {
            let mut args: Vec<String> = cmdline
                .arguments()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            // SECURITY: a "remote" command line was forwarded over the
            // session bus by some other process — potentially any peer that
            // can reach org.gtk.Application.CommandLine (a sandboxed app, a
            // program running inside a pane). It must NOT be able to make us
            // run arbitrary programs or open attacker-chosen paths. Locally
            // typed invocations (is_remote() == false) keep full power.
            if cmdline.is_remote() {
                args = sanitize_remote_args(&args);
            }
            // --toggle owns visibility itself; a first launch with
            // general.start_hidden waits for the hotkey; everything else
            // presents (a second plain `hashterm` invocation always shows).
            let first_launch = window.borrow().is_none();
            let stay_hidden = args.iter().any(|a| a == "--toggle")
                || (first_launch && config.borrow().general.start_hidden);
            let present = !stay_hidden;
            ensure_window(app, &config, &window, present);
            let w = window.borrow().as_ref().cloned();
            if let Some(w) = w {
                if let Some(warning) = startup_warning.borrow_mut().take() {
                    w.show_osd_message(&warning, 6000);
                }
                dispatch_flags(&w, &args);
            }
            glib::ExitCode::SUCCESS
        }
    });

    app.run_with_args(&gtk_args).into()
}

fn ensure_window(
    app: &gtk4::Application,
    config: &Rc<RefCell<Config>>,
    window: &Rc<RefCell<Option<Rc<MainWindow>>>>,
    present: bool,
) {
    if let Some(w) = window.borrow().as_ref() {
        if present {
            w.present();
        }
        return;
    }

    let display = gtk4::gdk::Display::default().expect("display");
    let styles = Rc::new(Styles::install(&display, &config.borrow()));

    let ctl = match build_controller(&config.borrow()) {
        Ok(c) => Rc::new(c),
        Err(e) => {
            tracing::error!("tmux setup failed: {e}");
            // A terminal without tmux is useless; bail out of this activation.
            app.quit();
            return;
        }
    };

    let backend = hashterm_platform::detect_backend(&display);
    tracing::info!("overlay backend: {}", backend.name());
    let win = MainWindow::new(app, config.clone(), ctl.clone(), backend);
    if present {
        win.initial_present();
    }
    *window.borrow_mut() = Some(win.clone());

    // Debug aid: HASHTERM_SHOT=/path.png renders the window content to a PNG
    // (offscreen GSK render of the widget tree) 3s after startup. Debug-only:
    // in a release binary an attacker-set env must not choose an output path.
    if let Some(path) = std::env::var_os("HASHTERM_SHOT").filter(|_| cfg!(debug_assertions)) {
        let w = Rc::downgrade(&win);
        let path = std::path::PathBuf::from(path);
        glib::timeout_add_seconds_local(3, move || {
            if let Some(w) = w.upgrade() {
                self_screenshot(w.gtk_window(), &path);
            }
            glib::ControlFlow::Break
        });
    }

    // Debug aid: HASHTERM_GEOMETRY=1 logs the allocation chain periodically.
    if cfg!(debug_assertions) && std::env::var_os("HASHTERM_GEOMETRY").is_some() {
        let w = Rc::downgrade(&win);
        glib::timeout_add_seconds_local(2, move || match w.upgrade() {
            Some(w) => {
                w.debug_geometry();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    wire_event_bus(&win);
    wire_reconciler(&win);
    wire_config_reload(config, &win, &styles);
    wire_autosave(config, &win);
    wire_global_hotkey(&display, config, &win);
    sync_autostart(config.borrow().general.autostart);
}

/// Keep ~/.config/autostart/ in sync with general.autostart: the desktop
/// launches hashterm on login (honoring start_hidden). Called at startup and
/// on config reload.
fn sync_autostart(enabled: bool) {
    let dir = glib::user_config_dir().join("autostart");
    let path = dir.join("com.redasgard.Hashterm.desktop");
    if enabled {
        // Hard-coded `Exec=hashterm` (resolved via PATH): never interpolate
        // current_exe(), whose path needs XDG quoting and could carry a
        // newline that injects extra desktop-entry keys.
        let entry = "[Desktop Entry]\n\
             Type=Application\n\
             Name=hashterm\n\
             Comment=tmux-native drop-down terminal\n\
             Exec=hashterm\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n";
        let up_to_date = std::fs::read_to_string(&path).ok().as_deref() == Some(entry);
        if !up_to_date {
            let write = || -> std::io::Result<()> {
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::create_dir_all(&dir)?;
                // O_NOFOLLOW: refuse to write through a symlink planted at the
                // autostart path.
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)?;
                f.write_all(entry.as_bytes())
            };
            if let Err(e) = write() {
                tracing::error!("cannot write autostart entry {}: {e}", path.display());
            } else {
                tracing::info!("autostart enabled: {}", path.display());
            }
        }
    } else if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::error!("cannot remove autostart entry {}: {e}", path.display());
        } else {
            tracing::info!("autostart disabled");
        }
    }
}

/// System-wide dropdown toggle: portal first, XGrabKey on X11, otherwise the
/// user binds `hashterm --toggle` externally (always works).
fn wire_global_hotkey(
    display: &gtk4::gdk::Display,
    config: &Rc<RefCell<Config>>,
    win: &Rc<MainWindow>,
) {
    if !config.borrow().dropdown.enabled {
        return;
    }
    let accel = hashterm_core::config::normalize_accel(&config.borrow().hotkeys.toggle_dropdown);
    let this = Rc::downgrade(win);
    let kind = hashterm_platform::register_toggle_hotkey(
        &accel,
        hashterm_platform::is_x11(display),
        Rc::new(move || {
            if let Some(w) = this.upgrade() {
                w.toggle_dropdown_from("portal-or-grab");
            }
        }),
    );
    tracing::info!("global hotkey source: {kind:?}");
    if kind == hashterm_platform::HotkeySourceKind::External {
        tracing::warn!(
            "no in-process global hotkey available; bind `hashterm --toggle` \
             to {accel} in your desktop's keyboard settings"
        );
    }
}

/// Drop the command-executing / path-taking verbs from a bus-forwarded
/// command line, keeping only the safe presence verbs. A remote peer may ask
/// us to show ourselves or open an empty tab; it may not choose the program,
/// the working directory, or a session to restore.
fn sanitize_remote_args(args: &[String]) -> Vec<String> {
    // Whitelist: from a remote peer, only the value-less presence verbs are
    // honored. `-e/--exec` (+ its trailing argv), `--cwd`, `--restore`,
    // `--config` and their values are dropped regardless of position.
    let mut out = Vec::with_capacity(args.len());
    if let Some(a0) = args.first() {
        out.push(a0.clone()); // argv[0]: GApplicationCommandLine includes it
    }
    let mut it = args.iter().skip(1);
    let mut dropped = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--toggle" | "--new-tab" => out.push(a.clone()),
            "-e" | "--exec" => {
                dropped = true;
                break; // consumes all following args
            }
            "--cwd" | "--restore" | "--config" => {
                dropped = true;
                let _ = it.next(); // discard the value too
            }
            _ => dropped = true, // unknown / bare command word: drop
        }
    }
    if dropped {
        tracing::warn!("ignored command-executing flags from a remote (D-Bus) invocation");
    }
    out
}

/// Minimal parse of action flags forwarded from a secondary invocation.
/// Remote invocations are pre-filtered by `sanitize_remote_args`.
fn dispatch_flags(win: &Rc<MainWindow>, args: &[String]) {
    let mut it = args.iter().skip(1).peekable();
    let mut new_tab = false;
    let mut toggle = false;
    let mut cwd: Option<std::path::PathBuf> = None;
    let mut restore: Option<String> = None;
    let mut exec: Option<Vec<String>> = None;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--toggle" => toggle = true,
            "--new-tab" => new_tab = true,
            "--cwd" => cwd = it.next().map(Into::into),
            "--restore" => restore = it.next().cloned(),
            "-e" | "--exec" => {
                exec = Some(it.by_ref().cloned().collect());
            }
            _ => {}
        }
    }
    if toggle {
        win.toggle_dropdown_from("cli");
    }
    if new_tab {
        win.new_tab_with(cwd, exec);
    }
    if let Some(name) = restore {
        win.restore_named(&name);
    }
}

/// Offscreen render of the window's widget tree to a PNG (debug aid).
fn self_screenshot(win: &gtk4::ApplicationWindow, path: &std::path::Path) {
    let Some(child) = win.child() else { return };
    let paintable = gtk4::WidgetPaintable::new(Some(&child));
    let w = paintable.intrinsic_width();
    let h = paintable.intrinsic_height();
    if w == 0 || h == 0 {
        tracing::warn!("self-screenshot: zero-size paintable");
        return;
    }
    let snapshot = gtk4::Snapshot::new();
    gtk4::prelude::PaintableExt::snapshot(&paintable, &snapshot, w as f64, h as f64);
    let Some(node) = snapshot.to_node() else {
        tracing::warn!("self-screenshot: empty render node");
        return;
    };
    let renderer = gtk4::gsk::CairoRenderer::new();
    if let Err(e) = renderer.realize(None) {
        tracing::warn!("self-screenshot: renderer realize failed: {e}");
        return;
    }
    let texture = renderer.render_texture(&node, None);
    match texture.save_to_png(path) {
        Ok(()) => tracing::info!("self-screenshot written to {}", path.display()),
        Err(e) => tracing::warn!("self-screenshot save failed: {e}"),
    }
    renderer.unrealize();
}

fn build_controller(config: &Config) -> std::io::Result<TmuxController> {
    let conf = hashterm_tmux::conf::install_conf(
        &SessionStore::default_root(),
        config.scrollback.lines,
        config.tmux.native_windows,
        config.tmux.extra_conf.as_deref(),
    )?;
    let ctl = TmuxController::new(socket_name(), conf);
    // copy-command is a server option read from the conf only at server
    // birth; apply it live too so an ALREADY-running server (sessions from a
    // previous hashterm) also pipes copies to the system clipboard.
    if let Some(tool) = hashterm_tmux::conf::clipboard_tool() {
        let _ = ctl.run(&["set-option", "-s", "copy-command", tool]);
    }
    Ok(ctl)
}

type WindowAction = Box<dyn Fn(&Rc<MainWindow>)>;

fn register_actions(app: &gtk4::Application, window: &Rc<RefCell<Option<Rc<MainWindow>>>>) {
    let acts: [(&str, WindowAction); 12] = [
        ("new-tab", Box::new(|w| w.new_tab())),
        ("close-tab", Box::new(|w| w.close_active_tab())),
        ("next-tab", Box::new(|w| w.cycle_tab(true))),
        ("prev-tab", Box::new(|w| w.cycle_tab(false))),
        ("toggle-tabbar", Box::new(|w| w.toggle_tabbar())),
        (
            "toggle-dropdown",
            Box::new(|w| w.toggle_dropdown_from("action")),
        ),
        ("sessions", Box::new(crate::picker::open)),
        ("open-config", Box::new(open_config_file)),
        ("copy", Box::new(|w| w.copy_selection())),
        ("paste", Box::new(|w| w.paste_clipboard())),
        ("next-group", Box::new(|w| w.cycle_group(true))),
        ("prev-group", Box::new(|w| w.cycle_group(false))),
    ];
    for (name, f) in acts {
        let action = gio::SimpleAction::new(name, None);
        let window = window.clone();
        action.connect_activate(move |_, _| {
            if let Some(w) = window.borrow().as_ref() {
                f(w);
            }
        });
        app.add_action(&action);
    }
}

/// Returns a short human-readable description of the first hotkey conflict
/// (for the OSD), if any.
fn apply_accels(app: &gtk4::Application, config: &Config) -> Option<String> {
    use hashterm_core::config::normalize_accel;
    let hk = &config.hotkeys;
    let pairs = [
        ("app.new-tab", &hk.new_tab),
        ("app.close-tab", &hk.close_tab),
        ("app.next-tab", &hk.next_tab),
        ("app.prev-tab", &hk.prev_tab),
        ("app.toggle-tabbar", &hk.toggle_tabbar),
        ("app.sessions", &hk.session_picker),
        ("app.open-config", &hk.open_config),
        ("app.copy", &hk.copy),
        ("app.paste", &hk.paste),
        ("app.next-group", &hk.next_group),
        ("app.prev-group", &hk.prev_group),
    ];
    // Two actions on one accel = only one of them fires (GTK picks silently).
    // Classic trigger: a config written before a default changed pins the old
    // key that a NEW default now also uses. Surface it instead of guessing.
    let mut conflict = None;
    for (i, (action, accel)) in pairs.iter().enumerate() {
        let norm = normalize_accel(accel);
        for (other, other_accel) in &pairs[i + 1..] {
            if normalize_accel(other_accel) == norm {
                tracing::warn!(
                    "hotkey conflict: '{accel}' is bound to both {action} and {other} — \
                     only one will fire; change one of them in [hotkeys]"
                );
                conflict.get_or_insert_with(|| {
                    let strip = |a: &str| a.trim_start_matches("app.").to_owned();
                    format!(
                        "Hotkey conflict: '{accel}' is bound to both {} and {}",
                        strip(action),
                        strip(other)
                    )
                });
            }
        }
    }
    for (action, accel) in pairs {
        if action == "app.paste" {
            // Shift+Insert is muscle memory for terminal paste everywhere.
            app.set_accels_for_action(action, &[&normalize_accel(accel), "<Shift>Insert"]);
        } else {
            app.set_accels_for_action(action, &[&normalize_accel(accel)]);
        }
    }
    conflict
}

/// The config file IS the settings dialog (live-reloaded on save): open it in
/// the user's default editor.
fn open_config_file(win: &Rc<MainWindow>) {
    // Guarantee there is a (commented) file to open.
    let _ = Config::install_default_file();
    let uri = format!("file://{}", Config::default_path().display());
    gtk4::UriLauncher::new(&uri).launch(Some(win.gtk_window()), gio::Cancellable::NONE, |result| {
        if let Err(e) = result {
            tracing::error!("cannot open config in editor: {e}");
        }
    });
}

/// tmux hooks -> hashterm-ctl -> unix socket -> this loop on the main context.
fn wire_event_bus(win: &Rc<MainWindow>) {
    let bus = match TmuxEventBus::start(&socket_path()) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("ipc socket unavailable ({e}); relying on polling only");
            return;
        }
    };
    let win = Rc::downgrade(win);
    glib::spawn_future_local(async move {
        // Bus (and its socket cleanup) lives as long as this loop.
        let _keep = &bus;
        while let Ok(event) = bus.receiver.recv().await {
            let Some(win) = win.upgrade() else { break };
            match event {
                TmuxEvent::SessionCreated { .. }
                | TmuxEvent::SessionClosed { .. }
                | TmuxEvent::SessionRenamed { .. }
                | TmuxEvent::ClientAttached { .. }
                | TmuxEvent::ClientDetached { .. } => win.sync_from_server(),
                TmuxEvent::Bell { session } => win.set_badge(&session, Badge::Bell),
                TmuxEvent::Activity { session } => win.set_badge(&session, Badge::Activity),
                TmuxEvent::NewTab => win.new_tab(),
                TmuxEvent::NextTab => win.cycle_tab(true),
                TmuxEvent::PrevTab => win.cycle_tab(false),
            }
        }
    });
}

/// Hooks can be lost; a lazy poll reconciles as the source of truth.
fn wire_reconciler(win: &Rc<MainWindow>) {
    let win = Rc::downgrade(win);
    glib::timeout_add_seconds_local(30, move || match win.upgrade() {
        Some(w) => {
            w.sync_from_server();
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
}

/// Watch config.toml; on change parse + diff-apply, keep old config on error.
fn wire_config_reload(config: &Rc<RefCell<Config>>, win: &Rc<MainWindow>, styles: &Rc<Styles>) {
    let path = Config::default_path();
    let file = gio::File::for_path(&path);
    let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else {
        return;
    };
    let config = config.clone();
    let win = Rc::downgrade(win);
    let styles = styles.clone();
    monitor.connect_changed(move |_, _, _, event| {
        if !matches!(
            event,
            gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created
        ) {
            return;
        }
        match Config::load(&path) {
            Ok(new_cfg) => {
                if *config.borrow() == new_cfg {
                    return;
                }
                let old_dd = config.borrow().dropdown.clone();
                let old_opacity = config.borrow().appearance.window_opacity;
                *config.borrow_mut() = new_cfg;
                styles.apply(&config.borrow());
                // Geometry edited in the config beats the remembered height:
                // forget it and resize the live window right away.
                let dd = config.borrow().dropdown.clone();
                let geometry_changed = dd.height != old_dd.height
                    || dd.height_pct != old_dd.height_pct
                    || dd.width_pct != old_dd.width_pct
                    || dd.edge != old_dd.edge;
                if geometry_changed {
                    let mut st = hashterm_core::state::UiState::load();
                    st.window_height_px = None;
                    st.save();
                }
                if let Some(w) = win.upgrade() {
                    // An edited window_opacity in the file beats the value
                    // remembered from Ctrl+Alt+Shift+scroll.
                    if config.borrow().appearance.window_opacity != old_opacity {
                        w.clear_opacity_override();
                    }
                    w.apply_config();
                    if geometry_changed {
                        w.reapply_geometry();
                    }
                    if let Some(conflict) =
                        apply_accels(&w.gtk_window().application().unwrap(), &config.borrow())
                    {
                        w.show_osd_message(&conflict, 4000);
                    }
                }
                sync_autostart(config.borrow().general.autostart);
                tracing::info!("config reloaded");
            }
            Err(e) => {
                tracing::error!("config reload skipped: {e}");
                if let Some(w) = win.upgrade() {
                    w.show_osd_message(
                        &format!("⚠ config error — keeping last good config\n{e}"),
                        5000,
                    );
                }
            }
        }
    });
    // Keep the monitor alive for the app's lifetime.
    std::mem::forget(monitor);
}

/// Periodic full dump into the rotated autosave area, off the main thread.
fn wire_autosave(config: &Rc<RefCell<Config>>, win: &Rc<MainWindow>) {
    let interval = config.borrow().session.autosave_interval_secs;
    if interval == 0 {
        return;
    }
    let cfg = config.clone();
    let _win = Rc::downgrade(win);
    glib::timeout_add_seconds_local(interval.min(u32::MAX as u64) as u32, move || {
        let snapshot = cfg.borrow().clone();
        std::thread::spawn(move || {
            let Ok(conf) = hashterm_tmux::conf::install_conf(
                &SessionStore::default_root(),
                snapshot.scrollback.lines,
                snapshot.tmux.native_windows,
                snapshot.tmux.extra_conf.as_deref(),
            ) else {
                return;
            };
            let ctl = TmuxController::new(socket_name(), conf);
            let store = SessionStore::new(SessionStore::default_root());
            let opts = hashterm_session::DumpOptions {
                exclude_panes: snapshot.restore.exclude_panes.clone(),
                max_scrollback_lines: snapshot.restore.max_scrollback_lines,
            };
            let stamp = hashterm_session::dump::now_stamp();
            match hashterm_session::dump_server(&ctl, &store, &stamp, true, &opts) {
                Ok(_) => {
                    let _ = store.prune_autosaves(snapshot.session.autosave_keep as usize);
                    tracing::debug!("autosaved {stamp}");
                }
                Err(hashterm_session::dump::DumpError::Empty) => {}
                Err(e) => tracing::warn!("autosave failed: {e}"),
            }
        });
        glib::ControlFlow::Continue
    });
}

#[cfg(test)]
mod tests {
    use super::sanitize_remote_args;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn remote_args_drop_exec_and_value_verbs() {
        // -e and everything after it is dropped.
        assert_eq!(
            sanitize_remote_args(&v(&["hashterm", "--new-tab", "-e", "sh", "-c", "evil"])),
            v(&["hashterm", "--new-tab"])
        );
        // --cwd / --restore / --config and their values are dropped anywhere.
        assert_eq!(
            sanitize_remote_args(&v(&["hashterm", "--cwd", "/root", "--toggle"])),
            v(&["hashterm", "--toggle"])
        );
        assert_eq!(
            sanitize_remote_args(&v(&["hashterm", "--restore", "x"])),
            v(&["hashterm"])
        );
        // Bare presence verbs survive.
        assert_eq!(
            sanitize_remote_args(&v(&["hashterm", "--toggle"])),
            v(&["hashterm", "--toggle"])
        );
    }
}
