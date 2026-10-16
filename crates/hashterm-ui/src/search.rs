//! The find bar: a small overlay at the top right of the console. It only
//! collects input and shows results; the window owns the tmux calls (they run
//! off the main thread) through the callbacks.

use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

pub type QueryCb = Rc<dyn Fn(String)>;
pub type StepCb = Rc<dyn Fn(bool)>;
pub type CloseCb = Rc<dyn Fn()>;

pub struct SearchBar {
    revealer: gtk4::Revealer,
    entry: gtk4::SearchEntry,
    status: gtk4::Label,
    nav: Vec<gtk4::Button>,
    pending: Cell<Option<gtk4::glib::SourceId>>,
}

/// "1,234 matches" / "1 match" / "No matches" / "Match found" (no count).
pub fn result_label(present: bool, count: Option<u32>) -> String {
    if !present {
        return "No matches".into();
    }
    match count {
        None => "Match found".into(),
        Some(1) => "1 match".into(),
        Some(n) => {
            let s = n.to_string();
            let mut out = String::new();
            for (i, c) in s.chars().enumerate() {
                if i > 0 && (s.len() - i) % 3 == 0 {
                    out.push(',');
                }
                out.push(c);
            }
            format!("{out} matches")
        }
    }
}

impl SearchBar {
    pub fn new(on_query: QueryCb, on_step: StepCb, on_close: CloseCb) -> Rc<Self> {
        let entry = gtk4::SearchEntry::builder()
            .placeholder_text("Find in scrollback")
            .width_chars(26)
            .build();
        entry.set_tooltip_text(Some(
            "Enter: older match. Shift+Enter: newer. Esc: close. Lowercase ignores case.",
        ));
        let status = gtk4::Label::builder().xalign(1.0).width_chars(12).build();
        status.add_css_class("search-status");
        let mk = |icon: &str, tip: &str| {
            let b = gtk4::Button::from_icon_name(icon);
            b.set_tooltip_text(Some(tip));
            b.set_focus_on_click(false);
            b.add_css_class("search-btn");
            b
        };
        let older = mk("go-up-symbolic", "Older match (Enter)");
        let newer = mk("go-down-symbolic", "Newer match (Shift+Enter)");
        let close = mk("window-close-symbolic", "Close (Esc)");
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        row.add_css_class("search-bar");
        for w in [
            entry.clone().upcast::<gtk4::Widget>(),
            status.clone().upcast(),
            older.clone().upcast(),
            newer.clone().upcast(),
            close.clone().upcast(),
        ] {
            row.append(&w);
        }
        let revealer = gtk4::Revealer::builder()
            .child(&row)
            .transition_type(gtk4::RevealerTransitionType::SlideDown)
            .transition_duration(120)
            .halign(gtk4::Align::End)
            .valign(gtk4::Align::Start)
            .margin_top(8)
            .margin_end(14)
            .build();
        let bar = Rc::new(Self {
            revealer,
            entry,
            status,
            nav: vec![older.clone(), newer.clone()],
            pending: Cell::new(None),
        });

        // Debounce: 150 ms after the last keystroke.
        {
            let weak = Rc::downgrade(&bar);
            let on_query = on_query.clone();
            bar.entry.connect_search_changed(move |e| {
                let Some(bar) = weak.upgrade() else { return };
                if let Some(id) = bar.pending.take() {
                    id.remove();
                }
                let text = e.text().to_string();
                if text.is_empty() {
                    bar.status.set_label("");
                }
                let on_query = on_query.clone();
                let weak2 = Rc::downgrade(&bar);
                let id = gtk4::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(150),
                    move || {
                        if let Some(bar) = weak2.upgrade() {
                            let _ = bar.pending.take();
                        }
                        on_query(text);
                    },
                );
                bar.pending.set(Some(id));
            });
        }
        {
            let on_step = on_step.clone();
            bar.entry.connect_activate(move |_| on_step(false));
        }
        {
            let on_step = on_step.clone();
            older.connect_clicked(move |_| on_step(false));
        }
        {
            let on_step = on_step.clone();
            newer.connect_clicked(move |_| on_step(true));
        }
        {
            let on_close = on_close.clone();
            close.connect_clicked(move |_| on_close());
        }
        // Esc closes; Shift+Enter steps the other way.
        let keys = gtk4::EventControllerKey::new();
        {
            let on_close = on_close.clone();
            let on_step = on_step.clone();
            keys.connect_key_pressed(move |_, key, _, mods| {
                use gtk4::gdk::Key;
                if key == Key::Escape {
                    on_close();
                    return gtk4::glib::Propagation::Stop;
                }
                if (key == Key::Return || key == Key::KP_Enter)
                    && mods.contains(gtk4::gdk::ModifierType::SHIFT_MASK)
                {
                    on_step(true);
                    return gtk4::glib::Propagation::Stop;
                }
                gtk4::glib::Propagation::Proceed
            });
        }
        bar.entry.add_controller(keys);
        bar.entry.connect_stop_search({
            let on_close = on_close.clone();
            move |_| on_close()
        });
        bar
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    pub fn is_open(&self) -> bool {
        self.revealer.reveals_child()
    }

    /// Show and focus; keeps the previous text selected so typing replaces it.
    pub fn open(&self) {
        self.revealer.set_reveal_child(true);
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    pub fn close(&self) {
        if let Some(id) = self.pending.take() {
            id.remove();
        }
        self.revealer.set_reveal_child(false);
        self.status.set_label("");
    }

    pub fn text(&self) -> String {
        self.entry.text().to_string()
    }

    pub fn show_result(&self, present: bool, count: Option<u32>) {
        for b in &self.nav {
            b.set_sensitive(present);
        }
        let label = result_label(present, count);
        self.status.set_label(&label);
        if present {
            self.status.remove_css_class("error");
        } else if !self.entry.text().is_empty() {
            self.status.add_css_class("error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::result_label;

    #[test]
    fn labels() {
        assert_eq!(result_label(false, Some(0)), "No matches");
        assert_eq!(result_label(true, Some(1)), "1 match");
        assert_eq!(result_label(true, Some(1111)), "1,111 matches");
        assert_eq!(result_label(true, Some(50001)), "50,001 matches");
        assert_eq!(result_label(true, Some(12)), "12 matches");
        assert_eq!(result_label(true, None), "Match found");
    }
}
