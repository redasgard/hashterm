//! Overlay backend boundary. One universal rule: `hashterm --toggle` (via
//! single-instance D-Bus) is the entry point everywhere; backends only add
//! stacking/placement/grabs on top. The slide animation is NOT here — it is a
//! client-side content translation (SlidePanel in hashterm-ui) identical on all
//! backends; backends merely map/unmap the surface at final geometry.

use hashterm_core::config::Dropdown;

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
pub struct PlainWindowOverlay;

impl OverlayBackend for PlainWindowOverlay {
    fn prepare(&self, _win: &gtk4::ApplicationWindow, _cfg: &Dropdown) -> Result<(), OverlayError> {
        Ok(())
    }

    fn place_and_show(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) {
        use gtk4::prelude::*;
        if let Some(surface) = win.surface()
            && let Some(monitor) = WidgetExt::display(win).monitor_at_surface(&surface)
        {
            let geo = monitor.geometry();
            win.set_default_size(
                (geo.width() as f64 * cfg.width_pct) as i32,
                crate::x11::resolve_height(cfg, geo.height()) as i32,
            );
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
