//! A small dismissible strip at the bottom of the console for one-time
//! guidance (first-run hint, "your global hotkey is limited"). It never takes
//! keyboard focus, so typing continues in the terminal underneath.

use gtk4::prelude::*;
use std::rc::Rc;

pub type Action = Rc<dyn Fn()>;

pub struct Notice {
    revealer: gtk4::Revealer,
    label: gtk4::Label,
    buttons: gtk4::Box,
    /// Bumped by every `show`, so a timer set for an older notice can tell.
    generation: std::cell::Cell<u64>,
}

impl Notice {
    pub fn new() -> Rc<Self> {
        let label = gtk4::Label::builder()
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .max_width_chars(70)
            .selectable(false)
            .build();
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        buttons.set_valign(gtk4::Align::Center);
        let inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        inner.add_css_class("notice");
        inner.append(&label);
        inner.append(&buttons);
        let revealer = gtk4::Revealer::builder()
            .child(&inner)
            .transition_type(gtk4::RevealerTransitionType::SlideUp)
            .transition_duration(160)
            .reveal_child(false)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::End)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();
        revealer.set_can_focus(false);
        revealer.set_accessible_role(gtk4::AccessibleRole::Alert);
        Rc::new(Self {
            revealer,
            label,
            buttons,
            generation: std::cell::Cell::new(0),
        })
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    pub fn is_shown(&self) -> bool {
        self.revealer.reveals_child()
    }

    /// Show `text` with one button per action. Every button runs its action and
    /// then dismisses the notice.
    pub fn show(self: &Rc<Self>, text: &str, actions: Vec<(String, Action)>) {
        while let Some(child) = self.buttons.first_child() {
            self.buttons.remove(&child);
        }
        self.generation.set(self.generation.get() + 1);
        self.label.set_label(text);
        for (title, action) in actions {
            let b = gtk4::Button::with_label(&title);
            b.set_focus_on_click(false);
            b.set_can_focus(false);
            let this = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                action();
                if let Some(n) = this.upgrade() {
                    n.hide();
                }
            });
            self.buttons.append(&b);
        }
        self.revealer.set_reveal_child(true);
    }

    /// Hide after `secs`, unless a different notice has replaced this one.
    pub fn hide_after(self: &Rc<Self>, secs: u32) {
        let (weak, mine) = (Rc::downgrade(self), self.generation.get());
        gtk4::glib::timeout_add_seconds_local_once(secs, move || {
            if let Some(n) = weak.upgrade()
                && n.generation.get() == mine
            {
                n.hide();
            }
        });
    }

    pub fn hide(&self) {
        self.revealer.set_reveal_child(false);
    }
}

/// First-run guidance. Shortcut labels come from the live config, and the
/// toggle key is left out when the drop-down is off.
pub fn welcome_text(
    dropdown: bool,
    toggle: &str,
    new_tab: &str,
    new_group: &str,
    settings: &str,
) -> String {
    let mut parts = Vec::new();
    if dropdown && !toggle.is_empty() {
        parts.push(format!("{toggle} shows and hides this terminal."));
    }
    parts.push(format!("{new_tab} opens a tab."));
    parts.push(format!(
        "{new_group} starts a group: a second row of tabs, right-click a tab to move it."
    ));
    parts.push(format!("{settings} opens Settings."));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::welcome_text;

    #[test]
    fn welcome_mentions_the_toggle_key_only_for_the_drop_down() {
        let with = welcome_text(true, "F12", "Ctrl+Shift+T", "Ctrl+Shift+N", "Ctrl+,");
        assert!(with.starts_with("F12 shows and hides"));
        assert!(with.contains("Ctrl+Shift+N starts a group"));
        let without = welcome_text(false, "F12", "Ctrl+Shift+T", "Ctrl+Shift+N", "Ctrl+,");
        assert!(!without.contains("F12"));
        assert!(without.starts_with("Ctrl+Shift+T opens a tab."));
    }
}
