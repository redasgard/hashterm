//! SlidePanel: quake-style reveal/conceal by translating the child's render
//! transform. The child is ALWAYS allocated at the panel's full size — only a
//! GskTransform moves during animation, so the terminal never sees a resize
//! (no SIGWINCH storm) and the same code runs on X11, layer-shell, and plain
//! Wayland windows.

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use hashterm_core::config::Edge;
use std::cell::Cell;

mod imp {
    use super::*;

    pub struct SlidePanel {
        pub child: std::cell::RefCell<Option<gtk4::Widget>>,
        /// 0.0 = fully revealed, 1.0 = fully off-screen.
        pub offset: Cell<f64>,
        pub edge: Cell<Edge>,
        pub tick: Cell<Option<gtk4::TickCallbackId>>,
    }

    impl Default for SlidePanel {
        fn default() -> Self {
            Self {
                child: Default::default(),
                offset: Cell::new(0.0),
                edge: Cell::new(Edge::Top),
                tick: Cell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SlidePanel {
        const NAME: &'static str = "HashtermSlidePanel";
        type Type = super::SlidePanel;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for SlidePanel {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_overflow(gtk4::Overflow::Hidden);
        }

        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for SlidePanel {
        fn measure(&self, orientation: gtk4::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            match &*self.child.borrow() {
                Some(child) => child.measure(orientation, for_size),
                None => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.borrow().clone() else {
                return;
            };
            let off = self.offset.get();
            let (dx, dy) = match self.edge.get() {
                Edge::Top => (0.0, -off * height as f64),
                Edge::Bottom => (0.0, off * height as f64),
                Edge::Left => (-off * width as f64, 0.0),
                Edge::Right => (off * width as f64, 0.0),
            };
            let transform = gtk4::gsk::Transform::new()
                .translate(&gtk4::graphene::Point::new(dx as f32, dy as f32));
            child.allocate(width, height, baseline, Some(transform));
        }
    }
}

glib::wrapper! {
    pub struct SlidePanel(ObjectSubclass<imp::SlidePanel>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl Default for SlidePanel {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl SlidePanel {
    pub fn new(edge: Edge) -> Self {
        let panel: Self = glib::Object::new();
        panel.imp().edge.set(edge);
        panel
    }

    pub fn set_child(&self, child: Option<&impl IsA<gtk4::Widget>>) {
        if let Some(old) = self.imp().child.take() {
            old.unparent();
        }
        if let Some(child) = child {
            child.set_parent(self);
            *self.imp().child.borrow_mut() = Some(child.clone().upcast());
        }
        self.queue_resize();
    }

    pub fn set_edge(&self, edge: Edge) {
        self.imp().edge.set(edge);
        self.queue_allocate();
    }

    /// Jump without animating (used before the first map).
    pub fn set_offset(&self, offset: f64) {
        self.imp().offset.set(offset.clamp(0.0, 1.0));
        self.queue_allocate();
    }

    /// Animate to revealed (offset 0) or concealed (offset 1); ease-out cubic.
    pub fn animate(&self, reveal: bool, duration_ms: u32, on_done: Option<Box<dyn FnOnce()>>) {
        if let Some(id) = self.imp().tick.take() {
            id.remove();
        }
        let start = self.imp().offset.get();
        let end = if reveal { 0.0 } else { 1.0 };
        if duration_ms == 0 || (start - end).abs() < f64::EPSILON {
            self.set_offset(end);
            if let Some(done) = on_done {
                done();
            }
            return;
        }
        let duration_us = (duration_ms as i64) * 1000;
        let started: Cell<Option<i64>> = Cell::new(None);
        let on_done = std::cell::RefCell::new(on_done);
        let id = self.add_tick_callback(move |panel, clock| {
            let now = clock.frame_time();
            let t0 = match started.get() {
                Some(t0) => t0,
                None => {
                    started.set(Some(now));
                    now
                }
            };
            let t = ((now - t0) as f64 / duration_us as f64).clamp(0.0, 1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            panel.imp().offset.set(start + (end - start) * eased);
            panel.queue_allocate();
            if t >= 1.0 {
                panel.imp().tick.set(None);
                if let Some(done) = on_done.borrow_mut().take() {
                    done();
                }
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        self.imp().tick.set(Some(id));
    }
}
