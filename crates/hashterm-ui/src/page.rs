//! One tab's content: a VTE terminal running `tmux attach` against the
//! dedicated server. tmux owns scrollback and splits; VTE just renders.

use gtk4::glib;
use gtk4::prelude::*;
use hashterm_core::config::Appearance;
use vte4::prelude::*;

type TitleCallback = Box<dyn Fn(String)>;
type ExitCallback = Box<dyn Fn()>;

pub struct TerminalPage {
    root: gtk4::Box,
    term: vte4::Terminal,
    on_title: std::rc::Rc<std::cell::RefCell<Option<TitleCallback>>>,
    on_exit: std::rc::Rc<std::cell::RefCell<Option<ExitCallback>>>,
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
        term.set_bold_is_bright(appearance.bold_is_bright);
        term.set_font_desc(Some(&gtk4::pango::FontDescription::from_string(
            &appearance.font,
        )));

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
                    cb(title.to_string());
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
        // environment, not inherit — pass the full parent environment.
        let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
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
        }
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
        self.term.set_bold_is_bright(appearance.bold_is_bright);
        self.term
            .set_font_desc(Some(&gtk4::pango::FontDescription::from_string(
                &appearance.font,
            )));
    }

    pub fn grab_focus(&self) {
        self.term.grab_focus();
    }

    pub fn copy_selection(&self) {
        self.term.copy_clipboard_format(vte4::Format::Text);
    }

    pub fn paste_clipboard(&self) {
        self.term.paste_clipboard();
    }
}

/// Foreground opaque; background carries the configured opacity so text stays
/// crisp while the terminal shows through (the window CSS matches this alpha).
fn apply_colors(term: &vte4::Terminal, appearance: &Appearance) {
    let fg = gtk4::gdk::RGBA::parse(&appearance.foreground)
        .unwrap_or_else(|_| gtk4::gdk::RGBA::parse("#c5c8c6").unwrap());
    let mut bg = gtk4::gdk::RGBA::parse(&appearance.background)
        .unwrap_or_else(|_| gtk4::gdk::RGBA::parse("#1d1f21").unwrap());
    bg.set_alpha(appearance.opacity.clamp(0.05, 1.0) as f32);
    term.set_colors(Some(&fg), Some(&bg), &[]);
}
