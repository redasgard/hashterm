//! Wayland layer-shell overlay backend (wlroots family + KDE; GNOME's mutter
//! deliberately does not implement the protocol). The window becomes a layer
//! surface anchored to the configured edge; keyboard mode OnDemand keeps
//! normal focus semantics so hide-on-focus-loss works.
//!
//! Hard constraint: `init_for_window` must run BEFORE the window is first
//! realized — hence the `prepare()` phase of the backend trait.

use crate::overlay::{Capabilities, OverlayBackend, OverlayError, SavedSlot, resolve_size};
use gtk4::prelude::*;
use gtk4_layer_shell::{Edge as LsEdge, KeyboardMode, Layer, LayerShell};
use hashterm_core::config::{Dropdown, DropdownLayer, Edge};
use hashterm_core::state::WindowGeometry;

/// Layer surfaces are anchored by the compositor, so only the remembered SIZE
/// applies; position is never restored here.
#[derive(Default)]
pub struct LayerShellOverlay {
    saved: SavedSlot,
}

impl OverlayBackend for LayerShellOverlay {
    fn set_saved_geometry(&self, g: Option<WindowGeometry>) {
        self.saved.set(g);
    }

    fn prepare(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) -> Result<(), OverlayError> {
        if !gtk4_layer_shell::is_supported() {
            return Err(OverlayError::Unavailable(
                "compositor lacks zwlr_layer_shell_v1".into(),
            ));
        }
        win.init_layer_shell();
        win.set_layer(match cfg.layer {
            DropdownLayer::Top => Layer::Top,
            DropdownLayer::Overlay => Layer::Overlay,
        });
        win.set_keyboard_mode(KeyboardMode::OnDemand);
        // Anchor only the configured edge: the unanchored axis centers.
        for (ls_edge, ours) in [
            (LsEdge::Top, Edge::Top),
            (LsEdge::Bottom, Edge::Bottom),
            (LsEdge::Left, Edge::Left),
            (LsEdge::Right, Edge::Right),
        ] {
            win.set_anchor(ls_edge, cfg.edge == ours);
        }
        win.set_exclusive_zone(if cfg.respect_panels { 0 } else { -1 });
        win.set_namespace(Some("hashterm"));
        Ok(())
    }

    fn place_and_show(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) {
        if let Some(monitor) = crate::x11::pick_monitor(&WidgetExt::display(win), &cfg.monitor) {
            win.set_monitor(Some(&monitor));
            let geo = monitor.geometry();
            let (w, h) = resolve_size(cfg, geo.width(), geo.height(), self.saved.get().as_ref());
            win.set_default_size(w as i32, h as i32);
        }
        win.present();
    }

    fn hide(&self, win: &gtk4::ApplicationWindow) {
        win.set_visible(false);
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::ALWAYS_ON_TOP
            | Capabilities::EDGE_POSITIONING
            | Capabilities::MONITOR_SELECT
            | Capabilities::SKIP_TASKBAR
    }

    fn name(&self) -> &'static str {
        "wayland-layer-shell"
    }
}
