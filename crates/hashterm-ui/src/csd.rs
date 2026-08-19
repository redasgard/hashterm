//! Client-side decoration helpers for the frameless window: eight invisible
//! resize handles layered over the window content via GtkOverlay, each starting
//! an interactive compositor resize (gdk::Toplevel::begin_resize — works on
//! both X11 and Wayland with no platform branches).

use gtk4::gdk;
use gtk4::prelude::*;

const HANDLE: i32 = 6; // px hit area along each edge

pub fn add_resize_handles(overlay: &gtk4::Overlay, window: &gtk4::ApplicationWindow) {
    use gdk::SurfaceEdge::*;
    let edges: [(gdk::SurfaceEdge, &str, gtk4::Align, gtk4::Align); 8] = [
        (North, "n-resize", gtk4::Align::Fill, gtk4::Align::Start),
        (South, "s-resize", gtk4::Align::Fill, gtk4::Align::End),
        (West, "w-resize", gtk4::Align::Start, gtk4::Align::Fill),
        (East, "e-resize", gtk4::Align::End, gtk4::Align::Fill),
        (
            NorthWest,
            "nw-resize",
            gtk4::Align::Start,
            gtk4::Align::Start,
        ),
        (NorthEast, "ne-resize", gtk4::Align::End, gtk4::Align::Start),
        (SouthWest, "sw-resize", gtk4::Align::Start, gtk4::Align::End),
        (SouthEast, "se-resize", gtk4::Align::End, gtk4::Align::End),
    ];

    for (edge, cursor, halign, valign) in edges {
        let corner = matches!(edge, NorthWest | NorthEast | SouthWest | SouthEast);
        let area = gtk4::Box::builder().halign(halign).valign(valign).build();
        match (halign, valign, corner) {
            (_, _, true) => {
                area.set_width_request(HANDLE * 2);
                area.set_height_request(HANDLE * 2);
            }
            (gtk4::Align::Fill, _, _) => area.set_height_request(HANDLE),
            (_, gtk4::Align::Fill, _) => area.set_width_request(HANDLE),
            _ => {}
        }
        area.set_cursor_from_name(Some(cursor));

        let gesture = gtk4::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        let win = window.downgrade();
        gesture.connect_pressed(move |gesture, _n, x, y| {
            let Some(win) = win.upgrade() else { return };
            let Some(surface) = win.surface() else { return };
            let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() else {
                return;
            };
            let Some(event) = gesture.current_event() else {
                return;
            };
            let device = event.device();
            // Translate the press point from the handle widget to the window.
            let widget = gesture.widget().expect("gesture has widget");
            let point = widget
                .compute_point(&win, &gtk4::graphene::Point::new(x as f32, y as f32))
                .unwrap_or_else(|| gtk4::graphene::Point::new(x as f32, y as f32));
            toplevel.begin_resize(
                edge,
                device.as_ref(),
                gdk::BUTTON_PRIMARY as i32,
                point.x() as f64,
                point.y() as f64,
                event.time(),
            );
            gesture.set_state(gtk4::EventSequenceState::Denied);
        });
        area.add_controller(gesture);
        overlay.add_overlay(&area);
    }
}
