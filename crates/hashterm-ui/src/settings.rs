//! The Settings window: every option of config.toml with live preview.
//!
//! Design: instant apply, no OK/Cancel. A change is queued, written to
//! config.toml ~0.4 s later (format-preserving, comments kept — see
//! `hashterm_core::settings`), and the app's existing config watcher applies it
//! live, exactly as if the file had been edited by hand. A row shows a reset
//! button whenever its value differs from the built-in default; resetting
//! removes the key from the file again. Nothing here talks to the running
//! window directly, so the file stays the single source of truth.

use gtk4::prelude::*;
use hashterm_core::config::{Config, normalize_accel, parse_size};
use hashterm_core::settings::{Edit, Setting, save_edits};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::window::MainWindow;

// ---- schema -----------------------------------------------------------

enum Kind {
    Bool,
    /// (stored value, label). A stored value of "" means "key absent".
    Choice(&'static [(&'static str, &'static str)]),
    Int {
        min: i64,
        max: i64,
        step: i64,
    },
    /// 0..1 shown as a percentage slider.
    Fraction {
        min: f64,
        max: f64,
    },
    Color,
    Font,
    /// A GTK accelerator such as `<Ctrl><Shift>t`.
    Accel,
    /// Free text (optional when `empty_unsets`).
    Text {
        placeholder: &'static str,
    },
    /// "600px" / "45%"; empty removes the key.
    Size,
}

struct Field {
    section: &'static str,
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    kind: Kind,
    /// Only takes effect after restarting hashterm.
    restart: bool,
}

struct Group {
    title: &'static str,
    fields: &'static [Field],
}

struct Page {
    id: &'static str,
    title: &'static str,
    groups: &'static [Group],
}

const EDGES: &[(&str, &str)] = &[
    ("top", "Top"),
    ("bottom", "Bottom"),
    ("left", "Left"),
    ("right", "Right"),
];
const EDGES_AUTO: &[(&str, &str)] = &[
    ("", "Automatic"),
    ("top", "Top"),
    ("bottom", "Bottom"),
    ("left", "Left"),
    ("right", "Right"),
];

macro_rules! f {
    ($s:expr, $k:expr, $l:expr, $h:expr, $kind:expr) => {
        Field {
            section: $s,
            key: $k,
            label: $l,
            hint: $h,
            kind: $kind,
            restart: false,
        }
    };
    ($s:expr, $k:expr, $l:expr, $h:expr, $kind:expr, restart) => {
        Field {
            section: $s,
            key: $k,
            label: $l,
            hint: $h,
            kind: $kind,
            restart: true,
        }
    };
}

const PAGES: &[Page] = &[
    Page {
        id: "general",
        title: "General",
        groups: &[
            Group {
                title: "Tabs",
                fields: &[
                    f!(
                        "general",
                        "inherit_cwd",
                        "New tabs start in the current directory",
                        "Otherwise they start in your home directory.",
                        Kind::Bool
                    ),
                    f!(
                        "general",
                        "close_kills_session",
                        "Closing a tab ends its session",
                        "Off: the session keeps running detached and comes back as a tab next start.",
                        Kind::Bool
                    ),
                ],
            },
            Group {
                title: "Notifications",
                fields: &[
                    f!(
                        "notifications",
                        "enabled",
                        "Desktop notifications",
                        "Tabs and groups always light up; this adds a desktop notification.",
                        Kind::Bool
                    ),
                    f!(
                        "notifications",
                        "on_bell",
                        "Notify on a terminal bell",
                        "",
                        Kind::Bool
                    ),
                    f!(
                        "notifications",
                        "on_activity",
                        "Notify on output in a background tab",
                        "After 5 seconds of silence. Can be noisy.",
                        Kind::Bool
                    ),
                    f!(
                        "notifications",
                        "only_when_unseen",
                        "Skip the tab I am looking at",
                        "",
                        Kind::Bool
                    ),
                ],
            },
            Group {
                title: "Links",
                fields: &[f!(
                    "links",
                    "enabled",
                    "Ctrl+click opens links",
                    "http, https and mailto, including hyperlinks programs print.",
                    Kind::Bool
                )],
            },
            Group {
                title: "Startup",
                fields: &[
                    f!(
                        "general",
                        "start_hidden",
                        "Start hidden",
                        "Launch without showing the console; the toggle key summons it.",
                        Kind::Bool
                    ),
                    f!(
                        "general",
                        "autostart",
                        "Start with my desktop session",
                        "Keeps an autostart entry in sync with this switch.",
                        Kind::Bool
                    ),
                    f!(
                        "general",
                        "log_to_file",
                        "Keep a log file for bug reports",
                        "~/.local/state/hashterm/hashterm.log, rotated at 2 MiB.",
                        Kind::Bool
                    ),
                ],
            },
        ],
    },
    Page {
        id: "appearance",
        title: "Appearance",
        groups: &[
            Group {
                title: "Text and colors",
                fields: &[
                    f!(
                        "appearance",
                        "font",
                        "Font",
                        "A monospace font works best.",
                        Kind::Font
                    ),
                    f!(
                        "appearance",
                        "scheme",
                        "Color scheme",
                        "Default uses the text and background colors below.",
                        Kind::Choice(&[
                            ("default", "Default"),
                            ("tomorrow-night", "Tomorrow Night"),
                            ("dracula", "Dracula"),
                            ("nord", "Nord"),
                            ("solarized-dark", "Solarized Dark"),
                            ("solarized-light", "Solarized Light"),
                            ("gruvbox-dark", "Gruvbox Dark"),
                            ("gruvbox-light", "Gruvbox Light"),
                            ("catppuccin-mocha", "Catppuccin Mocha"),
                            ("catppuccin-latte", "Catppuccin Latte"),
                            ("tokyo-night", "Tokyo Night"),
                            ("one-half-dark", "One Half Dark"),
                            ("atom-one-light", "Atom One Light")
                        ])
                    ),
                    f!(
                        "appearance",
                        "follow_system",
                        "Follow the desktop's light/dark setting",
                        "Uses the light and dark schemes below instead of the one above.",
                        Kind::Bool
                    ),
                    f!(
                        "appearance",
                        "scheme_dark",
                        "Scheme for dark mode",
                        "",
                        Kind::Choice(&[
                            ("tomorrow-night", "Tomorrow Night"),
                            ("dracula", "Dracula"),
                            ("nord", "Nord"),
                            ("solarized-dark", "Solarized Dark"),
                            ("solarized-light", "Solarized Light"),
                            ("gruvbox-dark", "Gruvbox Dark"),
                            ("gruvbox-light", "Gruvbox Light"),
                            ("catppuccin-mocha", "Catppuccin Mocha"),
                            ("catppuccin-latte", "Catppuccin Latte"),
                            ("tokyo-night", "Tokyo Night"),
                            ("one-half-dark", "One Half Dark"),
                            ("atom-one-light", "Atom One Light")
                        ])
                    ),
                    f!(
                        "appearance",
                        "scheme_light",
                        "Scheme for light mode",
                        "",
                        Kind::Choice(&[
                            ("tomorrow-night", "Tomorrow Night"),
                            ("dracula", "Dracula"),
                            ("nord", "Nord"),
                            ("solarized-dark", "Solarized Dark"),
                            ("solarized-light", "Solarized Light"),
                            ("gruvbox-dark", "Gruvbox Dark"),
                            ("gruvbox-light", "Gruvbox Light"),
                            ("catppuccin-mocha", "Catppuccin Mocha"),
                            ("catppuccin-latte", "Catppuccin Latte"),
                            ("tokyo-night", "Tokyo Night"),
                            ("one-half-dark", "One Half Dark"),
                            ("atom-one-light", "Atom One Light")
                        ])
                    ),
                    f!("appearance", "foreground", "Text color", "", Kind::Color),
                    f!(
                        "appearance",
                        "background",
                        "Background color",
                        "",
                        Kind::Color
                    ),
                    f!(
                        "appearance",
                        "cursor_shape",
                        "Cursor shape",
                        "",
                        Kind::Choice(&[
                            ("block", "Block"),
                            ("ibeam", "Bar"),
                            ("underline", "Underline")
                        ])
                    ),
                    f!(
                        "appearance",
                        "cursor_blink",
                        "Blinking cursor",
                        "",
                        Kind::Bool
                    ),
                    f!(
                        "appearance",
                        "bold_is_bright",
                        "Bold text uses bright colors",
                        "",
                        Kind::Bool
                    ),
                    f!(
                        "appearance",
                        "padding",
                        "Padding",
                        "Space around the terminal text, in pixels.",
                        Kind::Int {
                            min: 0,
                            max: 40,
                            step: 1
                        }
                    ),
                ],
            },
            Group {
                title: "Transparency",
                fields: &[
                    f!(
                        "appearance",
                        "opacity",
                        "Terminal background",
                        "Text stays fully opaque. Needs a compositor on X11.",
                        Kind::Fraction {
                            min: 0.05,
                            max: 1.0
                        }
                    ),
                    f!(
                        "appearance",
                        "tabbar_opacity",
                        "Tab bar background",
                        "",
                        Kind::Fraction { min: 0.0, max: 1.0 }
                    ),
                    f!(
                        "appearance",
                        "window_opacity",
                        "Whole window",
                        "Fades everything, text included. Also: Ctrl+Alt+Shift + scroll.",
                        Kind::Fraction { min: 0.2, max: 1.0 }
                    ),
                    f!(
                        "appearance",
                        "tab_border_width",
                        "Accent ring width",
                        "Border drawn around the terminal for tabs with an accent color; 0 turns it off.",
                        Kind::Int {
                            min: 0,
                            max: 8,
                            step: 1
                        }
                    ),
                ],
            },
        ],
    },
    Page {
        id: "tabs",
        title: "Tabs and groups",
        groups: &[
            Group {
                title: "Tab bar",
                fields: &[
                    f!("tabbar", "edge", "Position", "", Kind::Choice(EDGES)),
                    f!(
                        "tabbar",
                        "auto_hide",
                        "Hide until I point at the edge",
                        "The bar slides in over the terminal. Ctrl+Shift+B toggles it.",
                        Kind::Bool
                    ),
                    f!("tabbar", "show_close", "Show close buttons", "", Kind::Bool),
                    f!(
                        "tabbar",
                        "reveal_on_badge",
                        "Reveal when a tab needs attention",
                        "Bell or activity in a tab keeps a hidden bar visible.",
                        Kind::Bool
                    ),
                    f!(
                        "tabbar",
                        "switch_osd",
                        "Show the tab-switch popup",
                        "The dial that appears when you change tabs.",
                        Kind::Bool
                    ),
                    f!(
                        "tabbar",
                        "auto_titles",
                        "Name tabs after the folder or running program",
                        "Off: tabs show the host name until a program sets a title.",
                        Kind::Bool
                    ),
                    f!(
                        "tabbar",
                        "reveal_ms",
                        "Slide-in time",
                        "Milliseconds.",
                        Kind::Int {
                            min: 0,
                            max: 1000,
                            step: 10
                        }
                    ),
                ],
            },
            Group {
                title: "Groups",
                fields: &[
                    f!(
                        "groupbar",
                        "edge",
                        "Position",
                        "Automatic puts it on the edge across from the tab bar.",
                        Kind::Choice(EDGES_AUTO)
                    ),
                    f!(
                        "groupbar",
                        "hide_when_single",
                        "Hide while there is only one group",
                        "",
                        Kind::Bool
                    ),
                    f!("groupbar", "show_counts", "Show tab counts", "", Kind::Bool),
                    f!(
                        "groupbar",
                        "reorderable",
                        "Drag groups to reorder them",
                        "",
                        Kind::Bool
                    ),
                ],
            },
        ],
    },
    Page {
        id: "window",
        title: "Window",
        groups: &[
            Group {
                title: "Drop-down console",
                fields: &[
                    f!(
                        "dropdown",
                        "enabled",
                        "Drop-down mode",
                        "Off: a normal window that the toggle key shows and hides.",
                        Kind::Bool,
                        restart
                    ),
                    f!(
                        "dropdown",
                        "edge",
                        "Slides in from",
                        "",
                        Kind::Choice(EDGES)
                    ),
                    f!(
                        "dropdown",
                        "width_pct",
                        "Width",
                        "Share of the screen.",
                        Kind::Fraction { min: 0.2, max: 1.0 }
                    ),
                    f!(
                        "dropdown",
                        "height_pct",
                        "Height",
                        "Share of the screen, until you resize it yourself.",
                        Kind::Fraction { min: 0.1, max: 1.0 }
                    ),
                    f!(
                        "dropdown",
                        "height",
                        "Fixed height",
                        "Pins the height (\"600px\" or \"45%\") and overrides the remembered one.",
                        Kind::Size
                    ),
                    f!(
                        "dropdown",
                        "animation_ms",
                        "Slide time",
                        "Milliseconds.",
                        Kind::Int {
                            min: 0,
                            max: 1000,
                            step: 10
                        }
                    ),
                    f!(
                        "dropdown",
                        "hide_on_focus_loss",
                        "Hide when it loses focus",
                        "The classic quake feel.",
                        Kind::Bool
                    ),
                ],
            },
            Group {
                title: "Remember my window",
                fields: &[
                    f!(
                        "window",
                        "remember_size",
                        "Size",
                        "Restore the size you last resized it to.",
                        Kind::Bool
                    ),
                    f!(
                        "window",
                        "remember_position",
                        "Position",
                        "Needs X11 or XWayland; native Wayland places windows itself.",
                        Kind::Bool
                    ),
                ],
            },
            Group {
                title: "Platform",
                fields: &[
                    f!(
                        "dropdown",
                        "layer",
                        "Layer (Wayland layer-shell)",
                        "\"Overlay\" also covers fullscreen windows.",
                        Kind::Choice(&[("top", "Top"), ("overlay", "Overlay")]),
                        restart
                    ),
                    f!(
                        "dropdown",
                        "respect_panels",
                        "Keep clear of panels and docks",
                        "",
                        Kind::Bool,
                        restart
                    ),
                    f!(
                        "dropdown",
                        "prefer_xwayland",
                        "Use XWayland on GNOME",
                        "Lets the console stay on top; costs some sharpness with fractional scaling.",
                        Kind::Bool,
                        restart
                    ),
                ],
            },
        ],
    },
    Page {
        id: "sessions",
        title: "Sessions",
        groups: &[
            Group {
                title: "Saving",
                fields: &[
                    f!(
                        "session",
                        "autosave_interval_secs",
                        "Autosave every",
                        "Seconds; 0 turns autosave off.",
                        Kind::Int {
                            min: 0,
                            max: 3600,
                            step: 30
                        }
                    ),
                    f!(
                        "session",
                        "autosave_keep",
                        "Autosaves to keep",
                        "",
                        Kind::Int {
                            min: 1,
                            max: 100,
                            step: 1
                        }
                    ),
                    f!(
                        "scrollback",
                        "lines",
                        "Scrollback lines",
                        "Applies to new panes.",
                        Kind::Int {
                            min: 1000,
                            max: 1_000_000,
                            step: 1000
                        }
                    ),
                ],
            },
            Group {
                title: "Restoring",
                fields: &[
                    f!(
                        "session",
                        "prompt_on_start",
                        "Offer to restore after a crash or reboot",
                        "",
                        Kind::Bool
                    ),
                    f!(
                        "session",
                        "restore_on_start",
                        "Always restore the last save silently",
                        "",
                        Kind::Bool
                    ),
                    f!(
                        "restore",
                        "pretype_unrestored",
                        "Pre-type commands that weren't restarted",
                        "Press Enter to re-run what was running.",
                        Kind::Bool
                    ),
                    f!(
                        "tmux",
                        "native_windows",
                        "Use tmux's own window keys inside a tab",
                        "Brings back tmux's status line.",
                        Kind::Bool,
                        restart
                    ),
                ],
            },
        ],
    },
    Page {
        id: "keys",
        title: "Keyboard",
        groups: &[
            Group {
                title: "Global",
                fields: &[f!(
                    "hotkeys",
                    "toggle_dropdown",
                    "Show / hide the console",
                    "System-wide. e.g. F12 or <Ctrl>asciitilde",
                    Kind::Accel,
                    restart
                )],
            },
            Group {
                title: "Tabs",
                fields: &[
                    f!("hotkeys", "new_tab", "New tab", "", Kind::Accel),
                    f!("hotkeys", "close_tab", "Close tab", "", Kind::Accel),
                    f!("hotkeys", "next_tab", "Next tab", "", Kind::Accel),
                    f!("hotkeys", "prev_tab", "Previous tab", "", Kind::Accel),
                    f!(
                        "hotkeys",
                        "goto_tab_modifier",
                        "Jump to tab 1-10",
                        "Modifier + 1..9, 0, e.g. <Alt>. Empty turns it off.",
                        Kind::Text {
                            placeholder: "<Alt>"
                        }
                    ),
                    f!(
                        "hotkeys",
                        "toggle_tabbar",
                        "Show / hide the tab bar",
                        "",
                        Kind::Accel
                    ),
                ],
            },
            Group {
                title: "Groups",
                fields: &[
                    f!("hotkeys", "next_group", "Next group", "", Kind::Accel),
                    f!("hotkeys", "prev_group", "Previous group", "", Kind::Accel),
                    f!("hotkeys", "new_group", "New group", "", Kind::Accel),
                    f!("hotkeys", "split_right", "Split right", "", Kind::Accel),
                    f!("hotkeys", "split_down", "Split down", "", Kind::Accel),
                    f!("hotkeys", "pane_left", "Focus pane left", "", Kind::Accel),
                    f!("hotkeys", "pane_right", "Focus pane right", "", Kind::Accel),
                    f!("hotkeys", "pane_up", "Focus pane up", "", Kind::Accel),
                    f!("hotkeys", "pane_down", "Focus pane down", "", Kind::Accel),
                    f!("hotkeys", "zoom_pane", "Zoom pane", "", Kind::Accel),
                    f!("hotkeys", "search", "Find in scrollback", "", Kind::Accel),
                    f!("hotkeys", "zoom_in", "Larger text", "", Kind::Accel),
                    f!("hotkeys", "zoom_out", "Smaller text", "", Kind::Accel),
                    f!("hotkeys", "zoom_reset", "Reset text size", "", Kind::Accel),
                ],
            },
            Group {
                title: "Other",
                fields: &[
                    f!(
                        "hotkeys",
                        "session_picker",
                        "Save / restore sessions",
                        "",
                        Kind::Accel
                    ),
                    f!("hotkeys", "copy", "Copy", "", Kind::Accel),
                    f!(
                        "hotkeys",
                        "paste",
                        "Paste",
                        "Shift+Insert always pastes too.",
                        Kind::Accel
                    ),
                    f!("hotkeys", "settings", "Open Settings", "", Kind::Accel),
                    f!(
                        "hotkeys",
                        "open_config",
                        "Open config.toml in an editor",
                        "",
                        Kind::Accel
                    ),
                ],
            },
        ],
    },
];

// ---- value plumbing ----------------------------------------------------

fn lookup(root: &toml::Value, f: &Field) -> Option<toml::Value> {
    root.get(f.section)?.get(f.key).cloned()
}

fn same_as_default(now: &Option<Setting>, default: &Option<toml::Value>) -> bool {
    match (now, default) {
        (None, None) => true,
        (Some(Setting::Bool(a)), Some(toml::Value::Boolean(b))) => a == b,
        (Some(Setting::Int(a)), Some(toml::Value::Integer(b))) => a == b,
        (Some(Setting::Float(a)), Some(toml::Value::Float(b))) => (a - b).abs() < 0.0005,
        (Some(Setting::Str(a)), Some(toml::Value::String(b))) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn rgba_to_hex(c: &gtk4::gdk::RGBA) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.red() * 255.0).round() as u8,
        (c.green() * 255.0).round() as u8,
        (c.blue() * 255.0).round() as u8
    )
}

// ---- shared state ------------------------------------------------------

struct Ctx {
    defaults: toml::Value,
    current: toml::Value,
    /// Programmatic control updates must not be written back.
    updating: Cell<bool>,
    pending: RefCell<Vec<Edit>>,
    timer: Cell<Option<gtk4::glib::SourceId>>,
    status: gtk4::Label,
    win: Rc<MainWindow>,
}

impl Ctx {
    fn queue(self: &Rc<Self>, edit: Edit) {
        {
            let mut p = self.pending.borrow_mut();
            p.retain(|e| !(e.section == edit.section && e.key == edit.key));
            p.push(edit);
        }
        self.status.remove_css_class("error");
        self.status.set_text("Saving…");
        if let Some(id) = self.timer.take() {
            id.remove();
        }
        let this = Rc::downgrade(self);
        let id =
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                if let Some(ctx) = this.upgrade() {
                    ctx.timer.set(None);
                    ctx.flush();
                }
            });
        self.timer.set(Some(id));
    }

    fn flush(&self) {
        let edits: Vec<Edit> = self.pending.borrow_mut().drain(..).collect();
        if edits.is_empty() {
            return;
        }
        match save_edits(&Config::default_path(), &edits) {
            Ok(_) => {
                self.status.remove_css_class("error");
                self.status.set_text("Saved to config.toml");
            }
            Err(e) => {
                tracing::error!("settings: {e}");
                self.status.add_css_class("error");
                self.status.set_text(&format!("Not saved: {e}"));
            }
        }
    }
}

// ---- rows --------------------------------------------------------------

fn build_row(ctx: &Rc<Ctx>, f: &'static Field) -> gtk4::ListBoxRow {
    let default = lookup(&ctx.defaults, f);
    let current = lookup(&ctx.current, f);

    let reset = gtk4::Button::from_icon_name("edit-undo-symbolic");
    reset.add_css_class("flat");
    reset.set_valign(gtk4::Align::Center);
    reset.set_tooltip_text(Some("Reset to default"));
    reset.set_visible(false);

    // emit: user changed the control; set: program updates the control.
    let emit: Rc<dyn Fn(Option<Setting>)> = {
        let ctx = ctx.clone();
        let reset = reset.clone();
        let default = default.clone();
        Rc::new(move |value: Option<Setting>| {
            if ctx.updating.get() {
                return;
            }
            reset.set_visible(!same_as_default(&value, &default));
            ctx.queue(match value {
                Some(v) => Edit::set(f.section, f.key, v),
                None => Edit::unset(f.section, f.key),
            });
        })
    };

    type SetFn = Rc<dyn Fn(Option<&toml::Value>)>;
    let (control, set): (gtk4::Widget, SetFn) = match &f.kind {
        Kind::Bool => {
            let sw = gtk4::Switch::new();
            sw.set_valign(gtk4::Align::Center);
            let emit = emit.clone();
            sw.connect_active_notify(move |s| emit(Some(Setting::Bool(s.is_active()))));
            let w = sw.clone();
            (
                sw.upcast(),
                Rc::new(move |v| w.set_active(v.and_then(|v| v.as_bool()).unwrap_or(false))),
            )
        }
        Kind::Choice(opts) => {
            let labels: Vec<&str> = opts.iter().map(|(_, l)| *l).collect();
            let dd = gtk4::DropDown::from_strings(&labels);
            dd.set_valign(gtk4::Align::Center);
            let emit = emit.clone();
            dd.connect_selected_notify(move |d| {
                let (value, _) = opts[d.selected() as usize];
                emit(if value.is_empty() {
                    None
                } else {
                    Some(Setting::Str(value.into()))
                });
            });
            let w = dd.clone();
            (
                dd.upcast(),
                Rc::new(move |v| {
                    let want = v.and_then(|v| v.as_str()).unwrap_or("");
                    let idx = opts.iter().position(|(val, _)| *val == want).unwrap_or(0);
                    w.set_selected(idx as u32);
                }),
            )
        }
        Kind::Int { min, max, step } => {
            let spin = gtk4::SpinButton::with_range(*min as f64, *max as f64, *step as f64);
            spin.set_valign(gtk4::Align::Center);
            let emit = emit.clone();
            spin.connect_value_changed(move |s| emit(Some(Setting::Int(s.value() as i64))));
            let w = spin.clone();
            (
                spin.upcast(),
                Rc::new(move |v| w.set_value(v.and_then(|v| v.as_integer()).unwrap_or(0) as f64)),
            )
        }
        Kind::Fraction { min, max } => {
            let scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, *min, *max, 0.01);
            scale.set_width_request(220);
            scale.set_valign(gtk4::Align::Center);
            scale.set_draw_value(true);
            scale.set_value_pos(gtk4::PositionType::Right);
            scale.set_format_value_func(|_, v| format!("{:.0}%", v * 100.0));
            let emit = emit.clone();
            scale.connect_value_changed(move |s| emit(Some(Setting::Float(s.value()))));
            let w = scale.clone();
            (
                scale.upcast(),
                Rc::new(move |v| w.set_value(v.and_then(|v| v.as_float()).unwrap_or(1.0))),
            )
        }
        Kind::Color => {
            let btn = gtk4::ColorDialogButton::new(Some(gtk4::ColorDialog::new()));
            btn.set_valign(gtk4::Align::Center);
            let emit = emit.clone();
            btn.connect_rgba_notify(move |b| emit(Some(Setting::Str(rgba_to_hex(&b.rgba())))));
            let w = btn.clone();
            (
                btn.upcast(),
                Rc::new(move |v| {
                    if let Some(c) = v
                        .and_then(|v| v.as_str())
                        .and_then(|s| gtk4::gdk::RGBA::parse(s).ok())
                    {
                        w.set_rgba(&c);
                    }
                }),
            )
        }
        Kind::Font => {
            let btn = gtk4::FontDialogButton::new(Some(gtk4::FontDialog::new()));
            btn.set_valign(gtk4::Align::Center);
            btn.set_level(gtk4::FontLevel::Font);
            let emit = emit.clone();
            btn.connect_font_desc_notify(move |b| {
                if let Some(d) = b.font_desc() {
                    emit(Some(Setting::Str(d.to_string())));
                }
            });
            let w = btn.clone();
            (
                btn.upcast(),
                Rc::new(move |v| {
                    if let Some(s) = v.and_then(|v| v.as_str()) {
                        w.set_font_desc(&gtk4::pango::FontDescription::from_string(s));
                    }
                }),
            )
        }
        Kind::Accel | Kind::Text { .. } | Kind::Size => {
            let placeholder = match &f.kind {
                Kind::Text { placeholder } => *placeholder,
                Kind::Size => "e.g. 600px or 45%",
                _ => "",
            };
            let entry = gtk4::Entry::builder()
                .width_chars(22)
                .valign(gtk4::Align::Center)
                .placeholder_text(placeholder)
                .build();
            let commit = {
                let entry = entry.clone();
                let emit = emit.clone();
                let is_accel = matches!(f.kind, Kind::Accel);
                let is_size = matches!(f.kind, Kind::Size);
                move || {
                    let text = entry.text().trim().to_string();
                    let ok = if is_accel {
                        gtk4::accelerator_parse(normalize_accel(&text)).is_some()
                    } else if is_size {
                        text.is_empty() || parse_size(&text).is_some()
                    } else {
                        true
                    };
                    if !ok {
                        entry.add_css_class("error");
                        return;
                    }
                    entry.remove_css_class("error");
                    emit(if is_size && text.is_empty() {
                        None
                    } else {
                        Some(Setting::Str(text))
                    });
                }
            };
            let c = commit.clone();
            entry.connect_activate(move |_| c());
            let focus = gtk4::EventControllerFocus::new();
            let c = commit.clone();
            focus.connect_leave(move |_| c());
            entry.add_controller(focus);
            let w = entry.clone();
            (
                entry.upcast(),
                Rc::new(move |v| {
                    w.remove_css_class("error");
                    w.set_text(v.and_then(|v| v.as_str()).unwrap_or(""));
                }),
            )
        }
    };

    // Initial state, without echoing it back into the file.
    ctx.updating.set(true);
    set(current.as_ref().or(default.as_ref()));
    ctx.updating.set(false);
    let initial = current.as_ref().map(|v| match v {
        toml::Value::Boolean(b) => Setting::Bool(*b),
        toml::Value::Integer(i) => Setting::Int(*i),
        toml::Value::Float(x) => Setting::Float(*x),
        other => Setting::Str(other.as_str().unwrap_or_default().to_owned()),
    });
    reset.set_visible(!same_as_default(&initial, &default));
    {
        let ctx = ctx.clone();
        let default = default.clone();
        let reset2 = reset.clone();
        let set = set.clone();
        reset.connect_clicked(move |_| {
            ctx.updating.set(true);
            set(default.as_ref());
            ctx.updating.set(false);
            reset2.set_visible(false);
            ctx.queue(Edit::unset(f.section, f.key));
        });
    }

    // Layout: text on the left, control + reset on the right.
    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk4::Align::Center);
    let title = gtk4::Label::builder()
        .label(f.label)
        .xalign(0.0)
        .wrap(true)
        .build();
    text.append(&title);
    if !f.hint.is_empty() {
        let hint = gtk4::Label::builder()
            .label(f.hint)
            .xalign(0.0)
            .wrap(true)
            .build();
        hint.add_css_class("dim-label");
        hint.add_css_class("caption");
        text.append(&hint);
    }
    if f.restart {
        let note = gtk4::Label::builder()
            .label("Takes effect after restarting hashterm")
            .xalign(0.0)
            .build();
        note.add_css_class("caption");
        note.add_css_class("settings-restart");
        text.append(&note);
    }
    let right = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    right.set_valign(gtk4::Align::Center);
    right.append(&control);
    right.append(&reset);
    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 16);
    row_box.add_css_class("settings-row");
    row_box.append(&text);
    row_box.append(&right);
    let row = gtk4::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(&row_box));
    row
}

/// Read-only row under the global shortcut: what the desktop actually did
/// with it, and the fallback that always works.
fn hotkey_status_row(ctx: &Rc<Ctx>) -> gtk4::ListBoxRow {
    let enabled = ctx.win.config().borrow().dropdown.enabled;
    let text = if enabled {
        ctx.win.hotkey_status().describe()
    } else {
        "Off: the drop-down is disabled in the config, so there is no global shortcut.".into()
    };
    let title = gtk4::Label::builder()
        .label("Shortcut status")
        .xalign(0.0)
        .build();
    let detail = gtk4::Label::builder()
        .label(&text)
        .xalign(0.0)
        .wrap(true)
        .build();
    detail.add_css_class("dim-label");
    let tb = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    tb.set_hexpand(true);
    tb.append(&title);
    tb.append(&detail);
    let copy = gtk4::Button::with_label("Copy fallback command");
    copy.set_tooltip_text(Some(
        "Bind this command to any key in your desktop's keyboard settings",
    ));
    copy.set_valign(gtk4::Align::Center);
    let win = ctx.win.clone();
    copy.connect_clicked(move |b| {
        win.copy_toggle_command();
        b.set_label("Copied: hashterm --toggle");
    });
    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 16);
    row_box.add_css_class("settings-row");
    row_box.append(&tb);
    row_box.append(&copy);
    let row = gtk4::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_child(Some(&row_box));
    row
}

fn build_page(ctx: &Rc<Ctx>, page: &'static Page) -> gtk4::ScrolledWindow {
    let col = gtk4::Box::new(gtk4::Orientation::Vertical, 22);
    col.set_margin_top(20);
    col.set_margin_bottom(24);
    col.set_margin_start(24);
    col.set_margin_end(24);
    for group in page.groups {
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        let heading = gtk4::Label::builder()
            .label(group.title)
            .xalign(0.0)
            .build();
        heading.add_css_class("heading");
        section.append(&heading);
        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.set_show_separators(false);
        list.add_css_class("settings-list");
        for field in group.fields {
            list.append(&build_row(ctx, field));
        }
        if page.id == "keys" && group.title == "Global" {
            list.append(&hotkey_status_row(ctx));
        }
        section.append(&list);
        col.append(&section);
    }
    gtk4::ScrolledWindow::builder()
        .child(&col)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build()
}

// ---- window ------------------------------------------------------------

thread_local! {
    static OPEN: RefCell<Option<gtk4::glib::WeakRef<gtk4::Window>>> = const { RefCell::new(None) };
}

/// Open the Settings window (or raise it if it is already open).
/// Config keys of every `[hotkeys]` row (the in-app action table is checked
/// against this so a shortcut cannot exist without a Settings row).
#[cfg(test)]
pub(crate) fn hotkey_rows() -> Vec<&'static str> {
    PAGES
        .iter()
        .flat_map(|p| p.groups.iter())
        .flat_map(|g| g.fields.iter())
        .filter(|f| f.section == "hotkeys")
        .map(|f| f.key)
        .collect()
}

pub fn open(win: &Rc<MainWindow>) {
    let existing = OPEN.with(|o| o.borrow().as_ref().and_then(|w| w.upgrade()));
    if let Some(w) = existing {
        w.present();
        return;
    }

    let current = toml::Value::try_from(&*win.config().borrow())
        .unwrap_or(toml::Value::Table(Default::default()));
    let defaults =
        toml::Value::try_from(Config::default()).unwrap_or(toml::Value::Table(Default::default()));

    let status = gtk4::Label::builder()
        .label("Changes apply as you make them")
        .xalign(0.0)
        .hexpand(true)
        .build();
    status.add_css_class("dim-label");
    status.add_css_class("settings-status");
    let ctx = Rc::new(Ctx {
        defaults,
        current,
        updating: Cell::new(false),
        pending: RefCell::new(Vec::new()),
        timer: Cell::new(None),
        status: status.clone(),
        win: win.clone(),
    });

    let stack = gtk4::Stack::builder()
        .transition_type(gtk4::StackTransitionType::Crossfade)
        .transition_duration(100)
        .hexpand(true)
        .vexpand(true)
        .build();
    for page in PAGES {
        stack.add_titled(&build_page(&ctx, page), Some(page.id), page.title);
    }
    let sidebar = gtk4::StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_width_request(190);

    let body = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    body.append(&sidebar);
    body.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    body.append(&stack);

    let edit_file = gtk4::Button::with_label("Edit config.toml…");
    edit_file.set_tooltip_text(Some("Every option, including the ones not on these pages"));
    {
        let win = win.clone();
        edit_file.connect_clicked(move |_| crate::app::open_config_file(&win));
    }
    let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    bar.set_margin_top(8);
    bar.set_margin_bottom(8);
    bar.set_margin_start(16);
    bar.set_margin_end(16);
    bar.append(&status);
    bar.append(&edit_file);

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.append(&body);
    root.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    root.append(&bar);

    let dialog = gtk4::Window::builder()
        .transient_for(win.gtk_window())
        .title("Settings")
        .default_width(900)
        .default_height(760)
        .child(&root)
        .build();
    dialog.add_css_class("settings-window");
    // Esc closes; pending edits are flushed first so nothing is lost.
    let esc = gtk4::ShortcutController::new();
    esc.add_shortcut(gtk4::Shortcut::new(
        gtk4::ShortcutTrigger::parse_string("Escape"),
        Some(gtk4::CallbackAction::new(|w, _| {
            if let Some(win) = w.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) {
                win.close();
            }
            gtk4::glib::Propagation::Stop
        })),
    ));
    dialog.add_controller(esc);
    {
        let ctx = ctx.clone();
        dialog.connect_close_request(move |_| {
            if let Some(id) = ctx.timer.take() {
                id.remove();
            }
            ctx.flush();
            gtk4::glib::Propagation::Proceed
        });
    }
    OPEN.with(|o| *o.borrow_mut() = Some(dialog.downgrade()));
    dialog.present();
}
