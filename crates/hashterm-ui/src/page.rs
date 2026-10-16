//! One tab's content: a VTE terminal running `tmux attach` against the
//! dedicated server. tmux owns scrollback and splits; VTE just renders.

use gtk4::glib;
use gtk4::prelude::*;
use hashterm_core::config::Appearance;
use vte4::prelude::*;

type TitleCallback = Box<dyn Fn(String)>;
type ExitCallback = Box<dyn Fn()>;
/// Called with a cleaned link target after a Ctrl+click.
type LinkCallback = Box<dyn Fn(String)>;

pub struct TerminalPage {
    root: gtk4::Box,
    term: vte4::Terminal,
    on_title: std::rc::Rc<std::cell::RefCell<Option<TitleCallback>>>,
    on_exit: std::rc::Rc<std::cell::RefCell<Option<ExitCallback>>>,
    on_link: std::rc::Rc<std::cell::RefCell<Option<LinkCallback>>>,
    created: std::time::Instant,
    links_enabled: std::rc::Rc<std::cell::Cell<bool>>,
}

impl TerminalPage {
    pub fn new(attach_argv: &[String], appearance: &Appearance) -> Self {
        let term = vte4::Terminal::builder()
            .hexpand(true)
            .vexpand(true)
            // tmux owns history; a tiny VTE buffer avoids double-scrollback.
            .scrollback_lines(0)
            .build();
        apply_colors(&term, appearance);
        apply_cursor(&term, appearance);
        term.set_bold_is_bright(appearance.bold_is_bright);
        term.set_font_desc(Some(&gtk4::pango::FontDescription::from_string(
            &appearance.font,
        )));

        let on_link: std::rc::Rc<std::cell::RefCell<Option<LinkCallback>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let links_enabled = std::rc::Rc::new(std::cell::Cell::new(true));
        install_links(&term, on_link.clone(), links_enabled.clone());

        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("terminal-page");
        root.append(&term);

        let on_title: std::rc::Rc<std::cell::RefCell<Option<TitleCallback>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let on_exit: std::rc::Rc<std::cell::RefCell<Option<ExitCallback>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));

        {
            // VTE >= 0.78: window title arrives as the xterm.title termprop.
            let on_title = on_title.clone();
            term.connect_termprop_changed(Some("xterm.title"), move |t, prop| {
                if let Some(cb) = on_title.borrow().as_ref()
                    && let (Some(title), _) = t.termprop_string(prop)
                {
                    // A pane's output is untrusted: strip control chars and cap
                    // length before this reaches GTK. A multi-MB title otherwise
                    // blocks the main thread in Pango layout (DoS).
                    let clean: String = title
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(256)
                        .collect();
                    cb(clean);
                }
            });
        }
        {
            let on_exit = on_exit.clone();
            term.connect_child_exited(move |_, _status| {
                if let Some(cb) = on_exit.borrow().as_ref() {
                    cb();
                }
            });
        }

        let argv: Vec<&str> = attach_argv.iter().map(String::as_str).collect();
        // vte4's envv is not optional and an empty slice means an EMPTY
        // environment, not inherit — pass the parent environment, minus a
        // denylist of dynamic-linker / interpreter-hijack variables so a value
        // hashterm was launched with can't be inherited by every shell in
        // every tab. GDK_BACKEND (which we may force to x11) is also dropped so
        // it doesn't steer child GTK apps.
        let env: Vec<String> = std::env::vars()
            .filter(|(k, _)| !env_is_denied(k))
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let envv: Vec<&str> = env.iter().map(String::as_str).collect();
        term.spawn_async(
            vte4::PtyFlags::DEFAULT,
            None, // cwd irrelevant: tmux pane owns the real cwd
            &argv,
            &envv,
            glib::SpawnFlags::SEARCH_PATH,
            || {},
            -1,
            gtk4::gio::Cancellable::NONE,
            |result| {
                if let Err(e) = result {
                    tracing::error!("failed to spawn tmux attach: {e}");
                }
            },
        );

        Self {
            root,
            term,
            on_title,
            on_exit,
            on_link,
            created: std::time::Instant::now(),
            links_enabled,
        }
    }

    pub fn set_on_link(&self, cb: LinkCallback) {
        *self.on_link.borrow_mut() = Some(cb);
    }

    /// The screen content changed (VTE coalesces; this is cheap). Fires for
    /// tabs hidden in the stack too.
    pub fn connect_contents_changed(&self, cb: impl Fn() + 'static) {
        self.term.connect_contents_changed(move |_| cb());
    }

    /// How long ago this page was created (startup redraws are not activity).
    pub fn age(&self) -> std::time::Duration {
        self.created.elapsed()
    }

    pub fn set_links_enabled(&self, on: bool) {
        self.links_enabled.set(on);
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.root
    }

    pub fn set_on_title_changed(&self, cb: TitleCallback) {
        *self.on_title.borrow_mut() = Some(cb);
    }

    /// Fires when the `tmux attach` process exits (detach, kill-session, or
    /// server death). The window decides whether to re-attach or drop the tab.
    pub fn set_on_child_exited(&self, cb: ExitCallback) {
        *self.on_exit.borrow_mut() = Some(cb);
    }

    pub fn apply_appearance(&self, appearance: &Appearance) {
        apply_colors(&self.term, appearance);
        apply_cursor(&self.term, appearance);
        self.term.set_bold_is_bright(appearance.bold_is_bright);
        self.term
            .set_font_desc(Some(&gtk4::pango::FontDescription::from_string(
                &appearance.font,
            )));
    }

    /// Window-local text zoom (1.0 = the configured font size).
    pub fn set_font_scale(&self, scale: f64) {
        self.term.set_font_scale(scale);
    }

    /// Force a repaint of the terminal widget (used after a window resize).
    pub fn queue_redraw(&self) {
        self.term.queue_draw();
    }

    pub fn grab_focus(&self) {
        self.term.grab_focus();
    }

    pub fn copy_selection(&self) {
        self.term.copy_clipboard_format(vte4::Format::Text);
    }

    pub fn paste_clipboard(&self) {
        // Guard against paste-injection: clipboard content with a newline
        // executes as soon as it lands. Inspect the text first; if it carries
        // a newline (or other control byte), confirm before pasting. The paste
        // itself always goes through VTE's paste_clipboard(), which preserves
        // bracketed-paste framing (the standard defense) — never a raw
        // feed_child, which would strip it and be MORE dangerous.
        let term = self.term.clone();
        let clipboard = term.clipboard();
        clipboard.read_text_async(gtk4::gio::Cancellable::NONE, move |res| {
            let Ok(Some(text)) = res else {
                // Couldn't inspect (e.g. non-text clipboard): fall back to the
                // normal, bracketed paste.
                term.paste_clipboard();
                return;
            };
            let risky = text
                .chars()
                .any(|c| c == '\n' || (c.is_control() && c != '\t'));
            if !risky {
                term.paste_clipboard();
                return;
            }
            confirm_multiline_paste(&term, text.lines().count());
        });
    }
}

/// Environment variables never forwarded to child shells: dynamic-linker and
/// interpreter hijack vectors, plus the backend override we may have forced.
fn env_is_denied(key: &str) -> bool {
    matches!(
        key,
        "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "GTK_MODULES"
            | "GIO_MODULE_DIR"
            | "BASH_ENV"
            | "ENV"
            | "PYTHONSTARTUP"
            | "NODE_OPTIONS"
            | "PERL5OPT"
            | "GDK_BACKEND"
    )
}

/// Modal "this paste has newlines — run it?" confirmation. On confirm the
/// clipboard is pasted normally (bracketed-paste preserved); on cancel nothing
/// happens.
fn confirm_multiline_paste(term: &vte4::Terminal, lines: usize) {
    let root = term.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
    let dialog = gtk4::Window::builder()
        .modal(true)
        .title("Confirm paste")
        .default_width(420)
        .build();
    if let Some(w) = &root {
        dialog.set_transient_for(Some(w));
    }
    let root_box = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    root_box.set_margin_top(14);
    root_box.set_margin_bottom(14);
    root_box.set_margin_start(14);
    root_box.set_margin_end(14);
    let lines = lines.max(1);
    root_box.append(
        &gtk4::Label::builder()
            .label(format!(
                "The clipboard contains {lines} line(s) and may run commands as soon as it is pasted. Paste anyway?"
            ))
            .wrap(true)
            .xalign(0.0)
            .build(),
    );
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let cancel = gtk4::Button::with_label("Cancel");
    let paste = gtk4::Button::with_label("Paste");
    paste.add_css_class("destructive-action");
    buttons.append(&spacer);
    buttons.append(&cancel);
    buttons.append(&paste);
    root_box.append(&buttons);
    dialog.set_child(Some(&root_box));
    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| dialog.close()
    });
    paste.connect_clicked({
        let dialog = dialog.clone();
        let term = term.clone();
        move |_| {
            // Bracketed-paste preserved: re-read the clipboard through VTE.
            term.paste_clipboard();
            dialog.close();
        }
    });
    dialog.present();
}

/// Foreground opaque; background carries the configured opacity so text stays
/// crisp while the terminal shows through (the window CSS matches this alpha).
fn apply_colors(term: &vte4::Terminal, appearance: &Appearance) {
    let r = appearance.resolved();
    let parse = |s: &str, fallback: &str| {
        gtk4::gdk::RGBA::parse(s).unwrap_or_else(|_| gtk4::gdk::RGBA::parse(fallback).unwrap())
    };
    let fg = parse(&r.foreground, hashterm_core::scheme::DEFAULT_FG);
    let mut bg = parse(&r.background, hashterm_core::scheme::DEFAULT_BG);
    bg.set_alpha(appearance.opacity.clamp(0.05, 1.0) as f32);
    let palette: Vec<gtk4::gdk::RGBA> = r.palette.iter().map(|c| parse(c, "#808080")).collect();
    let refs: Vec<&gtk4::gdk::RGBA> = palette.iter().collect();
    term.set_colors(Some(&fg), Some(&bg), &refs);
    // Cursor and selection: the scheme's, or VTE's reverse-video when absent.
    let cursor = r.cursor.as_deref().map(|c| parse(c, &r.foreground));
    term.set_color_cursor(cursor.as_ref());
    term.set_color_cursor_foreground(cursor.as_ref().map(|_| &bg));
    let sel = r.selection.as_deref().map(|c| parse(c, &r.foreground));
    term.set_color_highlight(sel.as_ref());
    term.set_color_highlight_foreground(sel.as_ref().map(|_| &fg));
}

fn apply_cursor(term: &vte4::Terminal, appearance: &Appearance) {
    term.set_cursor_shape(match appearance.cursor_shape.as_str() {
        "ibeam" => vte4::CursorShape::Ibeam,
        "underline" => vte4::CursorShape::Underline,
        _ => vte4::CursorShape::Block,
    });
    term.set_cursor_blink_mode(if appearance.cursor_blink {
        vte4::CursorBlinkMode::On
    } else {
        vte4::CursorBlinkMode::Off
    });
}

/// PCRE2_MULTILINE: VTE requires it on match regexes.
const PCRE2_MULTILINE: u32 = 0x0000_0400;

/// The link under a widget-space point: an OSC 8 hyperlink first, else a
/// regex match, cleaned of trailing punctuation and checked for sanity.
fn link_at(term: &vte4::Terminal, x: f64, y: f64) -> Option<String> {
    let raw = term
        .check_hyperlink_at(x, y)
        .map(|g| g.to_string())
        .or_else(|| {
            term.check_match_at(x, y)
                .0
                .map(|g| crate::links::clean_url(&g))
        })?;
    crate::links::is_sane(&raw).then_some(raw)
}

fn ctrl_held(state: gtk4::gdk::ModifierType) -> bool {
    state.contains(gtk4::gdk::ModifierType::CONTROL_MASK)
}

/// Ctrl+click opens the link under the pointer. The gesture runs in the
/// capture phase and claims the click ONLY when a link is there, so every
/// other click (and tmux mouse mode) behaves exactly as before.
fn install_links(
    term: &vte4::Terminal,
    on_link: std::rc::Rc<std::cell::RefCell<Option<LinkCallback>>>,
    enabled: std::rc::Rc<std::cell::Cell<bool>>,
) {
    term.set_allow_hyperlink(true);
    match vte4::Regex::for_match(crate::links::URL_PATTERN, PCRE2_MULTILINE) {
        Ok(re) => {
            let tag = term.match_add_regex(&re, 0);
            term.match_set_cursor_name(tag, "pointer");
        }
        Err(e) => tracing::warn!("link pattern rejected by VTE: {e}"),
    }
    let click = gtk4::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        // Weak: a controller that owns its own widget would keep every closed
        // tab's terminal alive.
        let term = term.downgrade();
        let enabled = enabled.clone();
        click.connect_pressed(move |g, _, x, y| {
            let Some(term) = term.upgrade() else { return };
            if !enabled.get() || !ctrl_held(g.current_event_state()) {
                return;
            }
            if let Some(uri) = link_at(&term, x, y) {
                g.set_state(gtk4::EventSequenceState::Claimed);
                if let Some(cb) = on_link.borrow().as_ref() {
                    cb(uri);
                }
            }
        });
    }
    term.add_controller(click);

    // With Ctrl held, show where the link really goes.
    let motion = gtk4::EventControllerMotion::new();
    motion.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let term = term.downgrade();
        motion.connect_motion(move |c, x, y| {
            let Some(term) = term.upgrade() else { return };
            let tip = if enabled.get() && ctrl_held(c.current_event_state()) {
                link_at(&term, x, y)
            } else {
                None
            };
            term.set_tooltip_text(tip.as_deref());
        });
    }
    term.add_controller(motion);
}
