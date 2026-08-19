//! Platform layer: quake-overlay window backends (X11 EWMH / Wayland
//! layer-shell / plain-window fallback) and global-hotkey sources.

pub mod hotkey;
pub mod layershell;
pub mod overlay;
pub mod x11;

pub use hotkey::{HotkeySourceKind, register_toggle_hotkey};
pub use overlay::{Capabilities, OverlayBackend, OverlayError, PlainWindowOverlay};

/// Pick the overlay backend for the current display server. Called before the
/// window is realized so layer-shell can claim it in `prepare()`.
pub fn detect_backend(display: &gtk4::gdk::Display) -> Box<dyn OverlayBackend> {
    use gtk4::glib::object::Cast;
    if display.downcast_ref::<gdk4_x11::X11Display>().is_some() {
        return Box::new(x11::X11Overlay);
    }
    if display
        .downcast_ref::<gdk4_wayland::WaylandDisplay>()
        .is_some()
        && gtk4_layer_shell::is_supported()
    {
        return Box::new(layershell::LayerShellOverlay);
    }
    Box::new(PlainWindowOverlay)
}

/// True when running on an X11 GDK backend (for the XGrabKey hotkey path).
pub fn is_x11(display: &gtk4::gdk::Display) -> bool {
    use gtk4::glib::object::Cast;
    display.downcast_ref::<gdk4_x11::X11Display>().is_some()
}
