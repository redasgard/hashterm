//! Main application window: frameless shell, tab bar on any edge with
//! auto-hide, a GtkStack of terminal pages, and per-tab tmux sessions on the
//! dedicated server. Re-adopts live ht-* sessions at startup.

use gtk4::prelude::*;
use hashterm_core::config::{Config, Edge};
use hashterm_tmux::TmuxController;
use hashterm_tmux::controller::NewSessionOpts;
use std::cell::RefCell;
use std::rc::Rc;

use crate::csd;
use crate::groupbar::{GroupBar, GroupBarEvent};
use crate::page::TerminalPage;
use crate::slide::SlidePanel;
use crate::tabbar::{Badge, TabBar, TabBarEvent, TabInfo};
use hashterm_platform::OverlayBackend;

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
    /// Whole-window opacity set live via Ctrl+Alt+Shift+scroll. Overrides
    /// appearance.window_opacity; an edit of that config value clears it.
    opacity_override: std::cell::Cell<Option<f64>>,
    /// Debounce for persisting the override (scroll emits event storms).
    opacity_save_timer: std::cell::Cell<Option<gtk4::glib::SourceId>>,
    /// Centered fade-out toast shown after tab switches.
    osd_revealer: gtk4::Revealer,
    osd_label: gtk4::Label,
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
        // in/out by a crossfade Revealer.
        let osd_label = gtk4::Label::builder()
            .wrap(true)
            .wrap_mode(gtk4::pango::WrapMode::WordChar)
            .max_width_chars(52)
            .justify(gtk4::Justification::Center)
            .build();
        osd_label.add_css_class("tab-osd");
        let osd_revealer = gtk4::Revealer::builder()
            .child(&osd_label)
            .transition_type(gtk4::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .reveal_child(false)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .can_target(false)
            .build();
        overlay.add_overlay(&osd_revealer);

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
            capture_suppressed: std::cell::Cell::new(false),
            shown_at: std::cell::Cell::new(None),
            last_capture_attempt: std::cell::Cell::new(None),
            suppress_keyvals: RefCell::new(Vec::new()),
            tabbar_pinned: std::cell::Cell::new(false),
            hiding: std::cell::Cell::new(false),
            opacity_override: std::cell::Cell::new(ui_state.window_opacity),
            opacity_save_timer: std::cell::Cell::new(None),
            osd_revealer,
            osd_label,
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
    fn wire_quit_autosave(self: &Rc<Self>) {
        let ctl = (*self.ctl).clone();
        let config = self.config.clone();
        self.window.connect_close_request(move |w| {
            // Remember the height for next start.
            let h = w.height();
            if h > 0 {
                let mut st = hashterm_core::state::UiState::load();
                st.window_height_px = Some(h);
                st.save();
            }
            let cfg = config.borrow();
            if cfg.session.autosave_interval_secs > 0 {
                let store = hashterm_session::SessionStore::new(
                    hashterm_session::SessionStore::default_root(),
                );
                let opts = hashterm_session::DumpOptions {
                    exclude_panes: cfg.restore.exclude_panes.clone(),
                    max_scrollback_lines: cfg.restore.max_scrollback_lines,
                };
                let stamp = hashterm_session::dump::now_stamp();
                match hashterm_session::dump_server(&ctl, &store, &stamp, true, &opts) {
                    Ok(_) => {
                        let _ = store.prune_autosaves(cfg.session.autosave_keep as usize);
                    }
                    Err(hashterm_session::dump::DumpError::Empty) => {}
                    Err(e) => tracing::warn!("quit autosave failed: {e}"),
                }
            }
            gtk4::glib::Propagation::Proceed
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
        let cfg = self.config.borrow();
        let cwd = cwd.or_else(|| {
            if cfg.general.inherit_cwd {
                self.active_entry_cwd()
            } else {
                None
            }
        });
        // New tabs inherit the active group (None for the implicit default).
        let group = {
            let g = self.active_group.borrow().clone();
            (g != hashterm_tmux::controller::DEFAULT_GROUP).then_some(g)
        };
        let opts = NewSessionOpts {
            title: "shell".into(),
            cwd,
            command: command.or_else(|| cfg.general.default_command.clone()),
            group,
        };
        drop(cfg);
        match self.ctl.new_session(&opts) {
            Ok(id) => {
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
            if !self.window.is_active() {
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
        let scroll =
            gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
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

    /// Remember the current height so the next start (or show) restores it.
    fn persist_height(&self) {
        let h = self.window.height();
        if h > 0 {
            let mut st = hashterm_core::state::UiState::load();
            st.window_height_px = Some(h);
            st.save();
        }
    }

    fn hide_dropdown(self: &Rc<Self>, dd: &hashterm_core::config::Dropdown) {
        self.persist_height();
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
        // No explicit height in config -> restore the remembered one.
        let mut dd = dd.clone();
        if dd.height.is_none()
            && let Some(px) = hashterm_core::state::UiState::load().window_height_px
        {
            dd.height = Some(format!("{px}px"));
        }
        let dd = &dd;
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
            if !dd.enabled || !dd.hide_on_focus_loss || !w.is_visible() || !win.held_focus.get() {
                return;
            }
            let this2 = Rc::downgrade(&win);
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
                let Some(win) = this2.upgrade() else { return };
                if !win.window.is_active() && win.window.is_visible() && win.held_focus.get() {
                    win.toggle_dropdown_from("focus-loss");
                }
            });
        });
    }

    pub fn close_tab(self: &Rc<Self>, session: &str) {
        let kill = self.config.borrow().general.close_kills_session;
        if kill && let Err(e) = self.ctl.run(&["kill-session", "-t", session]) {
            tracing::error!("kill-session {session}: {e}");
        }
        self.tabs.borrow_mut().retain(|t| t.session != session);
        if let Some(child) = self.stack.child_by_name(session) {
            self.stack.remove(&child);
        }
        if self.tabs.borrow().is_empty() {
            // No tabs left ANYWHERE -> close; an emptied active group merely
            // switches to a surviving one (validate_active_group).
            self.window.close();
            return;
        }
        self.validate_active_group();
        self.refresh_tabbar();
    }

    pub fn close_active_tab(self: &Rc<Self>) {
        if let Some(name) = self.stack.visible_child_name() {
            self.close_tab(name.as_str());
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
        let group = {
            let mut tabs = self.tabs.borrow_mut();
            let entry = tabs.iter_mut().find(|t| t.session == session);
            if let Some(entry) = entry {
                entry.badge = None;
                entry.page.grab_focus();
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

    /// Centered "name · 2/3" toast, scoped to the active group.
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
        let title = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.session == name.as_str())
            .map(|t| t.title.clone())
            .unwrap_or_default();
        self.show_osd_message(&format!("{title}   ·   {}/{}", i + 1, visible.len()), 900);
    }

    /// Generic centered toast (tab switches, config errors, ...).
    pub fn show_osd_message(self: &Rc<Self>, text: &str, millis: u64) {
        self.osd_label.set_text(text);
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
                crate::tabbar::GroupInfo { name, color, count }
            })
            .collect()
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
        if !self
            .tabs
            .borrow()
            .iter()
            .any(|t| t.group_name() == name)
        {
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
                let name = hashterm_tmux::controller::sanitize_group_name(entry.text().trim());
                dialog.close();
                if name.is_empty() {
                    return;
                }
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
        let mut tabs = self.tabs.borrow_mut();

        // Remove tabs whose session died.
        tabs.retain(|t| {
            let alive = live.iter().any(|s| s.name == t.session);
            if !alive && let Some(child) = self.stack.child_by_name(&t.session) {
                self.stack.remove(&child);
            }
            alive
        });

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
                page,
            });
        }
        drop(tabs);

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

        self.validate_active_group();
        self.refresh_tabbar();
    }

    /// The pane's `tmux attach` process exited (tab detached externally,
    /// session killed, or server died). Drop the page; if the session still
    /// lives on the server, sync re-creates it with a fresh attach.
    pub fn handle_page_exit(self: &Rc<Self>, session: &str) {
        self.tabs.borrow_mut().retain(|t| t.session != session);
        if let Some(child) = self.stack.child_by_name(session) {
            self.stack.remove(&child);
        }
        self.sync_from_server();
        if self.tabs.borrow().is_empty() {
            self.window.close();
        }
    }

    pub fn set_tab_title(self: &Rc<Self>, session: &str, title: String) {
        if let Some(t) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| t.session == session)
        {
            if t.title_custom {
                return; // user-pinned name: automatic titles don't apply
            }
            t.title = title;
        }
        self.refresh_tabbar();
    }

    pub fn set_badge(self: &Rc<Self>, session: &str, badge: Badge) {
        let active = self.stack.visible_child_name().map(|s| s.to_string());
        if active.as_deref() == Some(session) {
            return; // no badges on the tab the user is looking at
        }
        if let Some(t) = self
            .tabs
            .borrow_mut()
            .iter_mut()
            .find(|t| t.session == session)
        {
            t.badge = Some(badge);
        }
        self.refresh_tabbar();
    }

    // ---- config-driven layout ------------------------------------------

    pub fn apply_config(self: &Rc<Self>) {
        let cfg = self.config.borrow().tabbar.clone();
        self.tabbar.apply_config(&cfg);
        self.place_tabbar();
        self.place_groupbar();
        self.apply_window_classes();
        self.tabbar_pinned.set(false);
        self.tabbar.set_revealed(!cfg.auto_hide);
        self.hover_strip.set_visible(cfg.auto_hide);
        let appearance = self.config.borrow().appearance.clone();
        self.window.set_opacity(self.effective_opacity());
        for tab in self.tabs.borrow().iter() {
            tab.page.apply_appearance(&appearance);
        }
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
                TabBarEvent::Close(id) => win.close_tab(&id),
                TabBarEvent::NewTab => win.new_tab(),
                TabBarEvent::Reorder(id, index) => win.reorder_tab(&id, index),
                TabBarEvent::Properties(id) => win.open_tab_properties(&id),
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
        if self.tabs.borrow().is_empty() {
            let restore_latest = self.config.borrow().session.restore_on_start
                && hashterm_session::SessionStore::new(
                    hashterm_session::SessionStore::default_root(),
                )
                .resolve("latest")
                .is_some();
            if restore_latest {
                self.restore_named("latest");
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

    fn refresh_tabbar(self: &Rc<Self>) {
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
        self.groupbar
            .widget()
            .set_reveal_child(!(gb_cfg.hide_when_single && groups.len() < 2));
        self.groupbar
            .set_groups(groups.clone(), active_group.clone(), gb_cfg.show_counts);
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

    pub fn gtk_window(&self) -> &gtk4::ApplicationWindow {
        &self.window
    }
}

/// The group bar's edge: the configured one, unless it is parallel to the
/// tab bar — the matrix needs perpendicular axes, so a parallel setting (or
/// none) resolves to the default perpendicular edge.
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
    use super::resolved_groupbar_edge;
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
