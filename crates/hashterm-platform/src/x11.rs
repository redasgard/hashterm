//! X11 overlay backend: a normal managed window (never override-redirect —
//! that is where tilda's focus bugs come from) pinned above all others via
//! EWMH `_NET_WM_STATE_ABOVE`, skipping taskbar/pager, placed one-shot per
//! show with ConfigureWindow on the XID. The slide animation itself is
//! client-side (SlidePanel) and never touches the XID per frame.

use crate::overlay::{Capabilities, OverlayBackend, OverlayError, SavedSlot, resolve_size};
use gtk4::prelude::*;
use hashterm_core::config::{Dropdown, Edge, MonitorPick};
use hashterm_core::state::WindowGeometry;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};

#[derive(Default)]
pub struct X11Overlay {
    saved: SavedSlot,
}

const ADD_STATE: u32 = 1; // _NET_WM_STATE_ADD

impl X11Overlay {
    fn xid(win: &gtk4::ApplicationWindow) -> Option<u32> {
        let surface = win.surface()?;
        let x11 = surface.downcast_ref::<gdk4_x11::X11Surface>()?;
        u32::try_from(x11.xid()).ok()
    }

    /// Top-left of the client area in root coordinates.
    fn query_position(xid: u32) -> Option<(i32, i32)> {
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen_num].root;
        let t = conn
            .translate_coordinates(xid, root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((i32::from(t.dst_x), i32::from(t.dst_y)))
    }

    fn apply_ewmh(
        xid: u32,
        geometry: Option<(i32, i32, u32, u32)>,
        activate: bool,
    ) -> Result<(), OverlayError> {
        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| OverlayError::Unavailable(e.to_string()))?;
        let root = conn.setup().roots[screen_num].root;
        let err = |e: x11rb::errors::ConnectionError| OverlayError::Unavailable(e.to_string());
        let rerr = |e: x11rb::errors::ReplyError| OverlayError::Unavailable(e.to_string());

        let atom = |name: &str| -> Result<xproto::Atom, OverlayError> {
            Ok(conn
                .intern_atom(false, name.as_bytes())
                .map_err(err)?
                .reply()
                .map_err(rerr)?
                .atom)
        };
        let wm_state = atom("_NET_WM_STATE")?;
        let above = atom("_NET_WM_STATE_ABOVE")?;
        let skip_taskbar = atom("_NET_WM_STATE_SKIP_TASKBAR")?;
        let skip_pager = atom("_NET_WM_STATE_SKIP_PAGER")?;

        // State changes on a mapped window go as client messages to the root.
        for pair in [[above, skip_taskbar], [skip_pager, 0]] {
            let event = xproto::ClientMessageEvent::new(
                32,
                xid,
                wm_state,
                [
                    ADD_STATE, pair[0], pair[1], 1, /* normal app source */
                    0,
                ],
            );
            conn.send_event(
                false,
                root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(err)?;
        }

        if let Some((x, y, w, h)) = geometry {
            conn.configure_window(
                xid,
                &xproto::ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(w)
                    .height(h),
            )
            .map_err(err)?;
        }

        // The hotkey press "belongs" to the compositor, so mutter's
        // focus-stealing prevention denies a plain present() focus. An
        // explicit _NET_ACTIVE_WINDOW request with source=pager (2) is the
        // sanctioned override (what `wmctrl -a` sends) — without it the
        // dropdown appears unfocused and hide_on_focus_loss kills it at once.
        if activate {
            let active = atom("_NET_ACTIVE_WINDOW")?;
            let event = xproto::ClientMessageEvent::new(
                32,
                xid,
                active,
                [2 /* pager source */, x11rb::CURRENT_TIME, 0, 0, 0],
            );
            conn.send_event(
                false,
                root,
                xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(err)?;
        }
        conn.flush().map_err(err)?;
        Ok(())
    }
}

/// Console height in px: explicit cfg.height ("600px"/"45%") wins, else the
/// height_pct fraction of the monitor. Clamped to the monitor.
pub(crate) fn resolve_height(cfg: &Dropdown, monitor_h: i32) -> u32 {
    use hashterm_core::config::{SizeSpec, parse_size};
    let h = match cfg.height.as_deref().and_then(parse_size) {
        Some(SizeSpec::Px(px)) => px,
        Some(SizeSpec::Pct(p)) => (monitor_h as f64 * p) as i32,
        None => (monitor_h as f64 * cfg.height_pct) as i32,
    };
    h.clamp(120, monitor_h) as u32
}

/// True when a window at (x, y) of size w x h shows enough of itself on some
/// monitor (a title-strip-sized overlap) to be grabbed and dragged back —
/// guards against restoring onto a monitor that has since been unplugged.
pub(crate) fn position_visible(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    monitors: &[(i32, i32, i32, i32)],
) -> bool {
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let ox = (x + w).min(mx + mw) - x.max(mx);
        let oy = (y + h).min(my + mh) - y.max(my);
        ox >= 100.min(w) && oy >= 40.min(h)
    })
}

fn monitor_rects(display: &gtk4::gdk::Display) -> Vec<(i32, i32, i32, i32)> {
    let monitors = display.monitors();
    (0..monitors.n_items())
        .filter_map(|i| monitors.item(i)?.downcast::<gtk4::gdk::Monitor>().ok())
        .map(|m| {
            let g = m.geometry();
            (g.x(), g.y(), g.width(), g.height())
        })
        .collect()
}

/// Pixel geometry for the dropdown on the chosen monitor. A remembered size
/// replaces the configured one; a remembered, still-visible position replaces
/// the edge anchoring.
pub(crate) fn dropdown_geometry(
    win: &gtk4::ApplicationWindow,
    cfg: &Dropdown,
    saved: Option<&WindowGeometry>,
) -> Option<(i32, i32, u32, u32)> {
    let display = WidgetExt::display(win);
    let monitor = pick_monitor(&display, &cfg.monitor)?;
    let geo = monitor.geometry();
    let (w, h) = resolve_size(cfg, geo.width(), geo.height(), saved);
    if let Some(WindowGeometry {
        x: Some(sx),
        y: Some(sy),
        ..
    }) = saved
        && position_visible(*sx, *sy, w as i32, h as i32, &monitor_rects(&display))
    {
        return Some((*sx, *sy, w, h));
    }
    let (x, y) = match cfg.edge {
        Edge::Top => (geo.x() + (geo.width() - w as i32) / 2, geo.y()),
        Edge::Bottom => (
            geo.x() + (geo.width() - w as i32) / 2,
            geo.y() + geo.height() - h as i32,
        ),
        Edge::Left => (geo.x(), geo.y() + (geo.height() - h as i32) / 2),
        Edge::Right => (
            geo.x() + geo.width() - w as i32,
            geo.y() + (geo.height() - h as i32) / 2,
        ),
    };
    Some((x, y, w, h))
}

pub(crate) fn pick_monitor(
    display: &gtk4::gdk::Display,
    pick: &MonitorPick,
) -> Option<gtk4::gdk::Monitor> {
    let monitors = display.monitors();
    let list: Vec<gtk4::gdk::Monitor> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i)?.downcast::<gtk4::gdk::Monitor>().ok())
        .collect();
    match pick {
        MonitorPick::Connector(name) => list
            .iter()
            .find(|m| m.connector().as_deref() == Some(name))
            .or(list.first())
            .cloned(),
        // gdk4 dropped an explicit primary-monitor API; first monitor is the
        // conventional stand-in. Focused = monitor under the pointer, which
        // only X11 can query portably; fall back to first elsewhere.
        MonitorPick::Primary | MonitorPick::Focused => list.first().cloned(),
    }
}

impl OverlayBackend for X11Overlay {
    fn prepare(&self, _win: &gtk4::ApplicationWindow, _cfg: &Dropdown) -> Result<(), OverlayError> {
        Ok(())
    }

    fn place_and_show(&self, win: &gtk4::ApplicationWindow, cfg: &Dropdown) {
        let saved = self.saved.get();
        if let Some((_, _, w, h)) = dropdown_geometry(win, cfg, saved.as_ref()) {
            win.set_default_size(w as i32, h as i32);
        }
        win.present();
        // EWMH needs the realized XID; run after the surface exists.
        let win2 = win.clone();
        let cfg2 = cfg.clone();
        gtk4::glib::idle_add_local_once(move || {
            let Some(xid) = Self::xid(&win2) else {
                tracing::warn!("x11 overlay: no XID after present");
                return;
            };
            let Some(geo) = dropdown_geometry(&win2, &cfg2, saved.as_ref()) else {
                return;
            };
            if let Err(e) = Self::apply_ewmh(xid, Some(geo), true) {
                tracing::warn!("x11 overlay hints failed: {e}");
            }
        });
    }

    fn hide(&self, win: &gtk4::ApplicationWindow) {
        win.set_visible(false);
    }

    fn set_saved_geometry(&self, g: Option<WindowGeometry>) {
        self.saved.set(g);
    }

    fn position(&self, win: &gtk4::ApplicationWindow) -> Option<(i32, i32)> {
        Self::query_position(Self::xid(win)?)
    }

    fn move_to(&self, win: &gtk4::ApplicationWindow, x: i32, y: i32) {
        let win = win.clone();
        // present() maps the window asynchronously and the toolkit / window
        // manager place it on map, racing a one-shot move. So: wait until it
        // is mapped, then (re)request the spot until it is actually reported
        // there (bounded, ~6 s). Once there, we stop — never fight the user.
        let mut tries = 0;
        gtk4::glib::timeout_add_local(std::time::Duration::from_millis(60), move || {
            tries += 1;
            let give_up = tries >= 100;
            let flow = |done: bool| {
                if done || give_up {
                    gtk4::glib::ControlFlow::Break
                } else {
                    gtk4::glib::ControlFlow::Continue
                }
            };
            let xid = Self::xid(&win).filter(|_| win.is_mapped());
            let Some(xid) = xid else {
                return flow(false);
            };
            if Self::query_position(xid) == Some((x, y)) {
                return flow(true);
            }
            let (w, h) = (win.width().max(1), win.height().max(1));
            if !position_visible(x, y, w, h, &monitor_rects(&WidgetExt::display(&win))) {
                return flow(true);
            }
            if let Ok((conn, _)) = x11rb::connect(None) {
                let _ = conn.configure_window(xid, &xproto::ConfigureWindowAux::new().x(x).y(y));
                let _ = conn.flush();
            }
            flow(false)
        });
    }

    fn keep_above(&self, win: &gtk4::ApplicationWindow) {
        let win = win.clone();
        gtk4::glib::idle_add_local_once(move || {
            if let Some(xid) = Self::xid(&win)
                && let Err(e) = Self::apply_ewmh(xid, None, false)
            {
                tracing::warn!("x11 keep-above failed: {e}");
            }
        });
    }

    fn activate(&self, win: &gtk4::ApplicationWindow) {
        // _NET_ACTIVE_WINDOW (pager source) beats focus-stealing prevention;
        // a plain present() would be ignored as untimestamped.
        if let Some(xid) = Self::xid(win) {
            if let Err(e) = Self::apply_ewmh(xid, None, true) {
                tracing::warn!("x11 activate failed: {e}");
            }
        } else {
            win.present();
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::ALWAYS_ON_TOP
            | Capabilities::EDGE_POSITIONING
            | Capabilities::MONITOR_SELECT
            | Capabilities::IN_PROCESS_HOTKEY
            | Capabilities::SKIP_TASKBAR
    }

    fn name(&self) -> &'static str {
        "x11-ewmh"
    }
}

#[cfg(test)]
mod tests {
    use super::position_visible;

    const MON: [(i32, i32, i32, i32); 2] = [(0, 0, 1920, 1080), (1920, 0, 1280, 1024)];

    #[test]
    fn on_screen_is_visible() {
        assert!(position_visible(100, 100, 800, 600, &MON));
        assert!(position_visible(2000, 50, 800, 600, &MON));
    }

    #[test]
    fn straddling_monitors_is_visible() {
        assert!(position_visible(1500, 100, 800, 600, &MON));
    }

    #[test]
    fn unplugged_monitor_is_not_visible() {
        assert!(!position_visible(3300, 100, 800, 600, &MON));
        assert!(!position_visible(100, 2000, 800, 600, &MON));
        assert!(!position_visible(-780, 100, 800, 600, &MON)); // only 20px peeking in
    }

    #[test]
    fn no_monitors_is_not_visible() {
        assert!(!position_visible(0, 0, 800, 600, &[]));
    }
}
