//! The group bar: the second axis of the tab MATRIX. Groups (rows) live on
//! one edge, the active group's tabs (columns) on the perpendicular edge.
//! Clicking a group switches the whole tab bar to it; dropping a tab onto a
//! group row moves the tab there; right-click renames. Dragging a group row
//! onto another reorders the groups (order is persisted by the window).

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
    /// Move group `0` to position `1` (group row dropped onto another row).
    Reorder(String, usize),
    /// Shift group `0` by `1` places (right-click "Move Earlier/Later").
    Shift(String, i32),
    /// Wheel over group `0`'s row: step through THAT group's tabs
    /// (`true` = next).
    Scroll(String, bool),
    /// Close group `0` with ALL its tabs (the window asks for confirmation).
    Close(String),
}

/// Drag payload prefix that tells a dragged GROUP row from a dragged tab
/// (whose payload is a bare session name). Never a valid session name.
pub(crate) const GROUP_DRAG_PREFIX: &str = "\u{1}group:";

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
    reorderable: Cell<bool>,
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
            reorderable: Cell::new(true),
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

    pub fn set_groups(
        self: &Rc<Self>,
        groups: Vec<GroupInfo>,
        active: String,
        show_counts: bool,
        reorderable: bool,
    ) {
        // Skip the widget churn when nothing changed (rebuilds are expensive
        // and leak per call on GTK 4.22).
        if *self.groups.borrow() == groups
            && *self.active.borrow() == active
            && self.show_counts.get() == show_counts
            && self.reorderable.get() == reorderable
        {
            return;
        }
        *self.groups.borrow_mut() = groups;
        *self.active.borrow_mut() = active;
        self.show_counts.set(show_counts);
        self.reorderable.set(reorderable);
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
        // Reordering without a mouse drag (also the keyboard/touch path).
        let (idx, total) = {
            let groups = self.groups.borrow();
            (groups.iter().position(|g| g.name == group), groups.len())
        };
        let can_earlier = self.reorderable.get() && idx.is_some_and(|i| i > 0);
        let can_later = self.reorderable.get() && idx.is_some_and(|i| i + 1 < total);
        for (name, delta, enabled) in [("earlier", -1, can_earlier), ("later", 1, can_later)] {
            if !enabled {
                continue;
            }
            let action = gio::SimpleAction::new(name, None);
            let this = Rc::downgrade(self);
            let group = group.to_owned();
            action.connect_activate(move |_, _| {
                if let Some(bar) = this.upgrade() {
                    bar.emit(GroupBarEvent::Shift(group.clone(), delta));
                }
            });
            actions.add_action(&action);
        }
        let model = gio::Menu::new();
        model.append(Some("Rename Group…"), Some("groupctx.rename"));
        model.append(Some("Group Color…"), Some("groupctx.color"));
        if has_color {
            model.append(Some("Clear Color"), Some("groupctx.clear-color"));
        }
        let (earlier, later) = match self.edge.get() {
            Edge::Top | Edge::Bottom => ("Move Left", "Move Right"),
            Edge::Left | Edge::Right => ("Move Up", "Move Down"),
        };
        if can_earlier || can_later {
            let section = gio::Menu::new();
            if can_earlier {
                section.append(Some(earlier), Some("groupctx.earlier"));
            }
            if can_later {
                section.append(Some(later), Some("groupctx.later"));
            }
            model.append_section(None, &section);
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
            // Close-group button. It is an OVERLAY sibling of the row, not a
            // child of it: a button nested in a toggle button loses its clicks
            // to the row's drag/click gestures. Invisible (and untargetable, so
            // a stray click can't hit it) until the pointer is over the row;
            // the row reserves its space so rows never jump on hover.
            let close = gtk4::Button::builder()
                .icon_name("window-close-symbolic")
                .has_frame(false)
                .halign(gtk4::Align::End)
                .valign(gtk4::Align::Center)
                .margin_end(6)
                .tooltip_text(format!("Close group \"{}\" and all its tabs", g.name))
                .build();
            close.add_css_class("group-close");
            close.set_opacity(0.0);
            close.set_can_target(false);
            {
                let this = Rc::downgrade(self);
                let name = g.name.clone();
                close.connect_clicked(move |_| {
                    if let Some(bar) = this.upgrade() {
                        bar.emit(GroupBarEvent::Close(name.clone()));
                    }
                });
            }
            inner.set_margin_end(22);
            let row = gtk4::Overlay::new();
            row.set_child(Some(&button));
            row.add_overlay(&close);
            {
                // contains-pointer covers the row AND the overlaid button:
                // enter/leave would fire when the pointer moves onto the
                // button itself, hiding it from under the cursor.
                let motion = gtk4::EventControllerMotion::new();
                let c = close.downgrade();
                motion.connect_contains_pointer_notify(move |m| {
                    if let Some(c) = c.upgrade() {
                        let on = m.contains_pointer();
                        c.set_opacity(if on { 1.0 } else { 0.0 });
                        c.set_can_target(on);
                    }
                });
                row.add_controller(motion);
            }
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

            // Wheel over a group's title steps through that group's tabs.
            {
                let this = Rc::downgrade(self);
                let name = g.name.clone();
                button.add_controller(crate::tabbar::wheel_steps(move |forward| {
                    if let Some(bar) = this.upgrade() {
                        bar.emit(GroupBarEvent::Scroll(name.clone(), forward));
                    }
                }));
            }

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
                // Reach the row through the controller, never a captured clone
                // (controller lives in the button: a clone is a leaking cycle).
                gesture.connect_released(move |g, _, x, y| {
                    if let (Some(bar), Some(btn)) = (this.upgrade(), g.widget()) {
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

            // Drag source: the row itself, to reorder groups.
            if self.reorderable.get() {
                let drag = gtk4::DragSource::builder()
                    .actions(gtk4::gdk::DragAction::MOVE)
                    .build();
                drag.set_propagation_phase(gtk4::PropagationPhase::Capture);
                let payload = format!("{GROUP_DRAG_PREFIX}{}", g.name);
                drag.connect_prepare(move |_, _, _| {
                    Some(gtk4::gdk::ContentProvider::for_value(&payload.to_value()))
                });
                button.add_controller(drag.clone());
                // Same claim-with-the-DragSource trick as the tabs, so the
                // empty-space window drag never competes with a row drag.
                let claim = gtk4::GestureDrag::new();
                claim.set_propagation_phase(gtk4::PropagationPhase::Capture);
                claim.connect_drag_update(|g, dx, dy| {
                    if dx.abs() > 3.0 || dy.abs() > 3.0 {
                        g.set_state(gtk4::EventSequenceState::Claimed);
                    }
                });
                button.add_controller(claim.clone());
                claim.group_with(&drag);
            }

            // Dropping a tab (payload = session name) on a row moves it there;
            // dropping a group row (prefixed payload) reorders groups.
            {
                let drop = gtk4::DropTarget::new(
                    gtk4::glib::types::Type::STRING,
                    gtk4::gdk::DragAction::MOVE,
                );
                let this = Rc::downgrade(self);
                let name = g.name.clone();
                let index = self.groups.borrow().iter().position(|x| x.name == g.name);
                drop.connect_drop(move |d, value, _, _| {
                    // Completed drops don't reliably deliver a leave crossing.
                    if let Some(w) = d.widget() {
                        w.remove_css_class("drag-over");
                    }
                    let (Ok(payload), Some(bar)) = (value.get::<String>(), this.upgrade()) else {
                        return false;
                    };
                    if let Some(dragged) = payload.strip_prefix(GROUP_DRAG_PREFIX) {
                        let (true, Some(index)) = (bar.reorderable.get(), index) else {
                            return false;
                        };
                        if dragged == name {
                            return false;
                        }
                        bar.emit(GroupBarEvent::Reorder(dragged.to_owned(), index));
                    } else {
                        bar.emit(GroupBarEvent::MoveTab(payload, name.clone()));
                    }
                    true
                });
                drop.connect_enter(|d, _, _| {
                    if let Some(w) = d.widget() {
                        w.add_css_class("drag-over");
                    }
                    gtk4::gdk::DragAction::MOVE
                });
                drop.connect_leave(|d| {
                    if let Some(w) = d.widget() {
                        w.remove_css_class("drag-over");
                    }
                });
                button.add_controller(drop);
            }

            self.container.append(&row);
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
            let drop =
                gtk4::DropTarget::new(gtk4::glib::types::Type::STRING, gtk4::gdk::DragAction::MOVE);
            let this = Rc::downgrade(self);
            drop.connect_drop(move |d, value, _, _| {
                if let Some(w) = d.widget() {
                    w.remove_css_class("drag-over");
                }
                let (Ok(session), Some(bar)) = (value.get::<String>(), this.upgrade()) else {
                    return false;
                };
                if session.starts_with(GROUP_DRAG_PREFIX) {
                    return false; // a group row isn't a tab
                }
                bar.emit(GroupBarEvent::MoveTabToNew(session));
                true
            });
            drop.connect_enter(|d, _, _| {
                if let Some(w) = d.widget() {
                    w.add_css_class("drag-over");
                }
                gtk4::gdk::DragAction::MOVE
            });
            drop.connect_leave(|d| {
                if let Some(w) = d.widget() {
                    w.remove_css_class("drag-over");
                }
            });
            plus.add_controller(drop);
        }
        self.container.append(&plus);
    }
}
