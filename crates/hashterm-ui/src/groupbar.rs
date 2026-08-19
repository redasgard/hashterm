//! The group bar: the second axis of the tab MATRIX. Groups (rows) live on
//! one edge, the active group's tabs (columns) on the perpendicular edge.
//! Clicking a group switches the whole tab bar to it; dropping a tab onto a
//! group row moves the tab there; right-click renames.

use gtk4::prelude::*;
use hashterm_core::config::Edge;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::tabbar::{GroupInfo, orientation, reveal_transition};

pub enum GroupBarEvent {
    /// Switch the visible group.
    Select(String),
    /// Create a group (with a fresh tab) via the "+" button.
    New,
    /// Rename this group (right-click menu).
    Rename(String),
    /// Pick a color for this group (right-click menu).
    Color(String),
    /// Clear this group's color (right-click menu; shown only when set).
    ClearColor(String),
    /// Move tab `0` into group `1` (tab dropped onto a group row).
    MoveTab(String, String),
    /// Move tab into a brand-new group (tab dropped onto "+").
    MoveTabToNew(String),
}

type EventCallback = Rc<dyn Fn(GroupBarEvent)>;

pub struct GroupBar {
    revealer: gtk4::Revealer,
    container: gtk4::Box,
    strip: gtk4::Box,
    handle: gtk4::Box,
    /// Reusable right-click menu, parented to the stable handle (the rebuilt
    /// `container` must never parent popovers — orphaning segfaults).
    ctx_menu: gtk4::PopoverMenu,
    groups: RefCell<Vec<GroupInfo>>,
    active: RefCell<String>,
    show_counts: Cell<bool>,
    edge: Cell<Edge>,
    on_event: RefCell<Option<EventCallback>>,
}

impl GroupBar {
    pub fn new(edge: Edge, reveal_ms: u32) -> Rc<Self> {
        let container = gtk4::Box::new(orientation(edge), 2);

        let scroller = gtk4::ScrolledWindow::builder()
            .child(&container)
            .hscrollbar_policy(gtk4::PolicyType::External)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .build();

        let strip = gtk4::Box::new(orientation(edge), 4);
        strip.add_css_class("groupbar");
        strip.append(&scroller);

        // Dragging empty group-bar space moves the frameless window (bubble
        // phase — see wire_window_drag for why this is not a WindowHandle).
        let handle = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        handle.append(&strip);
        crate::tabbar::wire_window_drag(&handle);

        let ctx_menu = gtk4::PopoverMenu::from_model(gtk4::gio::MenuModel::NONE);
        ctx_menu.set_parent(&handle);
        ctx_menu.set_has_arrow(false);

        let revealer = gtk4::Revealer::builder()
            .child(&handle)
            .reveal_child(true)
            .transition_duration(reveal_ms)
            .transition_type(reveal_transition(edge))
            .build();

        Rc::new(Self {
            revealer,
            container,
            strip,
            handle,
            ctx_menu,
            groups: RefCell::new(Vec::new()),
            active: RefCell::new(hashterm_tmux::controller::DEFAULT_GROUP.into()),
            show_counts: Cell::new(true),
            edge: Cell::new(edge),
            on_event: RefCell::new(None),
        })
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    pub fn set_on_event(&self, cb: EventCallback) {
        *self.on_event.borrow_mut() = Some(cb);
    }

    pub fn set_groups(self: &Rc<Self>, groups: Vec<GroupInfo>, active: String, show_counts: bool) {
        *self.groups.borrow_mut() = groups;
        *self.active.borrow_mut() = active;
        self.show_counts.set(show_counts);
        // A concealed bar (hide_when_single) skips the widget churn; the
        // caller always sets reveal state BEFORE the data, so the reveal that
        // brings it back arrives here with reveals_child() already true.
        if self.revealer.reveals_child() {
            self.rebuild();
        }
    }

    pub fn apply_edge(self: &Rc<Self>, edge: Edge, reveal_ms: u32) {
        self.edge.set(edge);
        self.container.set_orientation(orientation(edge));
        self.strip.set_orientation(orientation(edge));
        self.revealer.set_transition_type(reveal_transition(edge));
        self.revealer.set_transition_duration(reveal_ms);
        self.rebuild();
    }

    fn emit(&self, ev: GroupBarEvent) {
        if let Some(cb) = self.on_event.borrow().clone() {
            cb(ev);
        }
    }

    /// Right-click menu for one group row (Rename…, Group Color…).
    fn open_ctx_menu(self: &Rc<Self>, group: &str, x: f64, y: f64) {
        use gtk4::gio;
        let actions = gio::SimpleActionGroup::new();
        let add = |name: &str, ev_maker: Box<dyn Fn(String) -> GroupBarEvent>| {
            let action = gio::SimpleAction::new(name, None);
            let this = Rc::downgrade(self);
            let group = group.to_owned();
            action.connect_activate(move |_, _| {
                if let Some(bar) = this.upgrade() {
                    bar.emit(ev_maker(group.clone()));
                }
            });
            actions.add_action(&action);
        };
        add("rename", Box::new(GroupBarEvent::Rename));
        add("color", Box::new(GroupBarEvent::Color));
        add("clear-color", Box::new(GroupBarEvent::ClearColor));
        let has_color = self
            .groups
            .borrow()
            .iter()
            .any(|g| g.name == group && g.color.is_some());
        let model = gio::Menu::new();
        model.append(Some("Rename Group…"), Some("groupctx.rename"));
        model.append(Some("Group Color…"), Some("groupctx.color"));
        if has_color {
            model.append(Some("Clear Color"), Some("groupctx.clear-color"));
        }
        self.handle.insert_action_group("groupctx", Some(&actions));
        self.ctx_menu.set_menu_model(Some(&model));
        self.ctx_menu
            .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.ctx_menu.popup();
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.container.first_child() {
            child.unparent();
        }
        let active = self.active.borrow().clone();
        let side = matches!(self.edge.get(), Edge::Left | Edge::Right);
        let show_counts = self.show_counts.get();

        for g in self.groups.borrow().iter() {
            let button = gtk4::ToggleButton::builder()
                .active(g.name == active)
                .build();
            button.add_css_class("group-row");
            if side {
                button.set_width_request(140);
            }

            let dot = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            dot.add_css_class("group-dot");
            dot.set_valign(gtk4::Align::Center);
            dot.set_halign(gtk4::Align::Center);
            if let Some(hex) = g.color.as_deref().and_then(|c| c.strip_prefix('#')) {
                dot.add_css_class(&format!("gacc-{hex}"));
            }
            let text = if show_counts {
                format!("{} ({})", g.name, g.count)
            } else {
                g.name.clone()
            };
            let label = gtk4::Label::builder()
                .label(&text)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .max_width_chars(16)
                .hexpand(side)
                .xalign(0.0)
                .build();
            let inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            inner.append(&dot);
            inner.append(&label);
            button.set_child(Some(&inner));

            let this = Rc::downgrade(self);
            let name = g.name.clone();
            button.connect_clicked(move |btn| {
                if !btn.is_active() {
                    btn.set_active(true);
                }
                if let Some(bar) = this.upgrade() {
                    bar.emit(GroupBarEvent::Select(name.clone()));
                }
            });

            // Right-click -> rename menu, in stable-handle coordinates.
            {
                let gesture = gtk4::GestureClick::new();
                gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
                gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
                gesture.connect_pressed(|g, _, _, _| {
                    g.set_state(gtk4::EventSequenceState::Claimed);
                });
                let this = Rc::downgrade(self);
                let name = g.name.clone();
                let btn = button.clone();
                gesture.connect_released(move |_, _, x, y| {
                    if let Some(bar) = this.upgrade() {
                        let point = btn
                            .compute_point(
                                &bar.handle,
                                &gtk4::graphene::Point::new(x as f32, y as f32),
                            )
                            .unwrap_or_else(|| gtk4::graphene::Point::new(x as f32, y as f32));
                        bar.open_ctx_menu(&name, point.x() as f64, point.y() as f64);
                    }
                });
                button.add_controller(gesture);
            }

            // Dropping a tab (payload = session name) on a row moves it there.
            {
                let drop = gtk4::DropTarget::new(
                    gtk4::glib::types::Type::STRING,
                    gtk4::gdk::DragAction::MOVE,
                );
                let this = Rc::downgrade(self);
                let name = g.name.clone();
                let target = button.clone();
                drop.connect_drop(move |_, value, _, _| {
                    // Completed drops don't reliably deliver a leave crossing.
                    target.remove_css_class("drag-over");
                    let (Ok(session), Some(bar)) = (value.get::<String>(), this.upgrade()) else {
                        return false;
                    };
                    bar.emit(GroupBarEvent::MoveTab(session, name.clone()));
                    true
                });
                let btn = button.clone();
                drop.connect_enter(move |_, _, _| {
                    btn.add_css_class("drag-over");
                    gtk4::gdk::DragAction::MOVE
                });
                let btn = button.clone();
                drop.connect_leave(move |_| btn.remove_css_class("drag-over"));
                button.add_controller(drop);
            }

            self.container.append(&button);
        }

        // "+" creates a group (with a fresh tab); dropping a tab on it moves
        // the tab into a brand-new group instead.
        let plus = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .has_frame(false)
            .tooltip_text("New group")
            .build();
        plus.add_css_class("group-new");
        {
            let this = Rc::downgrade(self);
            plus.connect_clicked(move |_| {
                if let Some(bar) = this.upgrade() {
                    bar.emit(GroupBarEvent::New);
                }
            });
        }
        {
            let drop = gtk4::DropTarget::new(
                gtk4::glib::types::Type::STRING,
                gtk4::gdk::DragAction::MOVE,
            );
            let this = Rc::downgrade(self);
            let target = plus.clone();
            drop.connect_drop(move |_, value, _, _| {
                target.remove_css_class("drag-over");
                let (Ok(session), Some(bar)) = (value.get::<String>(), this.upgrade()) else {
                    return false;
                };
                bar.emit(GroupBarEvent::MoveTabToNew(session));
                true
            });
            let btn = plus.clone();
            drop.connect_enter(move |_, _, _| {
                btn.add_css_class("drag-over");
                gtk4::gdk::DragAction::MOVE
            });
            let btn = plus.clone();
            drop.connect_leave(move |_| btn.remove_css_class("drag-over"));
            plus.add_controller(drop);
        }
        self.container.append(&plus);
    }
}
