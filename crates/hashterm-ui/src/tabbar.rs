//! Custom tab bar over a GtkStack. v1: a rebuilt-on-change row of toggle
//! buttons (model-diffing and DnD reorder arrive in M6). Supports all four
//! edges — horizontal bars on top/bottom, vertical card lists on left/right —
//! auto-hide via GtkRevealer, and a trailing "+" button.

use gtk4::prelude::*;
use hashterm_core::config::{Edge, TabBarCfg};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabInfo {
    /// Stack child name == tmux session name.
    pub id: String,
    pub title: String,
    pub badge: Option<Badge>,
    /// Per-tab accent color "#rrggbb" (shown as an underline on the tab).
    pub accent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Bell,
    Activity,
    External,
}

/// A tab group as shown in the group bar (the matrix's other axis).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub name: String,
    pub color: Option<String>,
    pub count: usize,
}

pub enum TabBarEvent {
    Select(String),
    Close(String),
    NewTab,
    /// Move tab `id` so it sits at `index` in the strip.
    Reorder(String, usize),
    /// Open the per-tab properties dialog (rename, accent color).
    Properties(String),
    /// Move tab `0` into group `1`.
    MoveToGroup(String, String),
    /// Move tab into a brand-new group (name prompted).
    MoveToNewGroup(String),
    /// Rename this group. Lives in the tab context menu too so the default
    /// group stays renameable while the group bar is hidden
    /// (groupbar.hide_when_single).
    RenameGroup(String),
    /// Pick a color for this group (same hidden-group-bar reasoning).
    GroupColor(String),
    /// Clear this group's color (shown only when one is set).
    ClearGroupColor(String),
}

type EventCallback = Rc<dyn Fn(TabBarEvent)>;

pub struct TabBar {
    revealer: gtk4::Revealer,
    container: gtk4::Box,
    strip: gtk4::Box,
    /// Reusable per-tab context menu, parented to the stable handle.
    tab_menu: gtk4::PopoverMenu,
    handle: gtk4::Box,
    tabs: RefCell<Vec<TabInfo>>,
    active: RefCell<Option<String>>,
    groups: RefCell<Vec<GroupInfo>>,
    active_group: RefCell<String>,
    on_event: RefCell<Option<EventCallback>>,
    cfg: RefCell<TabBarCfg>,
    /// The context menu is open: auto-hide must NOT conceal the bar.
    menu_open: Rc<std::cell::Cell<bool>>,
}

impl TabBar {
    pub fn new(cfg: &TabBarCfg) -> Rc<Self> {
        let container = gtk4::Box::new(orientation(cfg.edge), 2);

        let scroller = gtk4::ScrolledWindow::builder()
            .child(&container)
            .hscrollbar_policy(gtk4::PolicyType::External)
            .vscrollbar_policy(gtk4::PolicyType::External)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .build();

        // The OUTER strip carries the bar background so it stays opaque in
        // overlaid/side layouts.
        let strip = gtk4::Box::new(orientation(cfg.edge), 4);
        strip.add_css_class("tabbar");
        strip.append(&scroller);

        // NOT a GtkWindowHandle: its window-move gesture runs in the capture
        // phase at the same 8px threshold as the tabs' DragSource, and as the
        // ancestor it evaluates first — a fast flick on a tab moved the whole
        // window instead of starting DnD. A plain box + our own BUBBLE-phase
        // move gesture can't race: bubble only fires when no child consumed
        // the drag, so empty bar space moves the window and tabs never do.
        let handle = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        handle.append(&strip);
        wire_window_drag(&handle);

        // Per-tab context menu: ONE reusable PopoverMenu on the stable handle.
        let tab_menu = gtk4::PopoverMenu::from_model(gtk4::gio::MenuModel::NONE);
        tab_menu.set_parent(&handle);
        tab_menu.set_has_arrow(false);

        // Right-click menu on EMPTY bar space (app-level actions). Right-click
        // ON a tab opens that tab's properties instead (per-tab gesture in
        // rebuild()). The popover MUST be parented to the stable `handle`,
        // never to `container`: rebuild() clears every container child, which
        // would orphan the popover and popping up an orphan segfaults in
        // gdk_surface_new_popup.
        let menu_open = Rc::new(std::cell::Cell::new(false));
        {
            let model = gtk4::gio::Menu::new();
            model.append(Some("New Tab"), Some("app.new-tab"));
            model.append(Some("Sessions…"), Some("app.sessions"));
            model.append(Some("Open Configuration"), Some("app.open-config"));
            model.append(Some("Hide Tab Bar"), Some("app.toggle-tabbar"));
            let popover = gtk4::PopoverMenu::from_model(Some(&model));
            popover.set_parent(&handle);
            popover.set_has_arrow(false);
            {
                let open = menu_open.clone();
                popover.connect_map(move |_| open.set(true));
            }
            {
                let open = menu_open.clone();
                popover.connect_closed(move |_| open.set(false));
            }

            let gesture = gtk4::GestureClick::new();
            gesture.set_button(gtk4::gdk::BUTTON_SECONDARY);
            // Capture + claim: keeps the press away from child gestures
            // for its own compositor window-menu gesture. Presses over a TAB
            // are denied here so the tab's own properties gesture wins.
            gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
            gesture.connect_pressed(|g, _, x, y| {
                let over_tab = g
                    .widget()
                    .and_then(|w| w.pick(x, y, gtk4::PickFlags::DEFAULT))
                    .map(|mut w| {
                        loop {
                            if w.has_css_class("tab") {
                                break true;
                            }
                            match w.parent() {
                                Some(p) => w = p,
                                None => break false,
                            }
                        }
                    })
                    .unwrap_or(false);
                g.set_state(if over_tab {
                    gtk4::EventSequenceState::Denied
                } else {
                    gtk4::EventSequenceState::Claimed
                });
            });
            let pop = popover.clone();
            // Popup on release, not press: opening during the press lets the
            // release land "outside" the fresh popover and dismiss it.
            gesture.connect_released(move |_, _, x, y| {
                pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                pop.popup();
            });
            handle.add_controller(gesture);
        }

        let revealer = gtk4::Revealer::builder()
            .child(&handle)
            .reveal_child(true)
            .transition_duration(cfg.reveal_ms)
            .transition_type(reveal_transition(cfg.edge))
            .build();

        let this = Rc::new(Self {
            revealer,
            container,
            strip,
            tab_menu,
            handle,
            tabs: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            groups: RefCell::new(Vec::new()),
            active_group: RefCell::new(hashterm_tmux::controller::DEFAULT_GROUP.into()),
            on_event: RefCell::new(None),
            cfg: RefCell::new(cfg.clone()),
            menu_open,
        });

        // menu_open covers every popover that must hold the bar revealed.
        {
            let open = this.menu_open.clone();
            this.tab_menu.connect_map(move |_| open.set(true));
            let open = this.menu_open.clone();
            this.tab_menu.connect_closed(move |_| open.set(false));
        }

        this
    }

    /// Auto-hide must not conceal the bar while its context menu is open.
    pub fn is_menu_open(&self) -> bool {
        self.menu_open.get()
    }

    pub fn widget(&self) -> &gtk4::Revealer {
        &self.revealer
    }

    pub fn set_on_event(&self, cb: EventCallback) {
        *self.on_event.borrow_mut() = Some(cb);
    }

    pub fn set_tabs(
        self: &Rc<Self>,
        tabs: Vec<TabInfo>,
        active: Option<String>,
        groups: Vec<GroupInfo>,
        active_group: String,
    ) {
        *self.tabs.borrow_mut() = tabs;
        *self.active.borrow_mut() = active;
        *self.groups.borrow_mut() = groups;
        *self.active_group.borrow_mut() = active_group;
        self.rebuild();
    }

    /// Per-tab context menu (right-click on a tab). A fresh action group is
    /// inserted each time with the session captured in closures.
    fn open_tab_menu(self: &Rc<Self>, session: &str, x: f64, y: f64) {
        use gtk4::gio;
        let actions = gio::SimpleActionGroup::new();
        let add = |name: &str, ev_maker: Box<dyn Fn() -> TabBarEvent>| {
            let action = gio::SimpleAction::new(name, None);
            let this = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(bar) = this.upgrade() {
                    bar.emit(ev_maker());
                }
            });
            actions.add_action(&action);
        };
        {
            let s = session.to_owned();
            add(
                "properties",
                Box::new(move || TabBarEvent::Properties(s.clone())),
            );
        }
        {
            let s = session.to_owned();
            add(
                "move-new",
                Box::new(move || TabBarEvent::MoveToNewGroup(s.clone())),
            );
        }
        {
            let s = session.to_owned();
            add("close", Box::new(move || TabBarEvent::Close(s.clone())));
        }
        {
            // The menu only ever opens on tabs of the ACTIVE group.
            let g = self.active_group.borrow().clone();
            add(
                "rename-group",
                Box::new(move || TabBarEvent::RenameGroup(g.clone())),
            );
        }
        {
            let g = self.active_group.borrow().clone();
            add(
                "group-color",
                Box::new(move || TabBarEvent::GroupColor(g.clone())),
            );
        }
        {
            let g = self.active_group.borrow().clone();
            add(
                "clear-group-color",
                Box::new(move || TabBarEvent::ClearGroupColor(g.clone())),
            );
        }

        let model = gio::Menu::new();
        model.append(Some("Properties…"), Some("tabctx.properties"));
        let move_menu = gio::Menu::new();
        let current_group = self
            .tabs
            .borrow()
            .iter()
            .find(|t| t.id == session)
            .map(|_| self.active_group.borrow().clone());
        for (i, g) in self.groups.borrow().iter().enumerate() {
            if Some(&g.name) == current_group.as_ref() {
                continue;
            }
            let action_name = format!("move-{i}");
            {
                let s = session.to_owned();
                let name = g.name.clone();
                add(
                    &action_name,
                    Box::new(move || TabBarEvent::MoveToGroup(s.clone(), name.clone())),
                );
            }
            move_menu.append(Some(&g.name), Some(&format!("tabctx.{action_name}")));
        }
        move_menu.append(Some("New group…"), Some("tabctx.move-new"));
        model.append_submenu(Some("Move to Group"), &move_menu);
        model.append(Some("Rename Group…"), Some("tabctx.rename-group"));
        model.append(Some("Group Color…"), Some("tabctx.group-color"));
        let active_has_color = {
            let active = self.active_group.borrow().clone();
            self.groups
                .borrow()
                .iter()
                .any(|g| g.name == active && g.color.is_some())
        };
        if active_has_color {
            model.append(Some("Clear Group Color"), Some("tabctx.clear-group-color"));
        }
        model.append(Some("Close"), Some("tabctx.close"));

        self.handle.insert_action_group("tabctx", Some(&actions));
        self.tab_menu.set_menu_model(Some(&model));
        self.tab_menu
            .set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        self.tab_menu.popup();
    }

    pub fn apply_config(self: &Rc<Self>, cfg: &TabBarCfg) {
        self.container.set_orientation(orientation(cfg.edge));
        self.strip.set_orientation(orientation(cfg.edge));
        self.revealer.set_transition_duration(cfg.reveal_ms);
        self.revealer
            .set_transition_type(reveal_transition(cfg.edge));
        for class in ["edge-top", "edge-bottom", "edge-left", "edge-right"] {
            self.container.remove_css_class(class);
        }
        self.container.add_css_class(edge_class(cfg.edge));
        *self.cfg.borrow_mut() = cfg.clone();
        self.rebuild();
    }

    pub fn set_revealed(&self, shown: bool) {
        self.revealer.set_reveal_child(shown);
    }

    pub fn is_revealed(&self) -> bool {
        self.revealer.reveals_child()
    }

    fn emit(&self, ev: TabBarEvent) {
        if let Some(cb) = self.on_event.borrow().clone() {
            cb(ev);
        }
    }

    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.container.first_child() {
            child.unparent();
        }
        let cfg = self.cfg.borrow().clone();
        let active = self.active.borrow().clone();
        let side = matches!(cfg.edge, Edge::Left | Edge::Right);

        for tab in self.tabs.borrow().iter() {
            let button = gtk4::ToggleButton::builder()
                .active(active.as_deref() == Some(tab.id.as_str()))
                .build();
            button.add_css_class("tab");
            if side {
                button.set_width_request(180);
            }
            match tab.badge {
                Some(Badge::Bell) => button.add_css_class("bell"),
                Some(Badge::Activity) => button.add_css_class("activity"),
                Some(Badge::External) => button.add_css_class("external"),
                None => {}
            }
            if let Some(accent) = &tab.accent
                && let Some(hex) = accent.strip_prefix('#')
            {
                button.add_css_class(&format!("acc-{hex}"));
            }

            // Right-click on a tab opens its context menu (properties, move
            // to group, close), positioned in stable-handle coordinates.
            {
                let props = gtk4::GestureClick::new();
                props.set_button(gtk4::gdk::BUTTON_SECONDARY);
                props.set_propagation_phase(gtk4::PropagationPhase::Capture);
                props.connect_pressed(|g, _, _, _| {
                    g.set_state(gtk4::EventSequenceState::Claimed);
                });
                let this = Rc::downgrade(self);
                let id = tab.id.clone();
                let btn = button.clone();
                props.connect_released(move |_, _, x, y| {
                    if let Some(bar) = this.upgrade() {
                        let point = btn
                            .compute_point(
                                &bar.handle,
                                &gtk4::graphene::Point::new(x as f32, y as f32),
                            )
                            .unwrap_or_else(|| gtk4::graphene::Point::new(x as f32, y as f32));
                        bar.open_tab_menu(&id, point.x() as f64, point.y() as f64);
                    }
                });
                button.add_controller(props);
            }

            let label = gtk4::Label::builder()
                .label(&tab.title)
                .ellipsize(gtk4::pango::EllipsizeMode::End)
                .hexpand(true)
                .xalign(0.0)
                .build();
            let inner = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            inner.append(&label);
            if cfg.show_close {
                let close = gtk4::Button::builder()
                    .icon_name("window-close-symbolic")
                    .has_frame(false)
                    .build();
                close.add_css_class("tab-close");
                let this = Rc::downgrade(self);
                let id = tab.id.clone();
                close.connect_clicked(move |_| {
                    if let Some(bar) = this.upgrade() {
                        bar.emit(TabBarEvent::Close(id.clone()));
                    }
                });
                inner.append(&close);
            }
            button.set_child(Some(&inner));

            let this = Rc::downgrade(self);
            let id = tab.id.clone();
            button.connect_clicked(move |btn| {
                // A toggle rebuilt from state: clicking the active tab keeps it on.
                if !btn.is_active() {
                    btn.set_active(true);
                }
                if let Some(bar) = this.upgrade() {
                    bar.emit(TabBarEvent::Select(id.clone()));
                }
            });

            // Drag source: the payload is the tab's session id.
            let drag = gtk4::DragSource::builder()
                .actions(gtk4::gdk::DragAction::MOVE)
                .content(&gtk4::gdk::ContentProvider::for_value(&tab.id.to_value()))
                .build();
            // Capture phase, like the claim gesture below: once the claim
            // wins the sequence, events keep flowing only to that phase.
            drag.set_propagation_phase(gtk4::PropagationPhase::Capture);
            {
                let id = tab.id.clone();
                drag.connect_prepare(move |_, x, y| {
                    tracing::debug!("tab drag: prepare at {x},{y}");
                    Some(gtk4::gdk::ContentProvider::for_value(&id.to_value()))
                });
            }
            drag.connect_drag_begin(|_, _| tracing::debug!("tab drag: begin"));
            drag.connect_drag_end(|_, _, delete| tracing::debug!("tab drag: end delete={delete}"));
            drag.connect_drag_cancel(|_, _, reason| {
                tracing::debug!("tab drag: cancelled ({reason:?})");
                false
            });
            button.add_controller(drag.clone());

            // Claim the sequence for the tab as soon as real motion starts,
            // so nothing above (empty-space window drag, future ancestors)
            // competes; grouping with the DragSource keeps the DnD gesture
            // alive through the claim (an ungrouped claim would cancel it
            // too). Plain clicks (no motion) are untouched.
            let claim = gtk4::GestureDrag::new();
            claim.set_propagation_phase(gtk4::PropagationPhase::Capture);
            claim.connect_drag_update(|g, dx, dy| {
                if dx.abs() > 3.0 || dy.abs() > 3.0 {
                    g.set_state(gtk4::EventSequenceState::Claimed);
                }
            });
            button.add_controller(claim.clone());
            claim.group_with(&drag);

            // Drop target: dropping tab A onto tab B inserts A at B's slot.
            let drop =
                gtk4::DropTarget::new(gtk4::glib::types::Type::STRING, gtk4::gdk::DragAction::MOVE);
            let this = Rc::downgrade(self);
            let target_id = tab.id.clone();
            drop.connect_drop(move |_, value, _, _| {
                tracing::debug!("tab drop on {target_id}: {value:?}");
                let Ok(dragged) = value.get::<String>() else {
                    return false;
                };
                let Some(bar) = this.upgrade() else {
                    return false;
                };
                if dragged == target_id {
                    return false;
                }
                let index = bar
                    .tabs
                    .borrow()
                    .iter()
                    .position(|t| t.id == target_id)
                    .unwrap_or(0);
                bar.emit(TabBarEvent::Reorder(dragged.clone(), index));
                true
            });
            button.add_controller(drop);

            self.container.append(&button);
        }

        let plus = gtk4::Button::builder()
            .icon_name("list-add-symbolic")
            .has_frame(false)
            .build();
        plus.add_css_class("tab-new");
        let this = Rc::downgrade(self);
        plus.connect_clicked(move |_| {
            if let Some(bar) = this.upgrade() {
                bar.emit(TabBarEvent::NewTab);
            }
        });
        self.container.append(&plus);
    }
}

/// Dragging EMPTY bar space moves the frameless window. Bubble phase: any
/// interactive child (tab DnD, buttons, group rows) that consumed the drag
/// wins first, so this can never steal from them — the inverse of the
/// GtkWindowHandle race this replaced. A pick() check additionally refuses
/// presses that started on any button, even if that button let go of the
/// sequence.
pub(crate) fn wire_window_drag(target: &impl IsA<gtk4::Widget>) {
    let drag = gtk4::GestureDrag::new();
    drag.connect_drag_update(|g, dx, dy| {
        if dx.abs() < 8.0 && dy.abs() < 8.0 {
            return;
        }
        let Some(widget) = g.widget() else { return };
        let Some((sx, sy)) = g.start_point() else {
            return;
        };
        let over_button = widget
            .pick(sx, sy, gtk4::PickFlags::DEFAULT)
            .map(|mut w| loop {
                if w.is::<gtk4::Button>() {
                    break true;
                }
                match w.parent() {
                    Some(p) => w = p,
                    None => break false,
                }
            })
            .unwrap_or(false);
        if over_button {
            g.set_state(gtk4::EventSequenceState::Denied);
            return;
        }
        let Some(root) = widget.root() else { return };
        let Some(surface) = root.surface() else { return };
        let Ok(toplevel) = surface.downcast::<gtk4::gdk::Toplevel>() else {
            return;
        };
        let Some(device) = g.device() else { return };
        let point = widget
            .compute_point(&root, &gtk4::graphene::Point::new(sx as f32, sy as f32))
            .unwrap_or_else(|| gtk4::graphene::Point::new(sx as f32, sy as f32));
        tracing::debug!("empty-space drag: begin window move");
        g.set_state(gtk4::EventSequenceState::Claimed);
        toplevel.begin_move(
            &device,
            1,
            point.x() as f64,
            point.y() as f64,
            g.current_event_time(),
        );
    });
    target.as_ref().add_controller(drag);
}

pub(crate) fn orientation(edge: Edge) -> gtk4::Orientation {
    match edge {
        Edge::Top | Edge::Bottom => gtk4::Orientation::Horizontal,
        Edge::Left | Edge::Right => gtk4::Orientation::Vertical,
    }
}

fn edge_class(edge: Edge) -> &'static str {
    match edge {
        Edge::Top => "edge-top",
        Edge::Bottom => "edge-bottom",
        Edge::Left => "edge-left",
        Edge::Right => "edge-right",
    }
}

pub(crate) fn reveal_transition(edge: Edge) -> gtk4::RevealerTransitionType {
    match edge {
        Edge::Top => gtk4::RevealerTransitionType::SlideDown,
        Edge::Bottom => gtk4::RevealerTransitionType::SlideUp,
        Edge::Left => gtk4::RevealerTransitionType::SlideRight,
        Edge::Right => gtk4::RevealerTransitionType::SlideLeft,
    }
}
