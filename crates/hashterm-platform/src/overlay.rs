//! Overlay backend boundary. One universal rule: `hashterm --toggle` (via
//! single-instance D-Bus) is the entry point everywhere; backends only add
//! stacking/placement/grabs on top. The slide animation is NOT here — it is a
//! client-side content translation (SlidePanel in hashterm-ui) identical on all
//! backends; backends merely map/unmap the surface at final geometry.

use hashterm_core::config::Dropdown;
use hashterm_core::state::WindowGeometry;
use std::cell::RefCell;

/// Holder for the remembered geometry a backend consults in `place_and_show`.
#[derive(Default)]
pub struct SavedSlot(RefCell<Option<WindowGeometry>>);

impl SavedSlot {
    pub fn set(&self, g: Option<WindowGeometry>) {
        *self.0.borrow_mut() = g;
    }
    pub fn get(&self) -> Option<WindowGeometry> {
        *self.0.borrow()
    }
}

/// Window size for the dropdown on a monitor of `mon_w` x `mon_h`: the
/// remembered width/height where present, else the configured ones.
pub(crate) fn resolve_size(
    cfg: &Dropdown,
    mon_w: i32,
    mon_h: i32,
    saved: Option<&WindowGeometry>,
) -> (u32, u32) {
    let w = match saved.and_then(|s| s.width) {
        Some(w) => w.clamp(200.min(mon_w), mon_w) as u32,
        None => (mon_w as f64 * cfg.width_pct) as u32,
    };
    let h = match saved.and_then(|s| s.height) {
        Some(h) => h.clamp(120.min(mon_h), mon_h) as u32,
        None => crate::x11::resolve_height(cfg, mon_h),
    };
    (w, h)
}

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("overlay backend unavailable: {0}")]
    Unavailable(String),
    #[error("hotkey grab failed: {0}")]
    HotkeyGrab(String),
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u8 {
        const ALWAYS_ON_TOP        = 1 << 0;
        const EDGE_POSITIONING     = 1 << 1;
        const MONITOR_SELECT       = 1 << 2;
        const IN_PROCESS_HOTKEY    = 1 << 3;
        const SKIP_TASKBAR         = 1 << 4;
    }
}

pub trait OverlayBackend {
    /// Must run before the window is first realized (layer-shell requirement).
    fn prepare(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) -> Result<(), OverlayError>;
    /// Map the surface at its final geometry (placement, stacking, monitor).
    fn place_and_show(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown);
    fn hide(&self, win: &gtk4::ApplicationWindow);
    /// Remembered geometry to prefer over the configured one at the next
    /// `place_and_show`. Backends that cannot honor a part ignore it.
    fn set_saved_geometry(&self, _g: Option<WindowGeometry>) {}
    /// Current top-left of the window in root coordinates, where the platform
    /// can report it (X11/XWayland). None = unknown (native Wayland).
    fn position(&self, _win: &gtk4::ApplicationWindow) -> Option<(i32, i32)> {
        None
    }
    /// Move the window if the platform allows it and (x, y) is on-screen.
    fn move_to(&self, _win: &gtk4::ApplicationWindow, _x: i32, _y: i32) {}
    /// Pin an already-mapped window above others without touching geometry
    /// (startup path; layer surfaces are inherently above, so default no-op).
    fn keep_above(&self, _win: &gtk4::ApplicationWindow) {}
    /// Give an already-visible window keyboard focus (hotkey pressed while
    /// the console is shown but unfocused). Default: plain present().
    fn activate(&self, win: &gtk4::ApplicationWindow) {
        use gtk4::prelude::GtkWindowExt;
        win.present();
    }
    fn capabilities(&self) -> Capabilities;
    fn name(&self) -> &'static str;
}

/// Fallback for platforms with no placement/stacking control (e.g. GNOME
/// Wayland): a normal window the compositor places; animation and focus-loss
/// hiding still work, everything else is honestly absent.
#[derive(Default)]
pub struct PlainWindowOverlay {
    saved: SavedSlot,
}

impl OverlayBackend for PlainWindowOverlay {
    fn set_saved_geometry(&self, g: Option<WindowGeometry>) {
        self.saved.set(g);
    }

    fn prepare(&self, _win: &gtk4::ApplicationWindow, _cfg: &Dropdown) -> Result<(), OverlayError> {
        Ok(())
    }

    fn place_and_show(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) {
        use gtk4::prelude::*;
        if let Some(surface) = win.surface()
            && let Some(monitor) = WidgetExt::display(win).monitor_at_surface(&surface)
        {
            let geo = monitor.geometry();
            let (w, h) = resolve_size(cfg, geo.width(), geo.height(), self.saved.get().as_ref());
            win.set_default_size(w as i32, h as i32);
        }
        win.present();
    }

    fn hide(&self, win: &gtk4::ApplicationWindow) {
        use gtk4::prelude::*;
        win.set_visible(false);
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::empty()
    }

    fn name(&self) -> &'static str {
        "plain-window"
    }
}
