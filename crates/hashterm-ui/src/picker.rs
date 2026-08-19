//! Session picker: save the current state under a name, and list / restore /
//! delete existing saves (named + autosaves).

use gtk4::prelude::*;
use hashterm_session::SessionStore;
use std::rc::Rc;

use crate::window::MainWindow;

pub fn open(win: &Rc<MainWindow>) {
    let dialog = gtk4::Window::builder()
        .transient_for(win.gtk_window())
        .modal(true)
        .title("Sessions")
        .default_width(440)
        .default_height(420)
        .build();
    dialog.add_css_class("session-picker");

    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);

    // -- save current --
    let save_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let name_entry = gtk4::Entry::builder()
        .placeholder_text("save current session as…")
        .hexpand(true)
        .build();
    let save_btn = gtk4::Button::with_label("Save");
    save_btn.add_css_class("suggested-action");
    save_row.append(&name_entry);
    save_row.append(&save_btn);
    root.append(&save_row);

    // -- list of saves --
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::Single)
        .vexpand(true)
        .build();
    let scroller = gtk4::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();
    root.append(&scroller);

    // -- actions --
    let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let restore_btn = gtk4::Button::with_label("Restore");
    restore_btn.add_css_class("suggested-action");
    let delete_btn = gtk4::Button::with_label("Delete");
    delete_btn.add_css_class("destructive-action");
    let close_btn = gtk4::Button::with_label("Close");
    actions.append(&restore_btn);
    actions.append(&delete_btn);
    let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(&close_btn);
    root.append(&actions);

    dialog.set_child(Some(&root));

    // Per-row (name, auto), kept in list order — safe replacement for stashing
    // typed pointers in GObject qdata via unsafe set_data/data.
    let rows: Rc<std::cell::RefCell<Vec<(String, bool)>>> =
        Rc::new(std::cell::RefCell::new(Vec::new()));
    let refresh = {
        let list = list.clone();
        let rows = rows.clone();
        move || {
            while let Some(row) = list.first_child() {
                list.remove(&row);
            }
            rows.borrow_mut().clear();
            let store = SessionStore::new(SessionStore::default_root());
            match store.list() {
                Ok(saves) => {
                    for s in saves {
                        let label = gtk4::Label::builder()
                            .label(format!(
                                "{}   {}   {} session(s){}",
                                s.created_local,
                                s.name,
                                s.session_count,
                                if s.auto { "   [autosave]" } else { "" }
                            ))
                            .xalign(0.0)
                            .build();
                        let row = gtk4::ListBoxRow::new();
                        row.set_child(Some(&label));
                        rows.borrow_mut().push((s.name.clone(), s.auto));
                        list.append(&row);
                    }
                }
                Err(e) => tracing::error!("listing saves failed: {e}"),
            }
        }
    };
    refresh();

    type Selector = Rc<dyn Fn(&gtk4::ListBox) -> Option<(String, bool)>>;
    let selected: Selector = {
        let rows = rows.clone();
        Rc::new(move |list: &gtk4::ListBox| {
            let idx = list.selected_row()?.index();
            rows.borrow().get(idx as usize).cloned()
        })
    };

    save_btn.connect_clicked({
        let win = Rc::downgrade(win);
        let name_entry = name_entry.clone();
        let refresh = refresh.clone();
        move |_| {
            let name = name_entry.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            if let Some(w) = win.upgrade() {
                w.save_named(&name);
            }
            name_entry.set_text("");
            refresh();
        }
    });

    restore_btn.connect_clicked({
        let win = Rc::downgrade(win);
        let list = list.clone();
        let dialog = dialog.clone();
        let selected = selected.clone();
        move |_| {
            if let (Some(w), Some((name, _auto))) = (win.upgrade(), selected(&list)) {
                w.restore_named(&name);
                dialog.close();
            }
        }
    });

    delete_btn.connect_clicked({
        let list = list.clone();
        let refresh = refresh.clone();
        let selected = selected.clone();
        move |_| {
            if let Some((name, auto)) = selected(&list) {
                let store = SessionStore::new(SessionStore::default_root());
                if let Err(e) = store.delete(&name, auto) {
                    tracing::error!("delete save '{name}': {e}");
                }
                refresh();
            }
        }
    });

    close_btn.connect_clicked({
        let dialog = dialog.clone();
        move |_| dialog.close()
    });

    dialog.present();
}
