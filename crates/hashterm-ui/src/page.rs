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
        // Guard against paste-injection: clipboard content with a newline
        // executes as soon as it lands. Read the text ourselves, and if it
        // carries a newline (or other control byte), confirm before feeding.
        let term = self.term.clone();
        let clipboard = term.clipboard();
        clipboard.read_text_async(gtk4::gio::Cancellable::NONE, move |res| {
            let Ok(Some(text)) = res else { return };
            let text = text.to_string();
            let risky = text
                .chars()
                .any(|c| c == '\n' || (c.is_control() && c != '\t'));
            if !risky {
                term.feed_child(text.as_bytes());
                return;
            }
            confirm_multiline_paste(&term, text);
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

/// Modal "this paste has newlines — run it?" confirmation. On confirm the raw
/// bytes are fed to the child; on cancel nothing happens.
fn confirm_multiline_paste(term: &vte4::Terminal, text: String) {
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
    let lines = text.lines().count().max(1);
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
            term.feed_child(text.as_bytes());
            dialog.close();
        }
    });
    dialog.present();
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
