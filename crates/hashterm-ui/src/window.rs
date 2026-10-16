//! Main application window: frameless shell, tab bar on any edge with
//! auto-hide, a GtkStack of terminal pages, and per-tab tmux sessions on the
//! dedicated server. Re-adopts live ht-* sessions at startup.

use gtk4::prelude::*;
use hashterm_core::config::{Config, Edge};
use hashterm_core::state::WindowGeometry;
use hashterm_tmux::TmuxController;
use hashterm_tmux::controller::NewSessionOpts;
use std::cell::RefCell;
use std::rc::Rc;

use std::sync::atomic::AtomicBool;

/// Set by the SIGTERM/SIGHUP handler; polled on the glib main loop to trigger a
/// final snapshot before exit. Only an atomic store happens in signal context.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A receding neighbor row of the tab dial. `distance` is 1 for an immediate
/// neighbor, 2 for the one beyond it (smaller, fainter).
fn osd_neighbor_label(text: &str, distance: usize) -> gtk4::Label {
    let label = gtk4::Label::builder()
        .label(text)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .max_width_chars(48)
        .halign(gtk4::Align::Center)
        .build();
    label.add_css_class(if distance >= 2 {
        "osd-far"
    } else {
        "osd-neighbor"
    });
    label
}

/// The center row of the wheel: bright title with a small, dim "i/N" suffix.
fn osd_current_label(title: &str, i: usize, n: usize) -> gtk4::Label {
    let markup = format!(
        "{}  <span size='small' foreground='#9aa1ab'>{i}/{n}</span>",
        gtk4::glib::markup_escape_text(title)
    );
    let label = gtk4::Label::builder()
        .use_markup(true)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .max_width_chars(48)
        .halign(gtk4::Align::Center)
        .build();
    label.set_markup(&markup);
    label.add_css_class("osd-current");
    label
}

extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

use crate::csd;
use crate::groupbar::{GroupBar, GroupBarEvent};
use crate::page::TerminalPage;
use crate::slide::SlidePanel;
use crate::tabbar::{Badge, TabBar, TabBarEvent, TabInfo};
use hashterm_platform::OverlayBackend;

/// Facts about a tab captured just before it is removed.
struct Removal {
    group: String,
    /// Position of the tab within its group.
    index: usize,
    /// It was the tab being shown.
    was_active: bool,
    /// The group's color, to restore if the group is kept.
    color: Option<String>,
    /// Group to land on if the group disappears.
    neighbor: Option<String>,
    /// Position of the group in the group bar (restored if it is kept).
    group_index: usize,
}

struct TabEntry {
    /// tmux session name; also the Stack child name.
    session: String,
    title: String,
    /// User renamed this tab: automatic titles are suppressed.
    title_custom: bool,
    /// "#rrggbb" accent, drawn as tab underline + inner terminal ring.
    accent: Option<String>,
    /// Tab group; None = DEFAULT_GROUP.
    group: Option<String>,
    /// Group color "#rrggbb" (outer terminal ring, group-bar dot).
    group_color: Option<String>,
    badge: Option<Badge>,
    /// Last title the terminal reported (OSC 2 or tmux's default pane title).
    vte_title: String,
    /// Foreground program and folder of the active pane (polled from tmux).
    fact_cmd: String,
    fact_path: String,
    page: TerminalPage,
}

impl TabEntry {
    fn group_name(&self) -> &str {
        self.group
            .as_deref()
            .unwrap_or(hashterm_tmux::controller::DEFAULT_GROUP)
    }
}

pub struct MainWindow {
    window: gtk4::ApplicationWindow,
    grid: gtk4::Grid,
    overlay: gtk4::Overlay,
    stack: gtk4::Stack,
    tabbar: Rc<TabBar>,
    /// The matrix's other axis: groups on the edge perpendicular to the tabs.
    groupbar: Rc<GroupBar>,
    hover_strip: gtk4::Box,
    slide: SlidePanel,
    backend: Rc<Box<dyn OverlayBackend>>,
    tabs: RefCell<Vec<TabEntry>>,
    ctl: Rc<TmuxController>,
    config: Rc<RefCell<Config>>,
    /// Capture-phase grabs (toggle key, goto-tab digits) so VTE never
    /// swallows them.
    toggle_shortcuts: gtk4::ShortcutController,
    current_shortcuts: RefCell<Vec<gtk4::Shortcut>>,
    /// Debounce: portal + in-app grab may both fire for one keypress.
    last_toggle: std::cell::Cell<Option<std::time::Instant>>,
    /// The window actually held focus since it was last shown — required
    /// before hide_on_focus_loss may fire (focus-stealing prevention can show
    /// us unfocused, which must not count as "lost focus").
    held_focus: std::cell::Cell<bool>,
    /// A modal confirmation (close group / empty group) is on screen: the
    /// dropdown must not hide because the dialog took focus.
    modal_open: std::cell::Cell<bool>,
    /// Set on every show: the toggle key that summoned the window is likely
    /// still physically held, and its AUTO-REPEAT presses would land in the
    /// freshly-focused window and instantly toggle it back off. Cleared by a
    /// key release, or by a press arriving after a gap no repeat train has
    /// (repeats tick every few dozen ms; >1.2s of quiet = a new keystroke).
    capture_suppressed: std::cell::Cell<bool>,
    shown_at: std::cell::Cell<Option<std::time::Instant>>,
    last_capture_attempt: std::cell::Cell<Option<std::time::Instant>>,
    /// Keyvals of the toggle key (incl. its shift sibling). While suppression
    /// is active, bare presses of these are swallowed: XWayland can deliver a
    /// stuck auto-repeat of the summoning key WITHOUT modifiers (the
    /// compositor consumed press+release), which would type into the shell.
    suppress_keyvals: RefCell<Vec<gtk4::gdk::Key>>,
    /// Ctrl+Shift+B pinned the bar open: auto-hide must not conceal it until
    /// the user toggles it hidden again.
    tabbar_pinned: std::cell::Cell<bool>,
    /// Hide animation running: any toggle arriving now is the late duplicate
    /// of the same keypress from another source (in-window grab fires
    /// instantly, the portal's D-Bus round-trip lands 200-400ms later) — it
    /// must be absorbed, or it re-raises the emptied window mid-animation.
    hiding: std::cell::Cell<bool>,
    /// Set when the tab/group UI was refreshed while the window was unmapped;
    /// GTK4 leaks ~390 kB per widget rebuild on an unmapped window, so hidden
    /// refreshes only mark this and one rebuild runs at next map.
    ui_dirty: std::cell::Cell<bool>,
    /// Geometry-memory poller state: the last settled geometry since the
    /// window was mapped (None until one settles) and the previous sample.
    /// A change is persisted only once two consecutive samples agree, so a
    /// drag or resize in progress is not written on every tick.
    geometry_settled: std::cell::Cell<Option<WindowGeometry>>,
    geometry_pending: std::cell::Cell<Option<WindowGeometry>>,
    /// Whole-window opacity set live via Ctrl+Alt+Shift+scroll. Overrides
    /// appearance.window_opacity; an edit of that config value clears it.
    opacity_override: std::cell::Cell<Option<f64>>,
    /// Debounce for persisting the override (scroll emits event storms).
    opacity_save_timer: std::cell::Cell<Option<gtk4::glib::SourceId>>,
    /// Centered fade-out toast shown after tab switches.
    osd_revealer: gtk4::Revealer,
    /// One-time guidance strip (first-run hint, hotkey problems).
    notice: Rc<crate::notice::Notice>,
    title_poll_busy: std::cell::Cell<bool>,
    /// Scrollback find bar (built once the window Rc exists).
    search: RefCell<Option<Rc<crate::search::SearchBar>>>,
    search_gen: hashterm_tmux::search::Generation,
    /// The tab the open find bar last searched (the visible tab may differ by
    /// the time the bar is closed after a tab switch).
    search_target: RefCell<Option<String>>,
    /// Window-local text zoom; 1.0 is the configured size.
    font_scale: std::cell::Cell<f64>,
    last_layout_change: std::cell::Cell<std::time::Instant>,
    activity_gates: RefCell<std::collections::HashMap<String, crate::attention::ActivityGate>>,
    notify_limiter: RefCell<crate::attention::RateLimiter>,
    /// Regenerates the config-derived CSS (set by the app, which owns the provider).
    restyle: RefCell<Option<Rc<dyn Fn()>>>,
    /// What the global hotkey is really doing (portal answers asynchronously).
    hotkey_status: RefCell<hashterm_platform::HotkeyStatus>,
    /// The toast body: rebuilt per show — one line for a message, or a
    /// prev/current/next wheel for a tab switch.
    osd_content: gtk4::Box,
    osd_timeout: std::cell::Cell<Option<gtk4::glib::SourceId>>,
    /// Generated per-accent-color CSS rules (tab underline, terminal rings).
    accent_css: gtk4::CssProvider,
    /// Workspaces: which group's tabs the bar shows.
    active_group: RefCell<String>,
    /// Display order of groups (persisted; pruned to live groups).
    group_order: RefCell<Vec<String>>,
    /// Per-group last-selected tab (session name).
    last_active: RefCell<std::collections::HashMap<String, String>>,
}

impl MainWindow {
    pub fn new(
        app: &gtk4::Application,
        config: Rc<RefCell<Config>>,
        ctl: Rc<TmuxController>,
        backend: Box<dyn OverlayBackend>,
    ) -> Rc<Self> {
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("hashterm")
            .default_width(1000)
            .default_height(600)
            .decorated(false)
            .build();
        window.add_css_class("hashterm");

        // Plain-window mode has no dropdown placement to carry the remembered
        // size, so apply it as the initial size here (position follows in
        // initial_present, once the window is realized).
        if !config.borrow().dropdown.enabled
            && config.borrow().window.remember_size
            && let Some(g) = hashterm_core::state::UiState::load().window_geometry
            && let (Some(w), Some(h)) = (g.width, g.height)
            && w > 0
            && h > 0
        {
            window.set_default_size(w, h);
        }

        // Layer-shell must claim the window BEFORE it is first realized.
        if config.borrow().dropdown.enabled
            && let Err(e) = backend.prepare(&window, &config.borrow().dropdown)
        {
            tracing::warn!("overlay backend '{}' unavailable: {e}", backend.name());
        }

        let stack = gtk4::Stack::builder()
            .transition_type(gtk4::StackTransitionType::Crossfade)
            .transition_duration(80)
            .hexpand(true)
            .vexpand(true)
            .build();

        let tabbar = TabBar::new(&config.borrow().tabbar);
        let groupbar = GroupBar::new(
            resolved_groupbar_edge(&config.borrow()),
            config.borrow().groupbar.reveal_ms,
        );

        let grid = gtk4::Grid::new();
        grid.attach(&stack, 1, 1, 1, 1);

        // 2px strip that reveals an auto-hidden bar on hover.
        let hover_strip = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        hover_strip.add_css_class("hover-strip");
        hover_strip.set_visible(false);

        let overlay = gtk4::Overlay::builder().child(&grid).build();
        // The console's visible body: background/corners/lip live HERE, on
        // the widget the SlidePanel translates — never on the window node
        // (see css.rs) — so show/hide reads as the console itself sliding.
        overlay.add_css_class("console-body");
        overlay.add_overlay(&hover_strip);
        csd::add_resize_handles(&overlay, &window);

        // Tab-switch OSD: centered translucent toast, click-through, faded
        // in/out by a crossfade Revealer. The content box holds either a
        // single message line or a prev/current/next wheel.
        let osd_content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(2)
            .halign(gtk4::Align::Center)
            .build();
        osd_content.add_css_class("tab-osd");
        let osd_revealer = gtk4::Revealer::builder()
            .child(&osd_content)
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .reveal_child(false)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .can_target(false)
            .build();
        overlay.add_overlay(&osd_revealer);
        let notice = crate::notice::Notice::new();
        overlay.add_overlay(notice.widget());

        // The whole content sits in a SlidePanel; in normal-window mode the
        // offset simply stays 0 and it is a transparent pass-through.
        let slide = SlidePanel::new(config.borrow().dropdown.edge);
        slide.set_child(Some(&overlay));
        window.set_child(Some(&slide));

        // Capture phase beats VTE's key handling: the toggle key must act on
        // the window, never type an escape-sequence tail into the shell.
        let toggle_shortcuts = gtk4::ShortcutController::new();
        toggle_shortcuts.set_propagation_phase(gtk4::PropagationPhase::Capture);
        window.add_controller(toggle_shortcuts.clone());

        let window_for_css = window.clone();
        let ui_state = hashterm_core::state::UiState::load();
        let this = Rc::new(Self {
            window,
            grid,
            overlay,
            stack,
            tabbar,
            groupbar,
            hover_strip,
            slide,
            backend: Rc::new(backend),
            tabs: RefCell::new(Vec::new()),
            ctl,
            config,
            toggle_shortcuts,
            current_shortcuts: RefCell::new(Vec::new()),
            last_toggle: std::cell::Cell::new(None),
            held_focus: std::cell::Cell::new(false),
            modal_open: std::cell::Cell::new(false),
            capture_suppressed: std::cell::Cell::new(false),
            shown_at: std::cell::Cell::new(None),
            last_capture_attempt: std::cell::Cell::new(None),
            suppress_keyvals: RefCell::new(Vec::new()),
            tabbar_pinned: std::cell::Cell::new(false),
            hiding: std::cell::Cell::new(false),
            ui_dirty: std::cell::Cell::new(false),
            geometry_settled: std::cell::Cell::new(None),
            geometry_pending: std::cell::Cell::new(None),
            opacity_override: std::cell::Cell::new(ui_state.window_opacity),
            opacity_save_timer: std::cell::Cell::new(None),
            osd_revealer,
            notice,
            title_poll_busy: std::cell::Cell::new(false),
            search: RefCell::new(None),
            search_gen: Default::default(),
            search_target: RefCell::new(None),
            font_scale: std::cell::Cell::new(1.0),
            last_layout_change: std::cell::Cell::new(std::time::Instant::now()),
            activity_gates: RefCell::new(Default::default()),
            notify_limiter: RefCell::new(Default::default()),
            restyle: RefCell::new(None),
            hotkey_status: RefCell::new(hashterm_platform::HotkeyStatus::Pending),
            osd_content,
            osd_timeout: std::cell::Cell::new(None),
            accent_css: {
                let provider = gtk4::CssProvider::new();
                gtk4::style_context_add_provider_for_display(
                    &WidgetExt::display(&window_for_css),
                    &provider,
                    gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
                );
                provider
            },
            active_group: RefCell::new(
                ui_state
                    .active_group
                    .unwrap_or_else(|| hashterm_tmux::controller::DEFAULT_GROUP.into()),
            ),
            group_order: RefCell::new(ui_state.group_order),
            last_active: RefCell::new(ui_state.last_active_tab),
        });

        // Any key RELEASE ends auto-repeat suppression: the summoning key has
        // left the keyboard, so the next press is a genuine new toggle.
        // While suppression is active, bare presses of the toggle keyval are
        // SWALLOWED: on XWayland the compositor may eat the chord's release,
        // leaving a modifier-less auto-repeat storm of the key that would
        // otherwise type into the shell until state resyncs.
        {
            let keys = gtk4::EventControllerKey::new();
            keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
            {
                let weak = Rc::downgrade(&this);
                keys.connect_key_pressed(move |_, keyval, _, state| {
                    let Some(win) = weak.upgrade() else {
                        return gtk4::glib::Propagation::Proceed;
                    };
                    if !win.capture_suppressed.get() {
                        return gtk4::glib::Propagation::Proceed;
                    }
                    tracing::debug!(
                        "key press during suppression: {:?} mods={state:?}",
                        keyval.name()
                    );
                    if win.suppress_keyvals.borrow().contains(&keyval) {
                        // Storm-cadence check: repeats tick every few dozen
                        // ms. A press after >1.2s of quiet is the user
                        // genuinely typing this key — let it through.
                        let now = std::time::Instant::now();
                        let anchor = win
                            .last_capture_attempt
                            .get()
                            .or(win.shown_at.get())
                            .unwrap_or(now);
                        if now.duration_since(anchor) > std::time::Duration::from_millis(1200) {
                            win.capture_suppressed.set(false);
                            return gtk4::glib::Propagation::Proceed;
                        }
                        tracing::debug!("swallowed stuck repeat of toggle key");
                        win.last_capture_attempt.set(Some(now));
                        return gtk4::glib::Propagation::Stop;
                    }
                    gtk4::glib::Propagation::Proceed
                });
            }
            {
                // Only a release of the TOGGLE key disarms suppression: the
                // compositor may deliver the modifier releases (Ctrl, Shift)
                // while still eating the toggle key's own release — clearing
                // on any release would disarm right before the stuck-repeat
                // storm starts.
                let weak = Rc::downgrade(&this);
                keys.connect_key_released(move |_, keyval, _, _| {
                    if let Some(win) = weak.upgrade()
                        && win.capture_suppressed.get()
                        && win.suppress_keyvals.borrow().contains(&keyval)
                    {
                        tracing::debug!("toggle key released; suppression off");
                        win.capture_suppressed.set(false);
                    }
                });
            }
            this.window.add_controller(keys);
        }

        this.window.set_opacity(this.effective_opacity());
        this.wire_opacity_scroll();
        this.place_tabbar();
        this.place_groupbar();
        this.apply_window_classes();
        this.wire_tabbar();
        this.wire_groupbar();
        this.wire_autohide();
        this.wire_focus_loss();
        this.wire_quit_autosave();
        this.wire_signal_snapshot();
        this.wire_geometry_memory();
        this.wire_title_poll();
        this.wire_system_scheme();
        this.wire_search();
        {
            // Layout changes make tmux repaint; seen-ness changes clear badges.
            let weak = Rc::downgrade(&this);
            this.window.connect_default_width_notify({
                let weak = weak.clone();
                move |_| {
                    if let Some(w) = weak.upgrade() {
                        w.last_layout_change.set(std::time::Instant::now());
                    }
                }
            });
            this.window.connect_default_height_notify({
                let weak = weak.clone();
                move |_| {
                    if let Some(w) = weak.upgrade() {
                        w.last_layout_change.set(std::time::Instant::now());
                    }
                }
            });
            this.window.connect_map({
                let weak = weak.clone();
                move |_| {
                    if let Some(w) = weak.upgrade() {
                        w.last_layout_change.set(std::time::Instant::now());
                        w.mark_active_seen();
                    }
                }
            });
            this.window.connect_is_active_notify(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.mark_active_seen();
                }
            });
        }
        this.schedule_first_run_hint();
        this.wire_redraw_on_resize();
        this.refresh_toggle_shortcut();
        this.adopt_or_create();

        // Pin above other windows from the very first map (where the platform
        // allows), not only after the first dropdown toggle.
        if this.config.borrow().dropdown.enabled {
            let backend = this.backend.clone();
            this.window.connect_map(move |w| {
                if let Ok(app_win) = w.clone().downcast::<gtk4::ApplicationWindow>() {
                    backend.keep_above(&app_win);
                }
            });
        }
        // Refreshes that arrived while unmapped were deferred (rebuilds on an
        // unmapped window leak); play the one pending rebuild now.
        {
            let this = Rc::downgrade(&this);
            this.upgrade()
                .expect("window alive while connecting map")
                .window
                .connect_map(move |_| {
                    if let Some(w) = this.upgrade()
                        && w.ui_dirty.take()
                    {
                        w.refresh_tabbar();
                    }
                });
        }
        this
    }

    /// (Re-)install the capture-phase in-window shortcuts from config: the
    /// dropdown toggle key plus <mod>+1..0 direct tab jumps. Capture phase is
    /// required — VTE consumes these keys otherwise (Alt+digit becomes an
    /// ESC-prefixed byte to the shell).
    fn refresh_toggle_shortcut(self: &Rc<Self>) {
        for old in self.current_shortcuts.borrow_mut().drain(..) {
            self.toggle_shortcuts.remove_shortcut(&old);
        }
        let mut installed = Vec::new();

        let accel =
            hashterm_core::config::normalize_accel(&self.config.borrow().hotkeys.toggle_dropdown);

        // Keyvals to swallow while summon-suppression is active: the toggle
        // key itself plus its US-layout shift sibling (a stuck XWayland
        // repeat can arrive with or without the modifiers).
        {
            let mut set = Vec::new();
            if let Some((key, _mods)) = gtk4::accelerator_parse(&accel) {
                set.push(key);
                use gtk4::gdk::Key;
                let sibling = match key {
                    Key::grave => Some(Key::asciitilde),
                    Key::asciitilde => Some(Key::grave),
                    Key::_1 => Some(Key::exclam),
                    Key::_2 => Some(Key::at),
                    Key::minus => Some(Key::underscore),
                    Key::equal => Some(Key::plus),
                    Key::semicolon => Some(Key::colon),
                    Key::apostrophe => Some(Key::quotedbl),
                    Key::comma => Some(Key::less),
                    Key::period => Some(Key::greater),
                    Key::slash => Some(Key::question),
                    Key::backslash => Some(Key::bar),
                    _ => None,
                };
                set.extend(sibling);
            }
            *self.suppress_keyvals.borrow_mut() = set;
        }

        if let Some(trigger) = gtk4::ShortcutTrigger::parse_string(&accel) {
            let this = Rc::downgrade(self);
            let action = gtk4::CallbackAction::new(move |_, _| {
                if let Some(win) = this.upgrade() {
                    win.toggle_dropdown_from("capture-key");
                    gtk4::glib::Propagation::Stop
                } else {
                    gtk4::glib::Propagation::Proceed
                }
            });
            let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
            self.toggle_shortcuts.add_shortcut(shortcut.clone());
            installed.push(shortcut);
        } else {
            tracing::warn!("unparseable toggle accel '{accel}'");
        }

        let modifier = self.config.borrow().hotkeys.goto_tab_modifier.clone();
        if !modifier.is_empty() {
            for n in 1..=10u32 {
                let accel = format!("{modifier}{}", n % 10);
                let Some(trigger) = gtk4::ShortcutTrigger::parse_string(&accel) else {
                    tracing::warn!("unparseable goto-tab accel '{accel}'");
                    break;
                };
                let this = Rc::downgrade(self);
                let action = gtk4::CallbackAction::new(move |_, _| {
                    if let Some(win) = this.upgrade() {
                        win.goto_tab(n as usize);
                        gtk4::glib::Propagation::Stop
                    } else {
                        gtk4::glib::Propagation::Proceed
                    }
                });
                let shortcut = gtk4::Shortcut::new(Some(trigger), Some(action));
                self.toggle_shortcuts.add_shortcut(shortcut.clone());
                installed.push(shortcut);
            }
        }
        *self.current_shortcuts.borrow_mut() = installed;
    }

    /// Jump straight to visible-group tab `n` (1-based; out-of-range no-op).
    pub fn goto_tab(self: &Rc<Self>, n: usize) {
        let session = self.visible_sessions().get(n.wrapping_sub(1)).cloned();
        if let Some(session) = session {
            self.select_tab(&session);
            self.show_tab_osd();
        }
    }

    /// Synchronous autosave on window close: dumps are fast (tens of ms) and
    /// this is the last chance before the process exits. Sessions themselves
    /// keep living on the tmux server regardless.
    /// Synchronous full snapshot of the server into a rotated autosave. Called
    /// on window close AND on SIGTERM/SIGHUP (graceful reboot/logout) so a
    /// restart loses at most the work since this returns, not up to a whole
    /// autosave interval.
    pub fn snapshot_now(&self) {
        let cfg = self.config.borrow();
        if cfg.session.autosave_interval_secs == 0 {
            return;
        }
        let store =
            hashterm_session::SessionStore::new(hashterm_session::SessionStore::default_root());
        let opts = hashterm_session::DumpOptions {
            exclude_panes: cfg.restore.exclude_panes.clone(),
            max_scrollback_lines: cfg.restore.max_scrollback_lines,
        };
        let stamp = hashterm_session::dump::now_stamp();
        match hashterm_session::dump_server(&self.ctl, &store, &stamp, true, &opts) {
            Ok(_) => {
                let _ = store.prune_autosaves(cfg.session.autosave_keep as usize);
            }
            Err(hashterm_session::dump::DumpError::Empty) => {}
            Err(e) => tracing::warn!("snapshot failed: {e}"),
        }
    }

    fn wire_quit_autosave(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        self.window.connect_close_request(move |_| {
            if let Some(win) = this.upgrade() {
                // Remember the geometry for next start.
                win.flush_geometry();
                win.snapshot_now();
            }
            // Clean exit: clear the crash marker so next start doesn't offer
            // to restore.
            hashterm_core::state::clear_session_lock();
            gtk4::glib::Propagation::Proceed
        });
    }

    /// Snapshot then quit on SIGTERM/SIGHUP — a graceful reboot/logout SIGTERMs
    /// us, and without this the periodic autosave is the only (stale) safety
    /// net. The signal handler itself only sets an atomic (async-signal-safe);
    /// a glib timeout does the actual dump on the main loop.
    fn wire_signal_snapshot(self: &Rc<Self>) {
        // SAFETY: installing a signal handler that only does an atomic store.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handle_shutdown_signal as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
            libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
        }
        let this = Rc::downgrade(self);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
            if SHUTDOWN_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if let Some(win) = this.upgrade() {
                    tracing::info!("shutdown signal: snapshotting before exit");
                    win.flush_geometry();
                    win.snapshot_now();
                    // Deliberately DO NOT clear the unclean marker: a reboot or
                    // logout is not a user quit, so the next start should offer
                    // to restore. Exit directly rather than via the window's
                    // close_request (which clears the marker).
                }
                std::process::exit(0);
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // ---- tab operations -------------------------------------------------

    pub fn new_tab(self: &Rc<Self>) {
        self.new_tab_with(None, None);
    }

    /// `cwd`/`command` override config defaults (used by `hashterm --new-tab`).
    pub fn new_tab_with(
        self: &Rc<Self>,
        cwd: Option<std::path::PathBuf>,
        command: Option<Vec<String>>,
    ) {
        let cwd = cwd.or_else(|| {
            if self.config.borrow().general.inherit_cwd {
                self.active_entry_cwd()
            } else {
                None
            }
        });
        // New tabs inherit the active group (None for the implicit default).
        let group = self.active_group.borrow().clone();
        self.spawn_tab(&group, cwd, command, None);
    }

    /// Create a tab in `group` and focus it. `color` re-applies a group color
    /// (used when a group's last tab was closed and the user keeps the group).
    fn spawn_tab(
        self: &Rc<Self>,
        group: &str,
        cwd: Option<std::path::PathBuf>,
        command: Option<Vec<String>>,
        color: Option<&str>,
    ) {
        let cfg = self.config.borrow();
        let group = (group != hashterm_tmux::controller::DEFAULT_GROUP).then(|| group.to_owned());
        let opts = NewSessionOpts {
            title: "shell".into(),
            cwd,
            command: command.or_else(|| cfg.general.default_command.clone()),
            group,
        };
        drop(cfg);
        match self.ctl.new_session(&opts) {
            Ok(id) => {
                if let Some(color) = color {
                    let _ = self.ctl.set_group_color(&id, Some(color));
                }
                self.sync_from_server();
                // Focus the freshly created tab.
                if let Ok(sessions) = self.ctl.list_sessions()
                    && let Some(info) = sessions.iter().find(|s| s.id == id)
                {
                    self.select_tab(&info.name.clone());
                }
            }
            Err(e) => tracing::error!("new tab failed: {e}"),
        }
    }

    /// Quake-style toggle: slide the content in/out and let the overlay
    /// backend own placement/stacking. Falls back to plain show/hide when
    /// dropdown mode is disabled.
    pub fn toggle_dropdown(self: &Rc<Self>) {
        self.toggle_dropdown_from("unspecified");
    }

    pub fn toggle_dropdown_from(self: &Rc<Self>, source: &str) {
        // One physical keypress can arrive twice (portal activation + the
        // in-window capture grab): collapse triggers closer than 200ms.
        // Auto-repeat guard: while the summoning key is still held, its
        // repeated presses reach the focused window as capture-key toggles.
        // A press after >1.2s of quiet cannot be part of a repeat train
        // (repeats arrive every few dozen ms): treat it as a genuine press.
        if source == "capture-key" && self.capture_suppressed.get() {
            let now = std::time::Instant::now();
            let anchor = self
                .last_capture_attempt
                .get()
                .or(self.shown_at.get())
                .unwrap_or(now);
            if now.duration_since(anchor) > std::time::Duration::from_millis(1200) {
                self.capture_suppressed.set(false);
            } else {
                self.last_capture_attempt.set(Some(now));
                tracing::debug!("toggle[{source}]: suppressed (toggle key still held)");
                return;
            }
        }
        if self.hiding.get() {
            tracing::debug!("toggle[{source}]: absorbed (hide animation running)");
            return;
        }
        let now = std::time::Instant::now();
        if let Some(last) = self.last_toggle.get()
            && now.duration_since(last) < std::time::Duration::from_millis(200)
        {
            tracing::debug!("toggle[{source}]: debounced");
            return;
        }
        self.last_toggle.set(Some(now));
        tracing::debug!(
            "toggle[{source}]: visible={} -> {}",
            self.window.is_visible(),
            !self.window.is_visible()
        );

        let dd = self.config.borrow().dropdown.clone();
        if !dd.enabled {
            if self.window.is_visible() {
                self.window.set_visible(false);
            } else {
                self.present();
            }
            return;
        }
        if self.window.is_visible() {
            // `focus-loss` IS the unfocused case: it must hide, never refocus.
            if !self.window.is_active() && source != "focus-loss" {
                // Visible but unfocused: the hotkey means "bring it to me",
                // not "hide it" — focus the console instead.
                tracing::debug!("toggle[{source}]: focusing visible unfocused window");
                self.backend.activate(&self.window);
                if let Some(name) = self.stack.visible_child_name()
                    && let Some(t) = self
                        .tabs
                        .borrow()
                        .iter()
                        .find(|t| t.session == name.as_str())
                {
                    t.page.grab_focus();
                }
                return;
            }
            self.hide_dropdown(&dd);
        } else {
            self.show_dropdown(&dd);
        }
    }

    /// The opacity actually applied: the live scroll override, else config.
    fn effective_opacity(&self) -> f64 {
        self.opacity_override
            .get()
            .unwrap_or(self.config.borrow().appearance.window_opacity)
            .clamp(0.1, 1.0)
    }

    /// appearance.window_opacity was edited in the config: the file wins,
    /// the remembered scroll value is dropped (same rule as dropdown height).
    pub fn clear_opacity_override(self: &Rc<Self>) {
        self.opacity_override.set(None);
        let mut st = hashterm_core::state::UiState::load();
        st.window_opacity = None;
        st.save();
    }

    /// Ctrl+Alt+Shift + scroll adjusts whole-window opacity live (1% per
    /// notch, 20%-100%). Capture phase: VTE must not scroll its buffer while
    /// the chord is held.
    fn wire_opacity_scroll(self: &Rc<Self>) {
        use gtk4::gdk::ModifierType;
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let this = Rc::downgrade(self);
        scroll.connect_scroll(move |ctl, _dx, dy| {
            let Some(win) = this.upgrade() else {
                return gtk4::glib::Propagation::Proceed;
            };
            let chord =
                ModifierType::CONTROL_MASK | ModifierType::ALT_MASK | ModifierType::SHIFT_MASK;
            if !ctl.current_event_state().contains(chord) {
                return gtk4::glib::Propagation::Proceed;
            }
            // Scroll up (dy < 0) = more opaque. Trackpads deliver fractional
            // deltas; wheels ±1 per notch — 1% either way for fine control.
            let next = ((win.effective_opacity() - dy * 0.01) * 100.0).round() / 100.0;
            let next = next.clamp(0.2, 1.0);
            win.opacity_override.set(Some(next));
            win.window.set_opacity(next);
            win.show_osd_message(&format!("Opacity {:.0}%", next * 100.0), 700);

            // Persist AFTER the storm: one state.json write per gesture.
            if let Some(id) = win.opacity_save_timer.take() {
                id.remove();
            }
            let weak = Rc::downgrade(&win);
            let id = gtk4::glib::timeout_add_local_once(
                std::time::Duration::from_millis(400),
                move || {
                    if let Some(win) = weak.upgrade() {
                        win.opacity_save_timer.set(None);
                        let mut st = hashterm_core::state::UiState::load();
                        st.window_opacity = win.opacity_override.get();
                        st.save();
                    }
                },
            );
            win.opacity_save_timer.set(Some(id));
            gtk4::glib::Propagation::Stop
        });
        self.window.add_controller(scroll);
    }

    /// What the user has chosen to be remembered, as it is right now. None
    /// while the window can't be measured meaningfully (hidden, maximized,
    /// fullscreen) or when nothing is enabled. Position comes from the
    /// platform backend and is absent where the platform can't report it.
    fn current_geometry(&self) -> Option<WindowGeometry> {
        let cfg = self.config.borrow().window.clone();
        if !self.window.is_mapped() || self.window.is_maximized() || self.window.is_fullscreen() {
            return None;
        }
        let mut g = WindowGeometry::default();
        if cfg.remember_size {
            let (w, h) = (self.window.width(), self.window.height());
            if w > 0 && h > 0 {
                g.width = Some(w);
                g.height = Some(h);
            }
        }
        if cfg.remember_position
            && let Some((x, y)) = self.backend.position(&self.window)
        {
            g.x = Some(x);
            g.y = Some(y);
        }
        (g != WindowGeometry::default()).then_some(g)
    }

    /// The remembered geometry filtered through the current config: parts the
    /// user turned off are dropped, and an explicit `dropdown.height` still
    /// beats a remembered height. Falls back to the pre-geometry height key.
    fn saved_geometry(&self) -> Option<WindowGeometry> {
        let cfg = self.config.borrow();
        let st = hashterm_core::state::UiState::load();
        let mut g = st.window_geometry.unwrap_or_default();
        if !cfg.window.remember_size {
            g.width = None;
            g.height = None;
        } else if g.height.is_none() {
            g.height = st.window_height_px;
        }
        if !cfg.window.remember_position {
            g.x = None;
            g.y = None;
        }
        if cfg.dropdown.height.is_some() {
            g.height = None;
        }
        (g != WindowGeometry::default()).then_some(g)
    }

    /// Hand the remembered geometry to the overlay backend ahead of a
    /// `place_and_show`.
    fn sync_saved_geometry(&self) {
        self.backend
            .set_saved_geometry(if self.config.borrow().dropdown.enabled {
                self.saved_geometry()
            } else {
                None
            });
    }

    fn store_geometry(&self, g: WindowGeometry) {
        let mut st = hashterm_core::state::UiState::load();
        let mut merged = st.window_geometry.unwrap_or_default();
        merged.x = g.x.or(merged.x);
        merged.y = g.y.or(merged.y);
        merged.width = g.width.or(merged.width);
        merged.height = g.height.or(merged.height);
        st.window_geometry = Some(merged);
        st.save();
    }

    /// Persist the geometry if it changed since the window was shown. Called
    /// right before the window goes away (hide / close / shutdown), when the
    /// poller may not have seen the last change yet.
    fn flush_geometry(&self) {
        if let (Some(settled), Some(cur)) = (self.geometry_settled.get(), self.current_geometry())
            && settled != cur
        {
            self.store_geometry(cur);
        }
        self.geometry_settled.set(None);
        self.geometry_pending.set(None);
    }

    /// Remember size/position when the user moves or resizes the window.
    /// GTK has no move notification and Wayland has no position at all, so a
    /// cheap 700 ms sampler watches the mapped window; a change is saved once
    /// it holds still for two samples. The first settled sample after a map is
    /// only the baseline (placement by the app itself is never "the user's").
    fn wire_geometry_memory(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        self.window.connect_map(move |_| {
            if let Some(w) = this.upgrade() {
                w.geometry_settled.set(None);
                w.geometry_pending.set(None);
            }
        });
        let this = Rc::downgrade(self);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(700), move || {
            let Some(win) = this.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            if win.hiding.get() {
                return gtk4::glib::ControlFlow::Continue;
            }
            let Some(cur) = win.current_geometry() else {
                win.geometry_pending.set(None);
                return gtk4::glib::ControlFlow::Continue;
            };
            if win.geometry_pending.get() == Some(cur) {
                match win.geometry_settled.get() {
                    Some(prev) if prev != cur => win.store_geometry(cur),
                    _ => {}
                }
                win.geometry_settled.set(Some(cur));
            }
            win.geometry_pending.set(Some(cur));
            gtk4::glib::ControlFlow::Continue
        });
    }

    /// Redraw the terminal once a resize settles. VTE tells tmux the new size
    /// (SIGWINCH) as the window changes, but a drag fires many intermediate
    /// sizes and the final repaint can be missed or leave stale cells. When
    /// the surface stops changing for 150 ms: repaint the visible terminal and
    /// ask its tmux client to redraw at the final size.
    fn wire_redraw_on_resize(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        let timer: Rc<std::cell::Cell<Option<gtk4::glib::SourceId>>> =
            Rc::new(std::cell::Cell::new(None));
        self.window.connect_realize(move |w| {
            let Some(surface) = w.surface() else { return };
            let this = this.clone();
            let timer = timer.clone();
            // Only a real size change counts: layout also fires for repaints
            // we cause ourselves, which must not re-arm the redraw.
            let last = std::cell::Cell::new((0, 0));
            surface.connect_layout(move |_, w, h| {
                if last.replace((w, h)) == (w, h) {
                    return;
                }
                if let Some(id) = timer.take() {
                    id.remove();
                }
                let this = this.clone();
                let timer2 = timer.clone();
                let id = gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(150),
                    move || {
                        timer2.set(None);
                        if let Some(win) = this.upgrade() {
                            win.redraw_visible_terminal();
                        }
                    },
                );
                timer.set(Some(id));
            });
        });
    }

    fn redraw_visible_terminal(&self) {
        let Some(name) = self.stack.visible_child_name() else {
            return;
        };
        let name = name.to_string();
        if let Some(t) = self.tabs.borrow().iter().find(|t| t.session == name) {
            t.page.queue_redraw();
        }
        // tmux: refresh every client attached to this session, off the UI
        // thread (it shells out to tmux).
        let ctl = (*self.ctl).clone();
        std::thread::spawn(move || {
            let Ok(clients) = ctl.run(&["list-clients", "-t", &name, "-F", "#{client_name}"])
            else {
                return;
            };
            for client in clients.lines().filter(|c| !c.is_empty()) {
                let ok = ctl.run(&["refresh-client", "-t", client]).is_ok();
                tracing::debug!("resize settled: refresh-client {client} on {name}: ok={ok}");
            }
        });
    }

    fn hide_dropdown(self: &Rc<Self>, dd: &hashterm_core::config::Dropdown) {
        self.flush_geometry();
        self.held_focus.set(false);
        self.hiding.set(true);
        let backend = self.backend.clone();
        let win = self.window.clone();
        let this = Rc::downgrade(self);
        self.slide.animate(
            false,
            dd.animation_ms,
            Some(Box::new(move || {
                backend.hide(&win);
                if let Some(w) = this.upgrade() {
                    w.hiding.set(false);
                }
            })),
        );
    }

    /// Slide the console in at its configured edge/size (Q3-style).
    pub fn show_dropdown(self: &Rc<Self>, dd: &hashterm_core::config::Dropdown) {
        // The backend restores the remembered size/position (unless config
        // pins an explicit height).
        self.sync_saved_geometry();
        self.held_focus.set(false);
        // Assume the summoning key is still held; a key release or a
        // >1.2s quiet gap (see the guard above) lifts this.
        self.capture_suppressed.set(true);
        self.shown_at.set(Some(std::time::Instant::now()));
        self.last_capture_attempt.set(None);
        self.slide.set_offset(1.0);
        self.slide.set_edge(dd.edge);
        self.backend.place_and_show(&self.window, dd);
        self.slide.animate(true, dd.animation_ms, None);
        if let Some(name) = self.stack.visible_child_name()
            && let Some(t) = self
                .tabs
                .borrow()
                .iter()
                .find(|t| t.session == name.as_str())
        {
            t.page.grab_focus();
        }
    }

    /// First presentation: in console mode the window IS the drop-down — it
    /// appears pinned to its edge at configured size with the slide, never as
    /// a floating default-size window.
    pub fn initial_present(self: &Rc<Self>) {
        let dd = self.config.borrow().dropdown.clone();
        if dd.enabled {
            self.show_dropdown(&dd);
        } else {
            self.window.present();
            // Plain-window mode: put it back where it was left (no-op where
            // the platform can't place windows or the spot is off-screen).
            if self.config.borrow().window.remember_position
                && let Some(g) = self.saved_geometry()
                && let (Some(x), Some(y)) = (g.x, g.y)
            {
                self.backend.move_to(&self.window, x, y);
            }
        }
    }

    /// Hide when focus leaves the window (config-gated), with a short grace
    /// period so popovers and fast refocus don't flap. Only a genuine
    /// active -> inactive transition counts: a window shown without focus
    /// (compositor focus-stealing prevention) must not instantly hide.
    fn wire_focus_loss(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |w| {
            let Some(win) = this.upgrade() else { return };
            tracing::debug!(
                "focus: active={} visible={} held_focus={}",
                w.is_active(),
                w.is_visible(),
                win.held_focus.get()
            );
            if w.is_active() {
                win.held_focus.set(true);
                return;
            }
            let dd = win.config.borrow().dropdown.clone();
            if !dd.enabled
                || !dd.hide_on_focus_loss
                || !w.is_visible()
                || !win.held_focus.get()
                || win.own_dialog_open()
            {
                return;
            }
            let this2 = Rc::downgrade(&win);
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                let Some(win) = this2.upgrade() else { return };
                if !win.window.is_active()
                    && win.window.is_visible()
                    && win.held_focus.get()
                    && !win.own_dialog_open()
                {
                    win.toggle_dropdown_from("focus-loss");
                }
            });
        });
    }

    /// One of this app's own dialogs (Settings, a confirmation, Properties…)
    /// is on screen. Focus moving to it is not "the user left", so the console
    /// must not hide from behind it.
    fn own_dialog_open(&self) -> bool {
        let main = self.window.upcast_ref::<gtk4::Window>();
        self.modal_open.get()
            || gtk4::Window::list_toplevels()
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk4::Window>())
                .any(|w| w != main && w.is_visible())
    }

    pub fn close_tab(self: &Rc<Self>, session: &str) {
        self.notify_limiter.borrow_mut().forget(session);
        self.activity_gates.borrow_mut().remove(session);
        let kill = self.config.borrow().general.close_kills_session;
        if kill && let Err(e) = self.ctl.run(&["kill-session", "-t", session]) {
            tracing::error!("kill-session {session}: {e}");
        }
        let removal = self.removal_info(session);
        self.tabs.borrow_mut().retain(|t| t.session != session);
        if let Some(child) = self.stack.child_by_name(session) {
            self.stack.remove(&child);
        }
        match removal {
            Some(r) => self.after_tab_removed(r),
            None => {
                self.validate_active_group();
                self.refresh_tabbar();
            }
        }
    }

    /// What the window needs to remember about a tab that is about to go
    /// away, so selection can follow it afterwards (see `after_tab_removed`).
    fn removal_info(self: &Rc<Self>, session: &str) -> Option<Removal> {
        let (group, index, color) = {
            let tabs = self.tabs.borrow();
            let t = tabs.iter().find(|t| t.session == session)?;
            let group = t.group_name().to_owned();
            let members: Vec<&TabEntry> = tabs.iter().filter(|x| x.group_name() == group).collect();
            let index = members.iter().position(|x| x.session == session)?;
            let color = members.iter().find_map(|x| x.group_color.clone());
            (group, index, color)
        };
        let was_active = self
            .stack
            .visible_child_name()
            .is_some_and(|n| n.as_str() == session);
        // Neighbouring group (previous, else next) to land on if this group
        // disappears with its last tab.
        let names: Vec<String> = self.groups().into_iter().map(|g| g.name).collect();
        let group_index = names.iter().position(|n| *n == group).unwrap_or(0);
        let neighbor = names.iter().position(|n| *n == group).and_then(|i| {
            i.checked_sub(1)
                .and_then(|p| names.get(p))
                .or_else(|| names.get(i + 1))
                .cloned()
        });
        Some(Removal {
            group,
            index,
            was_active,
            color,
            neighbor,
            group_index,
        })
    }

    /// Selection policy after a tab was closed (any way: Ctrl+W, the x button,
    /// the shell exiting):
    /// * a background tab closing changes nothing;
    /// * otherwise go to the PREVIOUS tab of the group, or — if the first tab
    ///   was closed — the NEXT one;
    /// * if the group has no tabs left, ask whether to close the group or to
    ///   keep it with a fresh tab in the home directory.
    fn after_tab_removed(self: &Rc<Self>, r: Removal) {
        if !r.was_active {
            self.validate_active_group();
            self.refresh_tabbar();
            return;
        }
        let remaining: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == r.group)
            .map(|t| t.session.clone())
            .collect();
        if !remaining.is_empty() {
            let pick = if r.index > 0 {
                remaining[(r.index - 1).min(remaining.len() - 1)].clone()
            } else {
                remaining[0].clone()
            };
            self.select_tab(&pick);
            self.show_tab_osd();
            return;
        }
        // The group is empty. Show the neighbouring group behind the dialog
        // (never a stale/blank page), then ask.
        match r.neighbor.clone() {
            Some(n) if self.tabs.borrow().iter().any(|t| t.group_name() == n) => {
                self.switch_group(&n);
            }
            _ => self.validate_active_group(),
        }
        self.refresh_tabbar();
        self.ask_empty_group(r.group, r.color, r.group_index);
    }

    /// "No more tabs left" prompt. Yes: the group goes away (and the window
    /// closes if it was the last one). No: keep the group with a new tab in
    /// the home directory.
    fn ask_empty_group(self: &Rc<Self>, group: String, color: Option<String>, position: usize) {
        if self.modal_open.get() {
            return;
        }
        let this = Rc::downgrade(self);
        let detail = format!(
            "Group \u{201c}{group}\u{201d} is empty. \u{201c}Keep group\u{201d} opens a new tab \
             in your home directory."
        );
        self.confirm(
            "No more tabs left, do you want to close the group?",
            &detail,
            "Close group",
            "Keep group",
            false,
            move |close| {
                let Some(win) = this.upgrade() else { return };
                if close {
                    if win.tabs.borrow().is_empty() {
                        win.window.close();
                    }
                } else {
                    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                    win.spawn_tab(&group, home, None, color.as_deref());
                    // The group vanished from the bar while empty; put it
                    // back where it was.
                    win.reorder_group(&group, position);
                }
            },
        );
    }

    /// Modal yes/no. `on_answer(true)` = the affirmative button. Closing the
    /// dialog any other way counts as the negative answer.
    fn confirm(
        self: &Rc<Self>,
        title: &str,
        detail: &str,
        yes: &str,
        no: &str,
        yes_is_default: bool,
        on_answer: impl FnOnce(bool) + 'static,
    ) {
        self.modal_open.set(true);
        let dialog = gtk4::AlertDialog::builder()
            .message(title)
            .detail(detail)
            .buttons([no, yes])
            .cancel_button(0)
            .default_button(if yes_is_default { 1 } else { 0 })
            .modal(true)
            .build();
        let this = Rc::downgrade(self);
        dialog.choose(
            Some(&self.window),
            None::<&gtk4::gio::Cancellable>,
            move |res| {
                if let Some(win) = this.upgrade() {
                    win.modal_open.set(false);
                }
                on_answer(matches!(res, Ok(1)));
                // GTK hands focus back to whatever held it before the dialog,
                // which may be a page that is no longer shown: put the
                // keyboard on the terminal the user is looking at.
                let this = this.clone();
                gtk4::glib::idle_add_local_once(move || {
                    if let Some(win) = this.upgrade() {
                        win.with_active_page(|p| p.grab_focus());
                    }
                });
            },
        );
    }

    /// Close a group with every tab in it — no undo (the sessions are killed
    /// on the tmux server, not detached). Asks first.
    pub fn close_group(self: &Rc<Self>, name: &str) {
        let count = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == name)
            .count();
        if count == 0 {
            return;
        }
        let this = Rc::downgrade(self);
        let group = name.to_owned();
        let tabs_word = if count == 1 { "tab" } else { "tabs" };
        self.confirm(
            &format!("Close group \u{201c}{name}\u{201d}?"),
            &format!(
                "This closes all {count} {tabs_word} in the group and ends the programs \
                 running in them. It cannot be undone."
            ),
            &format!("Close {count} {tabs_word}"),
            "Cancel",
            false,
            move |yes| {
                if yes && let Some(win) = this.upgrade() {
                    win.close_group_now(&group);
                }
            },
        );
    }

    fn close_group_now(self: &Rc<Self>, name: &str) {
        let sessions: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == name)
            .map(|t| t.session.clone())
            .collect();
        if sessions.is_empty() {
            return;
        }
        let was_active = *self.active_group.borrow() == name;
        let names: Vec<String> = self.groups().into_iter().map(|g| g.name).collect();
        let neighbor = names.iter().position(|n| n == name).and_then(|i| {
            i.checked_sub(1)
                .and_then(|p| names.get(p))
                .or_else(|| names.get(i + 1))
                .cloned()
        });
        for session in &sessions {
            if let Err(e) = self.ctl.run(&["kill-session", "-t", session]) {
                tracing::error!("kill-session {session}: {e}");
            }
        }
        self.tabs
            .borrow_mut()
            .retain(|t| !sessions.contains(&t.session));
        for session in &sessions {
            if let Some(child) = self.stack.child_by_name(session) {
                self.stack.remove(&child);
            }
        }
        self.last_active.borrow_mut().remove(name);
        if self.tabs.borrow().is_empty() {
            self.window.close();
            return;
        }
        if was_active && let Some(n) = neighbor {
            self.switch_group(&n);
        }
        self.validate_active_group();
        self.refresh_tabbar();
    }

    /// Ctrl+Shift+W: closes the active PANE while the tab has several, then
    /// the tab itself exactly as before.
    pub fn close_active_tab(self: &Rc<Self>) {
        let Some(name) = self.stack.visible_child_name() else {
            return;
        };
        let id = hashterm_tmux::TmuxSessionId(name.to_string());
        match self.ctl.pane_count(&id) {
            Ok(n) if n > 1 => self.pane_op(move |ctl| ctl.kill_active_pane(&id).map(|_| None)),
            Ok(_) => self.close_tab(name.as_str()),
            // Never guess "one pane" and close a tab that may hold several.
            Err(e) => {
                tracing::warn!("pane count failed: {e}");
                self.show_osd_message("Couldn't read the pane count; nothing closed", 2500);
            }
        }
    }

    /// The tab's "x": always the whole tab, but asks first when it holds
    /// several panes (closing it would end all of them).
    pub fn request_close_tab(self: &Rc<Self>, session: &str) {
        let id = hashterm_tmux::TmuxSessionId(session.to_string());
        let panes = match self.ctl.pane_count(&id) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("pane count failed: {e}");
                self.show_osd_message("Couldn't read the pane count; nothing closed", 2500);
                return;
            }
        };
        if panes <= 1 {
            self.close_tab(session);
            return;
        }
        let session = session.to_string();
        self.confirm(
            &format!("Close this tab and its {panes} panes?"),
            "Everything running in them will end.",
            "Close tab",
            "Cancel",
            false,
            {
                let this = Rc::downgrade(self);
                move |yes| {
                    if let (true, Some(win)) = (yes, this.upgrade()) {
                        win.close_tab(&session);
                    }
                }
            },
        );
    }

    // ---- panes (tmux draws them) -------------------------------------------

    /// Run a pane operation off the UI thread. `Ok(Some(msg))` is shown as an
    /// OSD (e.g. zoom state); an error shows tmux's own message.
    fn pane_op(
        self: &Rc<Self>,
        op: impl FnOnce(
            &hashterm_tmux::TmuxController,
        ) -> Result<Option<String>, hashterm_tmux::TmuxError>
        + Send
        + 'static,
    ) {
        let ctl: hashterm_tmux::TmuxController = (*self.ctl).clone();
        let weak = Rc::downgrade(self);
        gtk4::glib::spawn_future_local(async move {
            let res = gtk4::gio::spawn_blocking(move || op(&ctl)).await;
            let Some(win) = weak.upgrade() else { return };
            match res {
                Ok(Ok(Some(msg))) => win.show_osd_message(&msg, 1200),
                Ok(Ok(None)) => {}
                Ok(Err(hashterm_tmux::TmuxError::Command { stderr, .. })) => {
                    win.show_osd_message(&stderr, 2500)
                }
                Ok(Err(e)) => win.show_osd_message(&e.to_string(), 2500),
                Err(_) => {}
            }
            win.with_active_page(|p| p.queue_redraw());
        });
    }

    fn active_id(&self) -> Option<hashterm_tmux::TmuxSessionId> {
        self.stack
            .visible_child_name()
            .map(|n| hashterm_tmux::TmuxSessionId(n.to_string()))
    }

    pub fn pane_split(self: &Rc<Self>, horizontal: bool) {
        if let Some(id) = self.active_id() {
            self.pane_op(move |ctl| ctl.split(&id, horizontal).map(|_| None));
        }
    }

    pub fn pane_focus(self: &Rc<Self>, dir: hashterm_tmux::panes::Dir) {
        if let Some(id) = self.active_id() {
            self.pane_op(move |ctl| ctl.select_pane(&id, dir).map(|_| None));
        }
    }

    pub fn pane_zoom(self: &Rc<Self>) {
        if let Some(id) = self.active_id() {
            self.pane_op(move |ctl| {
                ctl.zoom_pane(&id)
                    .map(|z| Some(if z { "Pane zoomed" } else { "Pane restored" }.to_string()))
            });
        }
    }

    fn with_active_page(self: &Rc<Self>, f: impl FnOnce(&TerminalPage)) {
        let Some(name) = self.stack.visible_child_name() else {
            return;
        };
        let tabs = self.tabs.borrow();
        if let Some(t) = tabs.iter().find(|t| t.session == name.as_str()) {
            f(&t.page);
        }
    }

    pub fn copy_selection(self: &Rc<Self>) {
        self.with_active_page(|p| p.copy_selection());
    }

    pub fn paste_clipboard(self: &Rc<Self>) {
        self.with_active_page(|p| p.paste_clipboard());
    }

    pub fn select_tab(self: &Rc<Self>, session: &str) {
        self.stack.set_visible_child_name(session);
        self.withdraw_notification(session);
        let group = {
            let mut tabs = self.tabs.borrow_mut();
            let entry = tabs.iter_mut().find(|t| t.session == session);
            if let Some(entry) = entry {
                entry.badge = None;
                entry.page.grab_focus();
                self.activity_gates.borrow_mut().remove(session);
                Some(entry.group_name().to_owned())
            } else {
                None
            }
        };
        // Selecting a tab of a hidden group (picker, event bus) switches to
        // its group; also remember it as the group's last-active tab.
        if let Some(group) = group {
            self.last_active
                .borrow_mut()
                .insert(group.clone(), session.to_owned());
            if *self.active_group.borrow() != group {
                *self.active_group.borrow_mut() = group;
                self.persist_group_state();
            }
        }
        self.refresh_tabbar();
    }

    /// Sessions of the ACTIVE group, in tab order (hotkey scope).
    fn visible_sessions(self: &Rc<Self>) -> Vec<String> {
        let active = self.active_group.borrow().clone();
        self.tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == active)
            .map(|t| t.session.clone())
            .collect()
    }

    pub fn cycle_tab(self: &Rc<Self>, forward: bool) {
        let visible = self.visible_sessions();
        if visible.is_empty() {
            return;
        }
        let current = self.stack.visible_child_name().map(|s| s.to_string());
        let idx = visible
            .iter()
            .position(|s| Some(s) == current.as_ref())
            .unwrap_or(0);
        let next = if forward {
            (idx + 1) % visible.len()
        } else {
            (idx + visible.len() - 1) % visible.len()
        };
        self.select_tab(&visible[next].clone());
        self.show_tab_osd();
    }

    /// Wheel over the tab bar: step to the neighbouring tab of the active
    /// group, stopping at the ends (a wheel that wraps around is disorienting).
    pub fn scroll_tab(self: &Rc<Self>, forward: bool) {
        let visible = self.visible_sessions();
        let current = self.stack.visible_child_name().map(|s| s.to_string());
        let Some(idx) = visible.iter().position(|s| Some(s) == current.as_ref()) else {
            return;
        };
        let next = if forward {
            idx + 1
        } else if idx > 0 {
            idx - 1
        } else {
            return;
        };
        let Some(target) = visible.get(next).cloned() else {
            return;
        };
        tracing::debug!("wheel: tab {}/{} -> {}", idx + 1, visible.len(), next + 1);
        self.select_tab(&target);
        self.show_tab_osd();
    }

    /// Wheel over a group's title: step through THAT group's tabs, stopping at
    /// its ends. Starts from the visible tab when the group is already active,
    /// else from the tab it last had selected; selecting a tab of another group
    /// switches to it.
    pub fn scroll_group_tabs(self: &Rc<Self>, group: &str, forward: bool) {
        let members: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == group)
            .map(|t| t.session.clone())
            .collect();
        if members.is_empty() {
            return;
        }
        let current = if *self.active_group.borrow() == group {
            self.stack.visible_child_name().map(|s| s.to_string())
        } else {
            self.last_active.borrow().get(group).cloned()
        };
        let idx = current
            .and_then(|c| members.iter().position(|s| *s == c))
            .unwrap_or(0);
        let next = if forward {
            idx + 1
        } else if idx > 0 {
            idx - 1
        } else {
            return;
        };
        let Some(target) = members.get(next).cloned() else {
            return;
        };
        tracing::debug!(
            "group wheel: {group} tab {}/{} -> {}",
            idx + 1,
            members.len(),
            next + 1
        );
        self.select_tab(&target);
        self.show_tab_osd();
    }

    /// Quick rename from the tab's right-click menu. Empty = back to the
    /// automatic title (same semantics as the Properties dialog).
    pub fn rename_tab(self: &Rc<Self>, session: &str) {
        let Some(title) = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.session == session)
            .map(|t| t.title.clone())
        else {
            return;
        };
        let session = session.to_owned();
        self.prompt_text(
            "Rename tab",
            &title,
            "empty = automatic title",
            move |win, name| {
                let id = hashterm_tmux::TmuxSessionId(session.clone());
                let result = if name.is_empty() {
                    win.ctl.clear_custom_title(&id)
                } else {
                    win.ctl.set_custom_title(&id, &name)
                };
                if let Err(e) = result {
                    tracing::error!("renaming tab failed: {e}");
                }
                win.sync_from_server();
            },
        );
    }

    /// Centered "name · 2/3" toast, scoped to the active group.
    /// Tab-switch OSD as a scroll-wheel: the current tab bright in the center
    /// with its "i/N" counter, and the prev/next tabs faded above and below.
    fn show_tab_osd(self: &Rc<Self>) {
        if !self.config.borrow().tabbar.switch_osd {
            return;
        }
        let Some(name) = self.stack.visible_child_name() else {
            return;
        };
        let visible = self.visible_sessions();
        let Some(i) = visible.iter().position(|s| s == name.as_str()) else {
            return;
        };
        let n = visible.len();
        let title_of = |idx: usize| -> String {
            let session = &visible[idx];
            self.tabs
                .borrow()
                .iter()
                .find(|t| &t.session == session)
                .map(|t| t.title.clone())
                .unwrap_or_default()
        };

        self.osd_clear();
        // A dial: the current tab large in the middle, and neighbors receding
        // above and below — smaller and fainter the further out, so they read
        // as rotating into the distance. Reach ±2 once there are enough tabs
        // to keep every ring a distinct tab (avoids duplicates on small sets).
        let radius = if n >= 5 { 2 } else { 1 };
        for k in (1..=radius).rev() {
            if n > 1 {
                self.osd_content
                    .append(&osd_neighbor_label(&title_of((i + n - k) % n), k));
            }
        }
        self.osd_content
            .append(&osd_current_label(&title_of(i), i + 1, n));
        for k in 1..=radius {
            if n > 1 {
                self.osd_content
                    .append(&osd_neighbor_label(&title_of((i + k) % n), k));
            }
        }
        self.reveal_osd(900);
    }

    /// Generic centered toast — one line (group switch, opacity, errors, ...).
    pub fn show_osd_message(self: &Rc<Self>, text: &str, millis: u64) {
        self.osd_clear();
        let label = gtk4::Label::builder()
            .label(text)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .max_width_chars(52)
            .justify(gtk4::Justification::Center)
            .halign(gtk4::Align::Center)
            .build();
        label.add_css_class("osd-msg");
        self.osd_content.append(&label);
        self.reveal_osd(millis);
    }

    fn osd_clear(&self) {
        while let Some(child) = self.osd_content.first_child() {
            self.osd_content.remove(&child);
        }
    }

    fn reveal_osd(self: &Rc<Self>, millis: u64) {
        self.osd_revealer.set_reveal_child(true);
        if let Some(id) = self.osd_timeout.take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = gtk4::glib::timeout_add_local_once(
            std::time::Duration::from_millis(millis),
            move || {
                if let Some(win) = weak.upgrade() {
                    win.osd_timeout.set(None);
                    win.osd_revealer.set_reveal_child(false);
                }
            },
        );
        self.osd_timeout.set(Some(id));
    }

    pub fn toggle_tabbar(self: &Rc<Self>) {
        if self.tabbar.is_revealed() {
            self.tabbar_pinned.set(false);
            self.tabbar.set_revealed(false);
        } else {
            // Manual reveal pins the bar: auto-hide won't conceal it until
            // the user toggles it back.
            self.tabbar_pinned.set(true);
            self.tabbar.set_revealed(true);
        }
    }

    /// Re-apply dropdown geometry to the already-visible window (live config
    /// edits of height/height_pct/width_pct/edge must take effect at once).
    /// The caller has already cleared the remembered height, so config wins.
    pub fn reapply_geometry(self: &Rc<Self>) {
        let dd = self.config.borrow().dropdown.clone();
        if dd.enabled && self.window.is_visible() {
            self.slide.set_edge(dd.edge);
            self.sync_saved_geometry();
            self.backend.place_and_show(&self.window, &dd);
        }
    }

    /// Per-tab properties dialog: rename (empty = back to automatic titles)
    /// and accent color. Both persist as tmux session options, so they
    /// survive restarts and dump/restore.
    pub fn open_tab_properties(self: &Rc<Self>, session: &str) {
        let (title, accent) = {
            let tabs = self.tabs.borrow();
            let Some(t) = tabs.iter().find(|t| t.session == session) else {
                return;
            };
            (t.title.clone(), t.accent.clone())
        };

        let dialog = gtk4::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Tab properties")
            .default_width(360)
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        root.set_margin_top(14);
        root.set_margin_bottom(14);
        root.set_margin_start(14);
        root.set_margin_end(14);

        let name_entry = gtk4::Entry::builder()
            .text(&title)
            .placeholder_text("empty = automatic title")
            .activates_default(true)
            .build();
        root.append(&gtk4::Label::builder().label("Name").xalign(0.0).build());
        root.append(&name_entry);

        // Group row: dropdown of live groups + "New group…" revealing an entry.
        let current_group = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.session == session)
            .map(|t| t.group_name().to_owned())
            .unwrap_or_else(|| hashterm_tmux::controller::DEFAULT_GROUP.to_owned());
        let group_names: Vec<String> = self.groups().into_iter().map(|g| g.name).collect();
        let mut choices: Vec<&str> = group_names.iter().map(String::as_str).collect();
        choices.push("New group…");
        let group_dd = gtk4::DropDown::from_strings(&choices);
        if let Some(pos) = group_names.iter().position(|g| *g == current_group) {
            group_dd.set_selected(pos as u32);
        }
        let group_entry = gtk4::Entry::builder()
            .placeholder_text("new group name")
            .visible(false)
            .build();
        {
            let entry = group_entry.clone();
            let count = group_names.len() as u32;
            group_dd.connect_selected_notify(move |dd| {
                entry.set_visible(dd.selected() == count);
            });
        }
        root.append(&gtk4::Label::builder().label("Group").xalign(0.0).build());
        root.append(&group_dd);
        root.append(&group_entry);

        let color_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let use_accent = gtk4::CheckButton::builder()
            .label("Accent color")
            .active(accent.is_some())
            .build();
        let color_btn = gtk4::ColorDialogButton::new(Some(gtk4::ColorDialog::new()));
        if let Some(a) = accent.as_deref()
            && let Ok(rgba) = gtk4::gdk::RGBA::parse(a)
        {
            color_btn.set_rgba(&rgba);
        }
        color_row.append(&use_accent);
        color_row.append(&color_btn);
        root.append(&color_row);

        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let cancel = gtk4::Button::with_label("Cancel");
        let save = gtk4::Button::with_label("Save");
        save.add_css_class("suggested-action");
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        buttons.append(&spacer);
        buttons.append(&cancel);
        buttons.append(&save);
        root.append(&buttons);
        dialog.set_child(Some(&root));
        dialog.set_default_widget(Some(&save));

        cancel.connect_clicked({
            let dialog = dialog.clone();
            move |_| dialog.close()
        });
        save.connect_clicked({
            let this = Rc::downgrade(self);
            let dialog = dialog.clone();
            let session = session.to_owned();
            move |_| {
                let Some(win) = this.upgrade() else { return };
                let id = hashterm_tmux::TmuxSessionId(session.clone());
                let name = name_entry.text().trim().to_string();
                let result = if name.is_empty() {
                    win.ctl.clear_custom_title(&id)
                } else {
                    win.ctl.set_custom_title(&id, &name)
                };
                let accent = use_accent.is_active().then(|| {
                    let c = color_btn.rgba();
                    format!(
                        "#{:02x}{:02x}{:02x}",
                        (c.red() * 255.0) as u8,
                        (c.green() * 255.0) as u8,
                        (c.blue() * 255.0) as u8
                    )
                });
                let result2 = win.ctl.set_accent(&id, accent.as_deref());
                if let Err(e) = result.and(result2) {
                    tracing::error!("saving tab properties failed: {e}");
                }
                // Group: selected existing name, or the typed new one.
                let selected = group_dd.selected() as usize;
                let target_group = if selected < group_names.len() {
                    group_names[selected].clone()
                } else {
                    hashterm_tmux::controller::sanitize_group_name(group_entry.text().trim())
                };
                if !target_group.is_empty() && target_group != current_group {
                    win.move_tab_to_group(&session, &target_group);
                }
                win.sync_from_server();
                dialog.close();
            }
        });

        dialog.present();
    }

    /// Move tab `session` to position `index` (tab order is GUI-only state).
    pub fn reorder_tab(self: &Rc<Self>, session: &str, index: usize) {
        {
            let mut tabs = self.tabs.borrow_mut();
            let Some(from) = tabs.iter().position(|t| t.session == session) else {
                return;
            };
            let entry = tabs.remove(from);
            let at = index.min(tabs.len());
            tabs.insert(at, entry);
        }
        self.refresh_tabbar();
    }

    /// Named save of the entire current server state (blocking; fast).
    pub fn save_named(self: &Rc<Self>, name: &str) {
        let cfg = self.config.borrow();
        let store =
            hashterm_session::SessionStore::new(hashterm_session::SessionStore::default_root());
        let opts = hashterm_session::DumpOptions {
            exclude_panes: cfg.restore.exclude_panes.clone(),
            max_scrollback_lines: cfg.restore.max_scrollback_lines,
        };
        match hashterm_session::dump_server(&self.ctl, &store, name, false, &opts) {
            Ok(m) => tracing::info!("saved '{name}': {} session(s)", m.sessions.len()),
            Err(e) => tracing::error!("save '{name}' failed: {e}"),
        }
    }

    /// Restore a named save (or "latest") in a worker thread, then adopt the
    /// recreated sessions as tabs.
    pub fn restore_named(self: &Rc<Self>, name: &str) {
        use hashterm_session::{OnConflict, RestoreOptions, SessionStore};
        let ctl = (*self.ctl).clone();
        let cfg = self.config.borrow().clone();
        let name = name.to_owned();
        let (tx, rx) = async_channel::bounded::<()>(1);
        std::thread::spawn(move || {
            let store = SessionStore::new(SessionStore::default_root());
            let Some((concrete, auto)) = store.resolve(&name) else {
                tracing::error!("no save named '{name}'");
                return;
            };
            let opts = RestoreOptions {
                on_conflict: OnConflict::Rename,
                programs: cfg.restore.programs.clone(),
                restart_arbitrary: cfg.restore.restart_arbitrary,
                pretype_unrestored: cfg.restore.pretype_unrestored,
                shell: cfg.general.shell.clone(),
                helper: std::env::current_exe().unwrap_or_else(|_| "hashterm".into()),
                resume: cfg.restore.resume.clone(),
            };
            match hashterm_session::restore_save(&ctl, &store, &concrete, auto, &opts) {
                Ok(report) => {
                    for note in &report.notes {
                        tracing::info!("restore: {note}");
                    }
                }
                Err(e) => tracing::error!("restore '{concrete}' failed: {e}"),
            }
            let _ = tx.send_blocking(());
        });
        let this = Rc::downgrade(self);
        gtk4::glib::spawn_future_local(async move {
            let _ = rx.recv().await;
            if let Some(w) = this.upgrade() {
                w.sync_from_server();
            }
        });
    }

    // ---- tab groups (workspaces) ---------------------------------------

    /// Live groups in display order: persisted order first (pruned), then
    /// unseen groups by first member. Derived color = first member's set one.
    fn groups(self: &Rc<Self>) -> Vec<crate::tabbar::GroupInfo> {
        let tabs = self.tabs.borrow();
        let mut names: Vec<String> = Vec::new();
        for t in tabs.iter() {
            let g = t.group_name().to_owned();
            if !names.contains(&g) {
                names.push(g);
            }
        }
        let order = self.group_order.borrow();
        let mut sorted: Vec<String> = order
            .iter()
            .filter(|g| names.contains(g))
            .cloned()
            .collect();
        for g in names {
            if !sorted.contains(&g) {
                sorted.push(g);
            }
        }
        sorted
            .into_iter()
            .map(|name| {
                let color = tabs
                    .iter()
                    .filter(|t| t.group_name() == name)
                    .find_map(|t| t.group_color.clone());
                let count = tabs.iter().filter(|t| t.group_name() == name).count();
                let (attention, attention_count) = crate::attention::summarize(
                    tabs.iter()
                        .filter(|t| t.group_name() == name)
                        .map(|t| t.badge),
                );
                crate::tabbar::GroupInfo {
                    name,
                    color,
                    count,
                    attention,
                    attention_count,
                }
            })
            .collect()
    }

    /// Put `name` at position `index` in the group bar (persisted; the
    /// group_order is what `groups()` sorts by, so group cycling follows it).
    pub fn reorder_group(self: &Rc<Self>, name: &str, index: usize) {
        let names: Vec<String> = self.groups().into_iter().map(|g| g.name).collect();
        let Some(order) = reordered(&names, name, index) else {
            return;
        };
        *self.group_order.borrow_mut() = order;
        self.persist_group_state();
        self.refresh_tabbar();
    }

    /// Shift a group `delta` places (right-click "Move Earlier/Later").
    pub fn move_group(self: &Rc<Self>, name: &str, delta: i32) {
        let names: Vec<String> = self.groups().into_iter().map(|g| g.name).collect();
        let Some(from) = names.iter().position(|n| n == name) else {
            return;
        };
        let to = (from as i64 + delta as i64).clamp(0, names.len() as i64 - 1) as usize;
        self.reorder_group(name, to);
    }

    fn persist_group_state(self: &Rc<Self>) {
        let mut st = hashterm_core::state::UiState::load();
        st.active_group = Some(self.active_group.borrow().clone());
        st.group_order = self.groups().into_iter().map(|g| g.name).collect();
        *self.group_order.borrow_mut() = st.group_order.clone();
        st.last_active_tab = self.last_active.borrow().clone();
        st.save();
    }

    /// Make `name` the visible workspace and select its last-active tab.
    pub fn switch_group(self: &Rc<Self>, name: &str) {
        if *self.active_group.borrow() == name {
            return;
        }
        // A group exists iff it has members; never switch into a phantom
        // (stale state key, rename-merge race) — it would strand the UI with
        // an empty tab bar and no focused terminal.
        if !self.tabs.borrow().iter().any(|t| t.group_name() == name) {
            return;
        }
        *self.active_group.borrow_mut() = name.to_owned();
        let target = {
            let tabs = self.tabs.borrow();
            let remembered = self.last_active.borrow().get(name).cloned();
            remembered
                .filter(|s| {
                    tabs.iter()
                        .any(|t| t.session == *s && t.group_name() == name)
                })
                .or_else(|| {
                    tabs.iter()
                        .find(|t| t.group_name() == name)
                        .map(|t| t.session.clone())
                })
        };
        if let Some(session) = target {
            self.select_tab(&session);
        }
        self.refresh_tabbar();
        self.persist_group_state();
        self.show_group_osd();
    }

    pub fn cycle_group(self: &Rc<Self>, forward: bool) {
        let groups = self.groups();
        if groups.len() < 2 {
            return;
        }
        let current = self.active_group.borrow().clone();
        let idx = groups.iter().position(|g| g.name == current).unwrap_or(0);
        let next = if forward {
            (idx + 1) % groups.len()
        } else {
            (idx + groups.len() - 1) % groups.len()
        };
        let name = groups[next].name.clone();
        self.switch_group(&name);
    }

    fn show_group_osd(self: &Rc<Self>) {
        let groups = self.groups();
        let current = self.active_group.borrow().clone();
        let idx = groups.iter().position(|g| g.name == current).unwrap_or(0);
        self.show_osd_message(
            &format!("{current}   ·   {}/{}", idx + 1, groups.len()),
            900,
        );
    }

    /// Move a tab into `group` ("main"/empty -> the implicit default group).
    pub fn move_tab_to_group(self: &Rc<Self>, session: &str, group: &str) {
        let id = hashterm_tmux::TmuxSessionId(session.to_owned());
        if let Err(e) = self.ctl.set_group(&id, Some(group)) {
            tracing::error!("move to group failed: {e}");
            return;
        }
        // Inherit the target group's derived color so rings stay coherent.
        let target = hashterm_tmux::controller::sanitize_group_name(group);
        let color = self
            .groups()
            .into_iter()
            .find(|g| g.name == target)
            .and_then(|g| g.color);
        let _ = self.ctl.set_group_color(&id, color.as_deref());
        self.sync_from_server();
    }

    /// GTK color chooser for a group; the picked color lands on EVERY member
    /// (the group's color is derived from its members, self-healing).
    pub fn pick_group_color(self: &Rc<Self>, group: &str) {
        let current = self
            .groups()
            .into_iter()
            .find(|g| g.name == group)
            .and_then(|g| g.color)
            .and_then(|c| gtk4::gdk::RGBA::parse(&c).ok());
        let dialog = gtk4::ColorDialog::new();
        dialog.set_with_alpha(false);
        let this = Rc::downgrade(self);
        let group = group.to_owned();
        dialog.choose_rgba(
            Some(&self.window),
            current.as_ref(),
            gtk4::gio::Cancellable::NONE,
            move |result| {
                // Err = the user cancelled.
                let (Ok(rgba), Some(win)) = (result, this.upgrade()) else {
                    return;
                };
                let hex = format!(
                    "#{:02x}{:02x}{:02x}",
                    (rgba.red() * 255.0) as u8,
                    (rgba.green() * 255.0) as u8,
                    (rgba.blue() * 255.0) as u8
                );
                win.set_group_color_all(&group, Some(&hex));
            },
        );
    }

    /// Write (or clear) the group color on all members, then resync. Clearing
    /// must hit every member, or the self-heal would re-derive it from a
    /// leftover copy.
    pub fn set_group_color_all(self: &Rc<Self>, group: &str, color: Option<&str>) {
        let members: Vec<String> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == group)
            .map(|t| t.session.clone())
            .collect();
        for session in members {
            let id = hashterm_tmux::TmuxSessionId(session);
            if let Err(e) = self.ctl.set_group_color(&id, color) {
                tracing::error!("set group color: {e}");
            }
        }
        self.sync_from_server();
    }

    /// Small name-entry dialog used for "New group..." and group rename.
    fn prompt_name(
        self: &Rc<Self>,
        dialog_title: &str,
        initial: &str,
        on_done: impl Fn(&Rc<Self>, String) + 'static,
    ) {
        // Group names are sanitized, and an empty one means "cancel".
        self.prompt_text(dialog_title, initial, "", move |win, raw| {
            let name = hashterm_tmux::controller::sanitize_group_name(&raw);
            if !name.is_empty() {
                on_done(win, name);
            }
        });
    }

    /// Single-line text prompt; `on_done` gets the trimmed text (possibly
    /// empty — callers decide what empty means).
    fn prompt_text(
        self: &Rc<Self>,
        dialog_title: &str,
        initial: &str,
        placeholder: &str,
        on_done: impl Fn(&Rc<Self>, String) + 'static,
    ) {
        let dialog = gtk4::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title(dialog_title)
            .default_width(300)
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        root.set_margin_top(12);
        root.set_margin_bottom(12);
        root.set_margin_start(12);
        root.set_margin_end(12);
        let entry = gtk4::Entry::builder()
            .text(initial)
            .placeholder_text(placeholder)
            .activates_default(true)
            .build();
        root.append(&entry);
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let cancel = gtk4::Button::with_label("Cancel");
        let ok = gtk4::Button::with_label("OK");
        ok.add_css_class("suggested-action");
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        buttons.append(&spacer);
        buttons.append(&cancel);
        buttons.append(&ok);
        root.append(&buttons);
        dialog.set_child(Some(&root));
        dialog.set_default_widget(Some(&ok));
        cancel.connect_clicked({
            let dialog = dialog.clone();
            move |_| dialog.close()
        });
        ok.connect_clicked({
            let this = Rc::downgrade(self);
            let dialog = dialog.clone();
            move |_| {
                let name = entry.text().trim().to_string();
                dialog.close();
                if let Some(win) = this.upgrade() {
                    on_done(&win, name);
                }
            }
        });
        dialog.present();
    }

    /// "+" on the group bar: creates the group with a fresh tab in it.
    pub fn new_group_with_tab(self: &Rc<Self>) {
        self.prompt_name("New group", "", |win, name| {
            *win.active_group.borrow_mut() = name.clone();
            win.new_tab(); // inherits the (new) active group
            win.persist_group_state();
            win.show_group_osd();
        });
    }

    /// "New group..." from a tab's move menu.
    pub fn move_tab_to_new_group(self: &Rc<Self>, session: &str) {
        let session = session.to_owned();
        self.prompt_name("Move to new group", "", move |win, name| {
            win.move_tab_to_group(&session, &name);
        });
    }

    /// Rename group `old` (rewrites every member; merging on collision).
    pub fn rename_group(self: &Rc<Self>, old: &str) {
        let old = old.to_owned();
        self.prompt_name("Rename group", &old.clone(), move |win, name| {
            let old = old.clone();
            if name == old {
                return;
            }
            let was_active = *win.active_group.borrow() == old;
            let members: Vec<String> = win
                .tabs
                .borrow()
                .iter()
                .filter(|t| t.group_name() == old)
                .map(|t| t.session.clone())
                .collect();
            for session in members {
                let id = hashterm_tmux::TmuxSessionId(session);
                if let Err(e) = win.ctl.set_group(&id, Some(&name)) {
                    tracing::error!("group rename failed: {e}");
                }
            }
            // Fix state keys.
            let target = if name == hashterm_tmux::controller::DEFAULT_GROUP || name.is_empty() {
                hashterm_tmux::controller::DEFAULT_GROUP.to_owned()
            } else {
                name
            };
            let remembered = win.last_active.borrow_mut().remove(&old);
            if let Some(s) = remembered {
                win.last_active.borrow_mut().insert(target.clone(), s);
            }
            if was_active {
                *win.active_group.borrow_mut() = target;
            }
            win.sync_from_server();
            win.persist_group_state();
        });
    }

    /// The active group lost its last tab (or state named a dead group):
    /// fall back to the first existing group.
    fn validate_active_group(self: &Rc<Self>) {
        let groups = self.groups();
        if groups.is_empty() {
            return;
        }
        let current = self.active_group.borrow().clone();
        if !groups.iter().any(|g| g.name == current) {
            let fallback = groups[0].name.clone();
            *self.active_group.borrow_mut() = fallback;
            let target = {
                let name = self.active_group.borrow().clone();
                let tabs = self.tabs.borrow();
                tabs.iter()
                    .find(|t| t.group_name() == name)
                    .map(|t| t.session.clone())
            };
            if let Some(session) = target {
                self.select_tab(&session);
            }
            self.persist_group_state();
        }
    }

    /// Reconcile GUI tabs with live ht-* sessions (startup, hook events, poll).
    pub fn sync_from_server(self: &Rc<Self>) {
        let live = match self.ctl.list_sessions() {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("list-sessions: {e}");
                return;
            }
        };
        // Remove tabs whose session died. The stack children are removed only
        // AFTER the `tabs` borrow is released: tearing down a page can emit
        // VTE's child-exited synchronously, and handle_page_exit re-borrows
        // `tabs` (a held borrow here aborts the whole app with "RefCell
        // already borrowed").
        let removals: Vec<Removal> = {
            let tabs = self.tabs.borrow();
            let gone: Vec<String> = tabs
                .iter()
                .filter(|t| !live.iter().any(|s| s.name == t.session))
                .map(|t| t.session.clone())
                .collect();
            drop(tabs);
            gone.iter().filter_map(|s| self.removal_info(s)).collect()
        };
        let dead: Vec<String> = {
            let mut tabs = self.tabs.borrow_mut();
            let mut dead = Vec::new();
            tabs.retain(|t| {
                let alive = live.iter().any(|s| s.name == t.session);
                if !alive {
                    dead.push(t.session.clone());
                }
                alive
            });
            dead
        };
        for name in &dead {
            if let Some(child) = self.stack.child_by_name(name) {
                self.stack.remove(&child);
            }
        }

        let mut tabs = self.tabs.borrow_mut();
        let tabs_before = tabs.len();

        // Add tabs for sessions we don't show yet (created externally or adopted).
        for info in &live {
            if let Some(t) = tabs.iter_mut().find(|t| t.session == info.name) {
                // Existing tab: refresh server-owned properties.
                t.title_custom = info.title_custom;
                t.accent = info.accent.clone();
                t.group = info.group.clone();
                t.group_color = info.group_color.clone();
                if info.title_custom && !info.title.is_empty() {
                    t.title = info.title.clone();
                }
                // External attach badge.
                if info.attached > 1 && t.badge.is_none() {
                    t.badge = Some(Badge::External);
                }
                continue;
            }
            let argv = self.ctl.attach_argv(&info.id);
            let page = TerminalPage::new(&argv, &self.config.borrow().appearance);
            page.set_links_enabled(self.config.borrow().links.enabled);
            {
                let this = Rc::downgrade(self);
                let session = info.name.clone();
                page.connect_contents_changed(move || {
                    if let Some(win) = this.upgrade() {
                        win.on_tab_contents(&session);
                    }
                });
            }
            {
                let this = Rc::downgrade(self);
                page.set_on_link(Box::new(move |uri| {
                    if let Some(win) = this.upgrade() {
                        win.open_link(&uri);
                    }
                }));
            }
            self.stack
                .add_named(page.widget(), Some(info.name.as_str()));
            let title = if info.title.is_empty() {
                info.name.clone()
            } else {
                info.title.clone()
            };
            {
                let this = Rc::downgrade(self);
                let session = info.name.clone();
                page.set_on_title_changed(Box::new(move |title| {
                    if let Some(win) = this.upgrade() {
                        win.set_tab_title(&session, title);
                    }
                }));
            }
            {
                let this = Rc::downgrade(self);
                let session = info.name.clone();
                page.set_on_child_exited(Box::new(move || {
                    if let Some(win) = this.upgrade() {
                        win.handle_page_exit(&session);
                    }
                }));
            }
            tabs.push(TabEntry {
                session: info.name.clone(),
                title,
                title_custom: info.title_custom,
                accent: info.accent.clone(),
                group: info.group.clone(),
                group_color: info.group_color.clone(),
                badge: None,
                vte_title: String::new(),
                fact_cmd: String::new(),
                fact_path: String::new(),
                page,
            });
        }
        let added = tabs.len() > tabs_before;
        drop(tabs);
        if added {
            // The tmux server exists now (it may be newly started): colour its UI.
            self.apply_tmux_styles();
            // Give a fresh tab its folder name now instead of at the next tick.
            let weak = Rc::downgrade(self);
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
                if let Some(win) = weak.upgrade() {
                    win.poll_titles();
                }
            });
        }

        // Self-heal group colors: every member carries the group's derived
        // color (first member with one set). Write only on divergence.
        {
            let fixes: Vec<(String, Option<String>)> = {
                let tabs = self.tabs.borrow();
                let mut fixes = Vec::new();
                let mut seen: Vec<String> = Vec::new();
                for t in tabs.iter() {
                    let g = t.group_name().to_owned();
                    if seen.contains(&g) {
                        continue;
                    }
                    seen.push(g.clone());
                    let derived = tabs
                        .iter()
                        .filter(|x| x.group_name() == g)
                        .find_map(|x| x.group_color.clone());
                    for member in tabs.iter().filter(|x| x.group_name() == g) {
                        if member.group_color != derived {
                            fixes.push((member.session.clone(), derived.clone()));
                        }
                    }
                }
                fixes
            };
            for (session, color) in fixes {
                let id = hashterm_tmux::TmuxSessionId(session.clone());
                let _ = self.ctl.set_group_color(&id, color.as_deref());
                if let Some(t) = self
                    .tabs
                    .borrow_mut()
                    .iter_mut()
                    .find(|t| t.session == session)
                {
                    t.group_color = color;
                }
            }
        }

        for r in removals {
            self.after_tab_removed(r);
        }
        self.validate_active_group();
        self.refresh_tabbar();
    }

    /// The pane's `tmux attach` process exited (tab detached externally,
    /// session killed, or server died). Drop the page; if the session still
    /// lives on the server, sync re-creates it with a fresh attach.
    pub fn handle_page_exit(self: &Rc<Self>, session: &str) {
        // Can arrive synchronously from a widget teardown that happens while
        // `tabs` is borrowed elsewhere: retry from the main loop instead of
        // panicking (a panic inside a GTK callback aborts the process).
        let Ok(tabs) = self.tabs.try_borrow_mut() else {
            let this = Rc::downgrade(self);
            let session = session.to_owned();
            gtk4::glib::idle_add_local_once(move || {
                if let Some(win) = this.upgrade() {
                    win.handle_page_exit(&session);
                }
            });
            return;
        };
        drop(tabs);
        let removal = self.removal_info(session);
        self.tabs.borrow_mut().retain(|t| t.session != session);
        if let Some(child) = self.stack.child_by_name(session) {
            self.stack.remove(&child);
        }
        self.sync_from_server();
        if let Some(r) = removal {
            self.after_tab_removed(r);
        }
    }

    /// The terminal reported a title (OSC 2, or tmux's default: the host name).
    pub fn set_tab_title(self: &Rc<Self>, session: &str, title: String) {
        if let Some(t) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| t.session == session)
        {
            t.vte_title = title;
        }
        self.retitle(session);
    }

    /// Recompute a tab's label from what the terminal said and what tmux
    /// knows about the pane (folder, foreground program).
    fn retitle(self: &Rc<Self>, session: &str) {
        let auto = self.config.borrow().tabbar.auto_titles;
        {
            let mut tabs = self.tabs.borrow_mut();
            let Some(t) = tabs.iter_mut().find(|t| t.session == session) else {
                return;
            };
            if t.title_custom {
                return; // user-pinned name: automatic titles don't apply
            }
            let title = if auto {
                crate::title::auto_title(
                    &t.vte_title,
                    &t.fact_cmd,
                    &t.fact_path,
                    crate::title::Env::current(),
                )
            } else {
                t.vte_title.clone()
            };
            if title.is_empty() || t.title == title {
                return; // nothing to show, or no change: skip the widget rebuild
            }
            tracing::debug!("tab {session} title -> {title:?}");
            t.title = title;
        }
        self.refresh_tabbar();
    }

    /// Poll tmux for each tab's folder and foreground program while the
    /// window is on screen (one `list-panes -a` call off the GTK thread,
    /// about 5 ms). tmux only pushes the FIRST folder change as a title, so
    /// this is what keeps labels following `cd`.
    fn wire_title_poll(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        gtk4::glib::timeout_add_local(std::time::Duration::from_secs(2), move || {
            let Some(win) = this.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            if win.window.is_visible() {
                win.poll_titles();
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    /// One off-thread `list-panes -a` round trip; a no-op while one is in
    /// flight or when automatic titles are off.
    fn poll_titles(self: &Rc<Self>) {
        if self.title_poll_busy.get() || !self.config.borrow().tabbar.auto_titles {
            return;
        }
        self.title_poll_busy.set(true);
        let ctl: hashterm_tmux::TmuxController = (*self.ctl).clone();
        let weak = Rc::downgrade(self);
        gtk4::glib::spawn_future_local(async move {
            let facts = gtk4::gio::spawn_blocking(move || ctl.pane_facts()).await;
            let Some(win) = weak.upgrade() else { return };
            win.title_poll_busy.set(false);
            if let Ok(Ok(facts)) = facts {
                win.apply_pane_facts(facts);
            }
        });
    }

    fn apply_pane_facts(self: &Rc<Self>, facts: Vec<hashterm_tmux::controller::PaneFact>) {
        let mut changed = Vec::new();
        {
            let mut tabs = self.tabs.borrow_mut();
            for f in facts {
                if let Some(t) = tabs.iter_mut().find(|t| t.session == f.session)
                    && (t.fact_cmd != f.command || t.fact_path != f.path)
                {
                    t.fact_cmd = f.command;
                    t.fact_path = f.path;
                    changed.push(t.session.clone());
                }
            }
        }
        for session in changed {
            self.retitle(&session);
        }
    }

    /// Seen = this tab is showing, the window is on screen, and it has focus.
    fn is_seen(&self, session: &str) -> bool {
        self.stack.visible_child_name().as_deref() == Some(session)
            && self.window.is_visible()
            && self.window.is_active()
    }

    /// The window just became visible or focused: whatever tab is showing has
    /// been seen, so its badge (set while we were away) goes.
    pub fn mark_active_seen(self: &Rc<Self>) {
        let Some(name) = self.stack.visible_child_name() else {
            return;
        };
        if !self.is_seen(name.as_str()) {
            return;
        }
        let cleared = {
            let mut tabs = self.tabs.borrow_mut();
            tabs.iter_mut()
                .find(|t| t.session == name.as_str())
                .is_some_and(|t| t.badge.take().is_some())
        };
        if cleared {
            self.withdraw_notification(name.as_str());
            self.refresh_tabbar();
        }
    }

    pub fn set_badge(self: &Rc<Self>, session: &str, badge: Badge) {
        let seen = self.is_seen(session);
        tracing::debug!("badge {session} {badge:?} seen={seen}");
        let notif = self.config.borrow().notifications.clone();
        if !seen {
            let mut tabs = self.tabs.borrow_mut();
            let Some(t) = tabs.iter_mut().find(|t| t.session == session) else {
                return;
            };
            // A stronger badge replaces a weaker one, never the reverse.
            let (best, _) = crate::attention::summarize([t.badge, Some(badge)]);
            if t.badge != best {
                t.badge = best;
                drop(tabs);
                self.refresh_tabbar();
            }
        }
        let wanted = notif.enabled
            && crate::attention::notify_wanted(badge, notif.on_bell, notif.on_activity)
            && (!seen || !notif.only_when_unseen);
        if wanted {
            self.notify_tab(session, badge, notif.min_interval_secs);
        }
    }

    /// A background tab's screen changed: after 5 s of silence that is
    /// "activity". Redraws right after a tab is created or shown are not.
    fn on_tab_contents(self: &Rc<Self>, session: &str) {
        if self.is_seen(session) {
            return;
        }
        let young = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.session == session)
            .is_none_or(|t| t.page.age() < std::time::Duration::from_secs(4));
        if young || !self.window_settled() {
            return;
        }
        let fire = self
            .activity_gates
            .borrow_mut()
            .entry(session.to_string())
            .or_default()
            .on_change(std::time::Instant::now(), std::time::Duration::from_secs(5));
        if fire {
            self.set_badge(session, Badge::Activity);
        }
    }

    /// True unless the window was shown/resized a moment ago (tmux repaints
    /// every pane on a resize, which is not something to be told about).
    fn window_settled(&self) -> bool {
        self.last_layout_change.get().elapsed() > std::time::Duration::from_millis(1500)
    }

    fn notify_tab(self: &Rc<Self>, session: &str, badge: Badge, min_interval_secs: u32) {
        let allowed = self.notify_limiter.borrow_mut().allow(
            session,
            std::time::Instant::now(),
            std::time::Duration::from_secs(min_interval_secs as u64),
        );
        if !allowed {
            return;
        }
        let Some(app) = self.window.application() else {
            return;
        };
        let (title, group) = {
            let tabs = self.tabs.borrow();
            match tabs.iter().find(|t| t.session == session) {
                Some(t) => (t.title.clone(), t.group_name().to_string()),
                None => return,
            }
        };
        let what = match badge {
            Badge::Bell => "bell",
            _ => "new output",
        };
        let n = gtk4::gio::Notification::new("hashterm");
        let place = if group == hashterm_tmux::controller::DEFAULT_GROUP {
            title
        } else {
            format!("{group} \u{203A} {title}")
        };
        // Titles come from program output: keep them plain, short and inert.
        n.set_body(Some(&format!(
            "{}: {what}",
            crate::links::plain_text(&place, 80)
        )));
        n.set_default_action_and_target_value("app.show-tab", Some(&session.to_variant()));
        app.send_notification(Some(&format!("tab-{session}")), &n);
    }

    fn withdraw_notification(&self, session: &str) {
        if let Some(app) = self.window.application() {
            app.withdraw_notification(&format!("tab-{session}"));
        }
    }

    /// Notification click: show the console on that tab.
    pub fn show_tab(self: &Rc<Self>, session: &str) {
        self.present();
        self.select_tab(session);
    }

    // ---- config-driven layout ------------------------------------------

    pub fn apply_config(self: &Rc<Self>) {
        let cfg = self.config.borrow().tabbar.clone();
        self.tabbar.apply_config(&cfg);
        {
            let hk = self.config.borrow().hotkeys.clone();
            let label = |accel: &str| self.accel_label(accel);
            self.tabbar
                .set_shortcut_tips(&label(&hk.new_tab), &label(&hk.close_tab));
        }
        self.place_tabbar();
        self.place_groupbar();
        self.apply_window_classes();
        self.tabbar_pinned.set(false);
        self.tabbar.set_revealed(!cfg.auto_hide);
        self.hover_strip.set_visible(cfg.auto_hide);
        let appearance = self.config.borrow().appearance.clone();
        hashterm_core::logfile::set_file_enabled(self.config.borrow().general.log_to_file);
        self.window.set_opacity(self.effective_opacity());
        for tab in self.tabs.borrow().iter() {
            tab.page.apply_appearance(&appearance);
            tab.page.set_font_scale(self.font_scale.get());
            tab.page
                .set_links_enabled(self.config.borrow().links.enabled);
        }
        self.apply_tmux_styles();
        self.refresh_toggle_shortcut();
        self.refresh_tabbar();
    }

    fn place_tabbar(&self) {
        let cfg = self.config.borrow().tabbar.clone();
        let edge = cfg.edge;
        let bar = self.tabbar.widget().clone();
        if let Some(parent) = bar.parent() {
            if parent.downcast_ref::<gtk4::Grid>().is_some() {
                self.grid.remove(&bar);
            } else if let Some(ov) = parent.downcast_ref::<gtk4::Overlay>() {
                ov.remove_overlay(&bar);
            }
        }
        // A live edge swap may move the tab bar into the cell the group bar
        // still occupies (place_groupbar runs after us): clear it first so
        // the grid never holds two children in one cell, even transiently.
        let gbar = self.groupbar.widget().clone();
        if let Some(parent) = gbar.parent()
            && parent.downcast_ref::<gtk4::Grid>().is_some()
        {
            self.grid.remove(&gbar);
        }
        bar.remove_css_class("overlaid");

        if cfg.auto_hide {
            // Auto-hidden bar FLOATS over the terminal: revealing must never
            // reflow the panes (a grid row would resize the VTE -> SIGWINCH
            // -> tmux relayout on every reveal).
            match edge {
                Edge::Top => {
                    bar.set_halign(gtk4::Align::Fill);
                    bar.set_valign(gtk4::Align::Start);
                }
                Edge::Bottom => {
                    bar.set_halign(gtk4::Align::Fill);
                    bar.set_valign(gtk4::Align::End);
                }
                Edge::Left => {
                    bar.set_halign(gtk4::Align::Start);
                    bar.set_valign(gtk4::Align::Fill);
                }
                Edge::Right => {
                    bar.set_halign(gtk4::Align::End);
                    bar.set_valign(gtk4::Align::Fill);
                }
            }
            bar.add_css_class("overlaid");
            self.overlay.add_overlay(&bar);
        } else {
            bar.set_halign(gtk4::Align::Fill);
            bar.set_valign(gtk4::Align::Fill);
            pin_bar_expand(&bar, edge);
            let (col, row) = match edge {
                Edge::Top => (1, 0),
                Edge::Bottom => (1, 2),
                Edge::Left => (0, 1),
                Edge::Right => (2, 1),
            };
            self.grid.attach(&bar, col, row, 1, 1);
        }

        // Park the hover strip on the same edge.
        let strip = &self.hover_strip;
        match edge {
            Edge::Top => {
                strip.set_halign(gtk4::Align::Fill);
                strip.set_valign(gtk4::Align::Start);
                strip.set_height_request(2);
                strip.set_width_request(-1);
            }
            Edge::Bottom => {
                strip.set_halign(gtk4::Align::Fill);
                strip.set_valign(gtk4::Align::End);
                strip.set_height_request(2);
                strip.set_width_request(-1);
            }
            Edge::Left => {
                strip.set_halign(gtk4::Align::Start);
                strip.set_valign(gtk4::Align::Fill);
                strip.set_width_request(2);
                strip.set_height_request(-1);
            }
            Edge::Right => {
                strip.set_halign(gtk4::Align::End);
                strip.set_valign(gtk4::Align::Fill);
                strip.set_width_request(2);
                strip.set_height_request(-1);
            }
        }
    }

    fn wire_tabbar(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        self.tabbar.set_on_event(Rc::new(move |ev| {
            let Some(win) = this.upgrade() else { return };
            match ev {
                TabBarEvent::Select(id) => {
                    win.select_tab(&id);
                    win.show_tab_osd();
                }
                TabBarEvent::Close(id) => win.request_close_tab(&id),
                TabBarEvent::NewTab => win.new_tab(),
                TabBarEvent::NewGroup => win.new_group_with_tab(),
                TabBarEvent::Reorder(id, index) => win.reorder_tab(&id, index),
                TabBarEvent::Properties(id) => win.open_tab_properties(&id),
                TabBarEvent::Rename(id) => win.rename_tab(&id),
                TabBarEvent::Scroll(forward) => win.scroll_tab(forward),
                TabBarEvent::MoveToGroup(session, group) => win.move_tab_to_group(&session, &group),
                TabBarEvent::MoveToNewGroup(session) => win.move_tab_to_new_group(&session),
                TabBarEvent::RenameGroup(name) => win.rename_group(&name),
                TabBarEvent::GroupColor(name) => win.pick_group_color(&name),
                TabBarEvent::ClearGroupColor(name) => win.set_group_color_all(&name, None),
            }
        }));
    }

    fn wire_groupbar(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        self.groupbar.set_on_event(Rc::new(move |ev| {
            let Some(win) = this.upgrade() else { return };
            match ev {
                GroupBarEvent::Select(name) => win.switch_group(&name),
                GroupBarEvent::New => win.new_group_with_tab(),
                GroupBarEvent::Rename(name) => win.rename_group(&name),
                GroupBarEvent::Color(name) => win.pick_group_color(&name),
                GroupBarEvent::ClearColor(name) => win.set_group_color_all(&name, None),
                GroupBarEvent::MoveTab(session, group) => win.move_tab_to_group(&session, &group),
                GroupBarEvent::MoveTabToNew(session) => win.move_tab_to_new_group(&session),
                GroupBarEvent::Reorder(name, index) => win.reorder_group(&name, index),
                GroupBarEvent::Shift(name, delta) => win.move_group(&name, delta),
                GroupBarEvent::Scroll(name, forward) => win.scroll_group_tabs(&name, forward),
                GroupBarEvent::Close(name) => win.close_group(&name),
            }
        }));
    }

    /// Park the group bar on its (always perpendicular-to-tabs) grid edge.
    fn place_groupbar(self: &Rc<Self>) {
        let edge = resolved_groupbar_edge(&self.config.borrow());
        let bar = self.groupbar.widget().clone();
        if let Some(parent) = bar.parent()
            && parent.downcast_ref::<gtk4::Grid>().is_some()
        {
            self.grid.remove(&bar);
        }
        bar.set_halign(gtk4::Align::Fill);
        bar.set_valign(gtk4::Align::Fill);
        pin_bar_expand(&bar, edge);
        let (col, row) = match edge {
            Edge::Top => (1, 0),
            Edge::Bottom => (1, 2),
            Edge::Left => (0, 1),
            Edge::Right => (2, 1),
        };
        self.grid.attach(&bar, col, row, 1, 1);
        self.groupbar
            .apply_edge(edge, self.config.borrow().groupbar.reveal_ms);
    }

    fn wire_autohide(self: &Rc<Self>) {
        let cfg = self.config.borrow().tabbar.clone();
        self.tabbar.set_revealed(!cfg.auto_hide);
        self.hover_strip.set_visible(cfg.auto_hide);

        // Window-level motion watcher: reveal when the pointer nears the
        // bar's edge. (A hover target widget doesn't work — the resize
        // handles overlay the same edge and win the pick.)
        let motion = gtk4::EventControllerMotion::new();
        motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let handler = {
                let this = Rc::downgrade(self);
                move |x: f64, y: f64| {
                    let Some(win) = this.upgrade() else { return };
                    let cfg = win.config.borrow().tabbar.clone();
                    if !cfg.auto_hide {
                        return;
                    }
                    let (w, h) = (win.window.width() as f64, win.window.height() as f64);
                    const NEAR: f64 = 8.0;
                    // Bar thickness (edge-normal axis) plus a comfort margin
                    // counts as "still in the zone".
                    let bar = win.tabbar.widget();
                    let (thickness, distance) = match cfg.edge {
                        Edge::Top => (bar.height(), y),
                        Edge::Bottom => (bar.height(), h - y),
                        Edge::Left => (bar.width(), x),
                        Edge::Right => (bar.width(), w - x),
                    };
                    let zone = (thickness as f64).max(NEAR) + 24.0;
                    if !win.tabbar.is_revealed() {
                        if distance <= NEAR {
                            win.tabbar.set_revealed(true);
                        }
                    } else if distance > zone
                        && !win.tabbar.is_menu_open()
                        && !win.tabbar_pinned.get()
                    {
                        let badged = cfg.reveal_on_badge
                            && win.tabs.borrow().iter().any(|t| t.badge.is_some());
                        if !badged {
                            win.tabbar.set_revealed(false);
                        }
                    }
                }
            };
            let h2 = handler.clone();
            motion.connect_motion(move |_, x, y| handler(x, y));
            // Pointer jumps produce enter crossings without motion events.
            motion.connect_enter(move |_, x, y| h2(x, y));
        }
        self.window.add_controller(motion);

        // Leaving the bar conceals it after a grace period.
        let leave = gtk4::EventControllerMotion::new();
        {
            let this = Rc::downgrade(self);
            leave.connect_leave(move |_| {
                let Some(win) = this.upgrade() else { return };
                if !win.config.borrow().tabbar.auto_hide {
                    return;
                }
                let this2 = Rc::downgrade(&win);
                gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(300),
                    move || {
                        if let Some(win) = this2.upgrade()
                            && win.config.borrow().tabbar.auto_hide
                            && !win.tabbar.is_menu_open()
                            && !win.tabbar_pinned.get()
                        {
                            let badged = win.config.borrow().tabbar.reveal_on_badge
                                && win.tabs.borrow().iter().any(|t| t.badge.is_some());
                            if !badged {
                                win.tabbar.set_revealed(false);
                            }
                        }
                    },
                );
            });
        }
        self.tabbar.widget().add_controller(leave);
    }

    fn adopt_or_create(self: &Rc<Self>) {
        if let Err(e) = self.ctl.ensure_server() {
            tracing::error!("cannot start dedicated tmux server: {e}");
        }
        self.sync_from_server();
        // Detect whether the previous run ended uncleanly (crash/kill/reboot)
        // and arm the lock for this run — must happen once at startup.
        let unclean = hashterm_core::state::take_unclean_and_arm();
        if self.tabs.borrow().is_empty() {
            let store =
                hashterm_session::SessionStore::new(hashterm_session::SessionStore::default_root());
            let has_save = store.resolve("latest").is_some();
            let cfg = self.config.borrow().session.clone();
            if cfg.restore_on_start && has_save {
                // Silent auto-restore (opt-in).
                self.restore_named("latest");
            } else if cfg.prompt_on_start && unclean && has_save {
                // Browser-style: a fresh scratch tab now, then ask.
                self.new_tab();
                self.prompt_restore_previous(&store);
            } else {
                self.new_tab();
            }
        }
        // Select the active group's remembered tab (or its first).
        let target = {
            let group = self.active_group.borrow().clone();
            let tabs = self.tabs.borrow();
            self.last_active
                .borrow()
                .get(&group)
                .cloned()
                .filter(|s| {
                    tabs.iter()
                        .any(|t| t.session == *s && t.group_name() == group)
                })
                .or_else(|| {
                    tabs.iter()
                        .find(|t| t.group_name() == group)
                        .map(|t| t.session.clone())
                })
                .or_else(|| tabs.first().map(|t| t.session.clone()))
        };
        if let Some(s) = target {
            self.select_tab(&s);
        }
    }

    /// Browser-style "restore previous session?" dialog, shown at startup
    /// after an unclean exit. On Restore, the newest save is restored
    /// alongside the fresh scratch tab; on Start fresh, nothing happens.
    fn prompt_restore_previous(self: &Rc<Self>, store: &hashterm_session::SessionStore) {
        let (count, when) = match store
            .resolve("latest")
            .and_then(|(n, a)| store.load(&n, a).ok())
        {
            Some((m, _)) => (
                m.sessions.len(),
                if m.created_local.is_empty() {
                    m.created_at
                } else {
                    m.created_local
                },
            ),
            None => return,
        };
        let dialog = gtk4::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Restore previous session")
            .default_width(420)
            .build();
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.append(
            &gtk4::Label::builder()
                .label(format!(
                    "Your previous session ended unexpectedly.\n\nRestore {count} terminal(s) from {when}?"
                ))
                .wrap(true)
                .xalign(0.0)
                .build(),
        );
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        let fresh = gtk4::Button::with_label("Start fresh");
        let restore = gtk4::Button::with_label("Restore");
        restore.add_css_class("suggested-action");
        buttons.append(&spacer);
        buttons.append(&fresh);
        buttons.append(&restore);
        root.append(&buttons);
        dialog.set_child(Some(&root));
        dialog.set_default_widget(Some(&restore));

        fresh.connect_clicked({
            let dialog = dialog.clone();
            move |_| dialog.close()
        });
        restore.connect_clicked({
            let this = Rc::downgrade(self);
            let dialog = dialog.clone();
            move |_| {
                if let Some(win) = this.upgrade() {
                    win.restore_named("latest");
                }
                dialog.close();
            }
        });
        dialog.present();
    }

    fn refresh_tabbar(self: &Rc<Self>) {
        // Rebuilding widgets on an unmapped window leaks ~390 kB per call in
        // GTK 4.22 (and nothing is painted while hidden anyway): defer to the
        // next map. map() runs one rebuild then.
        if !self.window.is_mapped() {
            self.ui_dirty.set(true);
            return;
        }
        let active = self.stack.visible_child_name().map(|s| s.to_string());
        let active_group = self.active_group.borrow().clone();
        let infos: Vec<TabInfo> = self
            .tabs
            .borrow()
            .iter()
            .filter(|t| t.group_name() == active_group)
            .map(|t| TabInfo {
                id: t.session.clone(),
                title: t.title.clone(),
                badge: t.badge,
                accent: t.accent.clone(),
            })
            .collect();
        let groups = self.groups();
        let gb_cfg = self.config.borrow().groupbar.clone();
        // Reveal, don't set_visible: the Revealer's slide transition should
        // play when the group count crosses the hide_when_single threshold.
        let group_bar_hidden = gb_cfg.hide_when_single && groups.len() < 2;
        self.groupbar.widget().set_reveal_child(!group_bar_hidden);
        // While the matrix' second axis is hidden, offer it where the eye is.
        {
            let accel = self.config.borrow().hotkeys.new_group.clone();
            let label = self.accel_label(&accel);
            self.tabbar.set_group_chip(group_bar_hidden, &label);
        }
        self.groupbar.set_groups(
            groups.clone(),
            active_group.clone(),
            gb_cfg.show_counts,
            gb_cfg.reorderable,
        );
        self.tabbar.set_tabs(infos, active, groups, active_group);
        self.apply_accents();
    }

    /// Regenerate accent CSS and toggle page classes. Rings are stacked
    /// INSET BOX-SHADOWS (no layout impact -> no VTE resize on toggle):
    /// outer ring = group color, inner ring = per-tab accent.
    fn apply_accents(self: &Rc<Self>) {
        let width = self.config.borrow().appearance.tab_border_width as u32;
        let mut css = String::new();
        let mut seen = std::collections::BTreeSet::new();
        let valid = |c: Option<&str>| {
            c.filter(|c| crate::css::parse_hex(c).is_some())
                .map(str::to_owned)
        };
        for t in self.tabs.borrow().iter() {
            let accent = valid(t.accent.as_deref());
            let group = valid(t.group_color.as_deref());
            if let Some(a) = &accent {
                let hex = &a[1..];
                if seen.insert(format!("t{hex}")) {
                    css.push_str(&format!(
                        ".tab.acc-{hex} {{ box-shadow: inset 0 -3px 0 0 {a}; }}\n"
                    ));
                    if width > 0 {
                        css.push_str(&format!(
                            ".terminal-page.accb-{hex} {{ box-shadow: inset 0 0 0 {width}px {a}; border-radius: 6px; }}\n"
                        ));
                    }
                }
            }
            if let Some(g) = &group {
                let hex = &g[1..];
                if seen.insert(format!("g{hex}")) {
                    css.push_str(&format!(
                        ".group-dot.gacc-{hex} {{ background-color: {g}; }}\n"
                    ));
                    if width > 0 {
                        css.push_str(&format!(
                            ".terminal-page.gaccb-{hex} {{ box-shadow: inset 0 0 0 {width}px {g}; border-radius: 6px; }}\n"
                        ));
                    }
                }
            }
            // Both set: two concentric rings (outer group, inner tab).
            if width > 0
                && let (Some(a), Some(g)) = (&accent, &group)
                && seen.insert(format!("b{}{}", &g[1..], &a[1..]))
            {
                // GTK paints LATER box-shadows on top: the wide tab ring goes
                // first, the narrow group ring last so it stays visible at
                // the very edge (outer = group, inner = tab).
                css.push_str(&format!(
                    ".terminal-page.gaccb-{gh}.accb-{ah} {{ box-shadow: inset 0 0 0 {double}px {a}, inset 0 0 0 {width}px {g}; border-radius: 6px; }}\n",
                    gh = &g[1..],
                    ah = &a[1..],
                    double = width * 2,
                ));
            }
        }
        self.accent_css.load_from_string(&css);

        for t in self.tabs.borrow().iter() {
            let widget = t.page.widget();
            for class in widget.css_classes() {
                if class.starts_with("accb-") || class.starts_with("gaccb-") {
                    widget.remove_css_class(&class);
                }
            }
            if width == 0 {
                continue;
            }
            if let Some(a) = valid(t.accent.as_deref()) {
                widget.add_css_class(&format!("accb-{}", &a[1..]));
            }
            if let Some(g) = valid(t.group_color.as_deref()) {
                widget.add_css_class(&format!("gaccb-{}", &g[1..]));
            }
        }
    }

    fn active_entry_cwd(&self) -> Option<std::path::PathBuf> {
        let session = self.stack.visible_child_name()?;
        let out = self
            .ctl
            .run(&[
                "display-message",
                "-p",
                "-t",
                session.as_str(),
                "#{pane_current_path}",
            ])
            .ok()?;
        let path = std::path::PathBuf::from(out.trim());
        path.is_dir().then_some(path)
    }

    pub fn present(self: &Rc<Self>) {
        let dd = self.config.borrow().dropdown.clone();
        if dd.enabled && !self.window.is_visible() {
            self.show_dropdown(&dd);
        } else {
            self.window.present();
        }
    }

    /// Console-mode window styling: edge-aware corner rounding (square at the
    /// anchored screen edge) via CSS classes.
    fn apply_window_classes(&self) {
        for c in [
            "dropdown",
            "edge-top",
            "edge-bottom",
            "edge-left",
            "edge-right",
        ] {
            self.window.remove_css_class(c);
        }
        let dd = self.config.borrow().dropdown.clone();
        if dd.enabled {
            self.window.add_css_class("dropdown");
            self.window.add_css_class(match dd.edge {
                Edge::Top => "edge-top",
                Edge::Bottom => "edge-bottom",
                Edge::Left => "edge-left",
                Edge::Right => "edge-right",
            });
        }
    }

    /// Log the allocation chain (debug aid for rendering issues).
    pub fn debug_geometry(self: &Rc<Self>) {
        let w = &self.window;
        tracing::info!(
            "geometry: window={}x{} visible={} | slide={}x{} | stack={}x{} visible_child={:?}",
            w.width(),
            w.height(),
            w.is_visible(),
            self.slide.width(),
            self.slide.height(),
            self.stack.width(),
            self.stack.height(),
            self.stack.visible_child_name().map(|s| s.to_string()),
        );
        for t in self.tabs.borrow().iter() {
            let pw = t.page.widget();
            tracing::info!(
                "  page '{}': alloc={}x{} mapped={}",
                t.session,
                pw.width(),
                pw.height(),
                pw.is_mapped()
            );
        }
    }

    // ---- links ------------------------------------------------------------

    /// Open a link the user Ctrl+clicked, if its scheme is on the allowlist.
    pub fn open_link(self: &Rc<Self>, uri: &str) {
        if !crate::links::is_sane(uri) {
            return;
        }
        if !crate::links::is_allowed(uri) {
            let scheme = crate::links::scheme_of(uri)
                .map(|s| crate::links::plain_text(&s, 16))
                .unwrap_or_else(|| "unknown".into());
            self.show_osd_message(&format!("Won't open {scheme}: links"), 2500);
            return;
        }
        // Debug builds only (like HASHTERM_SOCKET): record instead of launching,
        // so headless tests can assert exactly what a click would open.
        if cfg!(debug_assertions)
            && let Some(path) = std::env::var_os("HASHTERM_TEST_OPEN_LOG")
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{uri}");
            }
            return;
        }
        let weak = Rc::downgrade(self);
        gtk4::UriLauncher::new(uri).launch(
            Some(&self.window),
            gtk4::gio::Cancellable::NONE,
            move |result| {
                if let Err(e) = result {
                    tracing::warn!("cannot open a link: {e}");
                    if let Some(win) = weak.upgrade() {
                        win.show_osd_message("Couldn't open that link", 2500);
                    }
                }
            },
        );
    }

    // ---- scrollback search (tmux copy-mode) ----------------------------

    fn wire_search(self: &Rc<Self>) {
        let (w1, w2, w3) = (
            Rc::downgrade(self),
            Rc::downgrade(self),
            Rc::downgrade(self),
        );
        let bar = crate::search::SearchBar::new(
            Rc::new(move |text| {
                if let Some(win) = w1.upgrade() {
                    win.run_search(Some(text));
                }
            }),
            Rc::new(move |reverse| {
                if let Some(win) = w2.upgrade() {
                    win.step_search(reverse);
                }
            }),
            Rc::new(move || {
                if let Some(win) = w3.upgrade() {
                    win.close_search();
                }
            }),
        );
        self.overlay.add_overlay(bar.widget());
        *self.search.borrow_mut() = Some(bar);
        // Switching tab ends the search on the old one.
        let weak = Rc::downgrade(self);
        self.stack.connect_visible_child_name_notify(move |_| {
            if let Some(win) = weak.upgrade() {
                win.close_search();
            }
        });
    }

    fn search_bar(&self) -> Option<Rc<crate::search::SearchBar>> {
        self.search.borrow().clone()
    }

    pub fn open_search(self: &Rc<Self>) {
        if let Some(bar) = self.search_bar() {
            bar.open();
            // Reopened with text from last time: search it again right away.
            if !bar.text().is_empty() {
                self.run_search(Some(bar.text()));
            }
        }
    }

    pub fn close_search(self: &Rc<Self>) {
        let Some(bar) = self.search_bar() else { return };
        if !bar.is_open() {
            return;
        }
        bar.close();
        self.search_gen.bump(); // drop in-flight results
        let target = self
            .search_target
            .borrow_mut()
            .take()
            .or_else(|| self.stack.visible_child_name().map(|n| n.to_string()));
        if let Some(name) = target {
            let ctl: hashterm_tmux::TmuxController = (*self.ctl).clone();
            let id = hashterm_tmux::TmuxSessionId(name);
            std::thread::spawn(move || ctl.search_cancel(&id));
        }
        self.with_active_page(|p| p.grab_focus());
    }

    /// New query (`Some`) or step (`None`) on the visible tab, off the UI
    /// thread; a result is shown only if no newer query was started meanwhile.
    fn run_search(self: &Rc<Self>, text: Option<String>) {
        self.search_job(move |ctl, id| match &text {
            Some(t) => ctl.search_start(id, t),
            None => ctl.search_info(id),
        });
    }

    fn step_search(self: &Rc<Self>, reverse: bool) {
        self.search_job(move |ctl, id| ctl.search_step(id, reverse));
    }

    fn search_job(
        self: &Rc<Self>,
        job: impl FnOnce(
            &hashterm_tmux::TmuxController,
            &hashterm_tmux::TmuxSessionId,
        )
            -> Result<hashterm_tmux::search::SearchResult, hashterm_tmux::TmuxError>
        + Send
        + 'static,
    ) {
        let (Some(bar), Some(name)) = (self.search_bar(), self.stack.visible_child_name()) else {
            return;
        };
        let ticket = self.search_gen.bump();
        *self.search_target.borrow_mut() = Some(name.to_string());
        let gen_handle = self.search_gen.clone();
        let ctl: hashterm_tmux::TmuxController = (*self.ctl).clone();
        let id = hashterm_tmux::TmuxSessionId(name.to_string());
        let weak = Rc::downgrade(self);
        gtk4::glib::spawn_future_local(async move {
            // A query superseded while it waited for the tmux lock never runs.
            let res = gtk4::gio::spawn_blocking(move || {
                // Superseded while queued: never touch tmux. (The controller
                // methods serialize themselves; do not lock again here.)
                gen_handle.is_current(ticket).then(|| job(&ctl, &id))
            })
            .await;
            let Some(win) = weak.upgrade() else { return };
            if !win.search_gen.is_current(ticket) {
                return;
            }
            match res {
                Ok(None) => {}
                Ok(Some(Ok(r))) => bar.show_result(r.present, r.count),
                Ok(Some(Err(e))) => {
                    tracing::debug!("search failed: {e}");
                    bar.show_result(false, Some(0));
                }
                Err(_) => {}
            }
        });
    }

    // ---- colour scheme, zoom -------------------------------------------

    /// Text zoom for this window only. `delta` 0 resets; clamped to 0.5x..4x.
    pub fn zoom(self: &Rc<Self>, delta: f64) {
        let next = if delta == 0.0 {
            1.0
        } else {
            ((self.font_scale.get() + delta) * 100.0).round() / 100.0
        }
        .clamp(0.5, 4.0);
        self.font_scale.set(next);
        for tab in self.tabs.borrow().iter() {
            tab.page.set_font_scale(next);
        }
        self.show_osd_message(
            &format!("Text size {}%", (next * 100.0).round() as i32),
            900,
        );
    }

    pub fn set_restyle(&self, f: Rc<dyn Fn()>) {
        *self.restyle.borrow_mut() = Some(f);
    }

    /// The effective colours changed without a config edit (desktop theme flip).
    fn apply_appearance_only(self: &Rc<Self>) {
        let f = self.restyle.borrow().clone();
        if let Some(f) = f {
            f();
        }
        self.apply_config();
    }

    /// Push the scheme's colours into tmux's own UI (off the UI thread).
    fn apply_tmux_styles(&self) {
        let styles =
            hashterm_core::scheme::tmux_styles(&self.config.borrow().appearance.resolved());
        let ctl: hashterm_tmux::TmuxController = (*self.ctl).clone();
        std::thread::spawn(move || {
            if let Err(e) = ctl.set_window_styles(&styles) {
                tracing::debug!("tmux styles not applied: {e}");
            }
        });
    }

    /// Follow the desktop's light/dark preference (GNOME `color-scheme`).
    fn wire_system_scheme(self: &Rc<Self>) {
        use gtk4::gio::prelude::*;
        let Some(src) = gtk4::gio::SettingsSchemaSource::default() else {
            return;
        };
        if src.lookup("org.gnome.desktop.interface", true).is_none() {
            return;
        }
        let settings = gtk4::gio::Settings::new("org.gnome.desktop.interface");
        let read = |s: &gtk4::gio::Settings| {
            hashterm_core::scheme::set_system_dark(Some(hashterm_core::scheme::dark_from_gnome(
                s.string("color-scheme").as_str(),
            )));
        };
        read(&settings);
        let weak = Rc::downgrade(self);
        settings.connect_changed(Some("color-scheme"), move |s, _| {
            read(s);
            if let Some(win) = weak.upgrade()
                && win.config.borrow().appearance.follow_system
            {
                win.apply_appearance_only();
            }
        });
        // Keep the Settings object alive for the process lifetime.
        std::mem::forget(settings);
    }

    // ---- guidance notices ------------------------------------------------

    fn accel_label(&self, accel: &str) -> String {
        gtk4::accelerator_parse(hashterm_core::config::normalize_accel(accel))
            .map(|(k, m)| gtk4::accelerator_get_label(k, m).to_string())
            .unwrap_or_default()
    }

    fn hint_seen(id: &str) -> bool {
        hashterm_core::state::UiState::load()
            .hints_seen
            .iter()
            .any(|h| h == id)
    }

    fn mark_hint_seen(id: &str) {
        let mut st = hashterm_core::state::UiState::load();
        if !st.hints_seen.iter().any(|h| h == id) {
            st.hints_seen.push(id.to_string());
            st.save();
        }
    }

    /// Show the welcome strip once, when the console is actually on screen.
    fn schedule_first_run_hint(self: &Rc<Self>) {
        if Self::hint_seen("welcome") {
            return;
        }
        let this = Rc::downgrade(self);
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(900), move || {
            let Some(win) = this.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            if !win.window.is_visible() || win.tabs.borrow().is_empty() {
                return gtk4::glib::ControlFlow::Continue; // still hidden or empty
            }
            // Seen the moment it is displayed: it never nags on later launches.
            Self::mark_hint_seen("welcome");
            let (hk, dropdown) = {
                let c = win.config.borrow();
                (c.hotkeys.clone(), c.dropdown.enabled)
            };
            let text = crate::notice::welcome_text(
                dropdown,
                &win.accel_label(&hk.toggle_dropdown),
                &win.accel_label(&hk.new_tab),
                &win.accel_label(&hk.new_group),
                &win.accel_label(&hk.settings),
            );
            win.notice
                .show(&text, vec![("Got it".into(), Rc::new(|| {}))]);
            win.notice.hide_after(25);
            gtk4::glib::ControlFlow::Break
        });
    }

    /// The global hotkey reported a state (possibly late, from the portal).
    pub fn set_hotkey_status(self: &Rc<Self>, status: hashterm_platform::HotkeyStatus) {
        // A desktop-level shortcut that runs `hashterm --toggle` is a working
        // setup, not a problem: report it instead of nagging.
        let status = if status.needs_attention() {
            match hashterm_platform::external_toggle_binding() {
                Some(trigger) => hashterm_platform::HotkeyStatus::External { trigger },
                None => status,
            }
        } else {
            status
        };
        tracing::info!("global hotkey: {}", status.describe());
        *self.hotkey_status.borrow_mut() = status.clone();
        if status.needs_attention() && !Self::hint_seen("hotkey") {
            let this = Rc::downgrade(self);
            let weak_win = Rc::downgrade(self);
            let copy: crate::notice::Action = Rc::new(move || {
                if let Some(win) = weak_win.upgrade() {
                    win.copy_toggle_command();
                }
            });
            let dismiss: crate::notice::Action = Rc::new(|| Self::mark_hint_seen("hotkey"));
            // Let the first-run strip (if any) finish first: retry while a
            // notice is up (it hides itself after 25 s).
            let tries = std::cell::Cell::new(0u32);
            gtk4::glib::timeout_add_local(std::time::Duration::from_secs(3), move || {
                let Some(win) = this.upgrade() else {
                    return gtk4::glib::ControlFlow::Break;
                };
                // A drop-down that starts hidden is the normal case: keep
                // waiting for it to appear (bounded: about two minutes).
                if tries.get() >= 40 {
                    return gtk4::glib::ControlFlow::Break;
                }
                if !win.window.is_visible() || win.notice.is_shown() {
                    tries.set(tries.get() + 1);
                    return gtk4::glib::ControlFlow::Continue;
                }
                win.notice.show(
                    &format!("Global hotkey: {}", status.describe()),
                    vec![
                        ("Copy command".into(), copy.clone()),
                        ("Don't remind me".into(), dismiss.clone()),
                    ],
                );
                gtk4::glib::ControlFlow::Break
            });
        }
    }

    pub fn hotkey_status(&self) -> hashterm_platform::HotkeyStatus {
        self.hotkey_status.borrow().clone()
    }

    /// Put `hashterm --toggle` (the always-works fallback) on the clipboard.
    pub fn copy_toggle_command(&self) {
        self.window.clipboard().set_text("hashterm --toggle");
    }

    /// The live configuration (shared with the config watcher).
    pub fn config(&self) -> Rc<RefCell<Config>> {
        self.config.clone()
    }

    pub fn gtk_window(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}

/// The group bar's edge: the configured one, unless it is parallel to the
/// tab bar — the matrix needs perpendicular axes, so a parallel setting (or
/// none) resolves to the default perpendicular edge.
/// A bar docked on a side edge must not claim spare width (and a top/bottom
/// bar not spare height). GTK propagates a child's expand flag upward - the
/// row labels are hexpand - so without an explicit flag the grid column of a
/// left/right bar competes with the terminal column for the window's extra
/// width and the bar balloons to a third of the window.
fn pin_bar_expand(bar: &impl IsA<gtk4::Widget>, edge: Edge) {
    let side = matches!(edge, Edge::Left | Edge::Right);
    bar.set_hexpand(!side);
    bar.set_vexpand(side);
}

/// `names` with `name` moved to `index` (clamped); None when it is unknown or
/// already there.
fn reordered(names: &[String], name: &str, index: usize) -> Option<Vec<String>> {
    let from = names.iter().position(|n| n == name)?;
    let mut out = names.to_vec();
    let moved = out.remove(from);
    let at = index.min(out.len());
    if at == from {
        return None;
    }
    out.insert(at, moved);
    Some(out)
}

fn resolved_groupbar_edge(cfg: &Config) -> Edge {
    let tab_edge = cfg.tabbar.edge;
    let horizontal = |e: Edge| matches!(e, Edge::Top | Edge::Bottom);
    let fallback = if horizontal(tab_edge) {
        Edge::Left
    } else {
        Edge::Top
    };
    match cfg.groupbar.edge {
        Some(e) if horizontal(e) != horizontal(tab_edge) => e,
        Some(e) => {
            tracing::warn!(
                "groupbar.edge {e:?} is parallel to tabbar.edge {tab_edge:?}; using {fallback:?}"
            );
            fallback
        }
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::{reordered, resolved_groupbar_edge};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorder_moves_and_clamps() {
        let g = names(&["a", "b", "c"]);
        assert_eq!(reordered(&g, "a", 1), Some(names(&["b", "a", "c"])));
        assert_eq!(reordered(&g, "a", 99), Some(names(&["b", "c", "a"])));
        assert_eq!(reordered(&g, "c", 0), Some(names(&["c", "a", "b"])));
    }

    #[test]
    fn reorder_noop_and_unknown() {
        let g = names(&["a", "b", "c"]);
        assert_eq!(reordered(&g, "b", 1), None);
        assert_eq!(reordered(&g, "zzz", 0), None);
    }
    use hashterm_core::config::{Config, Edge};

    fn cfg(tab: Edge, group: Option<Edge>) -> Config {
        let mut c = Config::default();
        c.tabbar.edge = tab;
        c.groupbar.edge = group;
        c
    }

    #[test]
    fn auto_edge_is_perpendicular() {
        assert_eq!(resolved_groupbar_edge(&cfg(Edge::Top, None)), Edge::Left);
        assert_eq!(resolved_groupbar_edge(&cfg(Edge::Bottom, None)), Edge::Left);
        assert_eq!(resolved_groupbar_edge(&cfg(Edge::Left, None)), Edge::Top);
        assert_eq!(resolved_groupbar_edge(&cfg(Edge::Right, None)), Edge::Top);
    }

    #[test]
    fn explicit_perpendicular_edge_wins() {
        assert_eq!(
            resolved_groupbar_edge(&cfg(Edge::Top, Some(Edge::Right))),
            Edge::Right
        );
        assert_eq!(
            resolved_groupbar_edge(&cfg(Edge::Left, Some(Edge::Bottom))),
            Edge::Bottom
        );
    }

    #[test]
    fn parallel_edge_falls_back() {
        assert_eq!(
            resolved_groupbar_edge(&cfg(Edge::Top, Some(Edge::Bottom))),
            Edge::Left
        );
        assert_eq!(
            resolved_groupbar_edge(&cfg(Edge::Top, Some(Edge::Top))),
            Edge::Left
        );
        assert_eq!(
            resolved_groupbar_edge(&cfg(Edge::Left, Some(Edge::Right))),
            Edge::Top
        );
    }
}
