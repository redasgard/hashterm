//! Global-hotkey sources, tried in order:
//! 1. xdg-desktop-portal GlobalShortcuts (works on GNOME/KDE/Hyprland, both
//!    display servers; the compositor shows a one-time consent dialog).
//! 2. XGrabKey via the global-hotkey crate (X11 only).
//! 3. External: the user binds `hashterm --toggle` in their DE/compositor —
//!    always available, surfaced as instructions, no code here.
//!
//! Every source ultimately fires the same callback on the GTK main thread.

use futures_util::StreamExt;
use gtk4::glib;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeySourceKind {
    Portal,
    X11Grab,
    External,
}

/// Try portal first, then X11 grab; report what actually worked so the UI can
/// show setup instructions for External.
pub fn register_toggle_hotkey(
    accel: &str,
    is_x11: bool,
    on_activate: Rc<dyn Fn()>,
) -> HotkeySourceKind {
    // Portal binding is async (consent dialog); optimistically start it. If
    // the bind conclusively fails, the async path falls back to an X11 grab
    // (X11 sessions) or logs external-bind instructions.
    if portal_available() {
        spawn_portal_bind(accel.to_owned(), is_x11, on_activate);
        return HotkeySourceKind::Portal;
    }
    if is_x11 && x11_grab(accel, on_activate) {
        return HotkeySourceKind::X11Grab;
    }
    HotkeySourceKind::External
}

fn portal_available() -> bool {
    // HASHTERM_NO_PORTAL=1 forces the X11-grab/external path (tests, broken
    // portal setups).
    if std::env::var_os("HASHTERM_NO_PORTAL").is_some() {
        return false;
    }
    // Cheap synchronous probe: the portal service file exists on the session
    // bus when any implementation is installed. The real test is the bind.
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| std::path::Path::new(&d).join("bus").exists())
            .unwrap_or(false)
}

/// GTK accel ("<Ctrl><Shift>F12" / "F12") -> portal trigger ("CTRL+SHIFT+F12").
fn accel_to_portal_trigger(accel: &str) -> Option<String> {
    let (key, mods) = gtk4::accelerator_parse(accel)?;
    let mut parts: Vec<&str> = Vec::new();
    if mods.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        parts.push("CTRL");
    }
    if mods.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        parts.push("SHIFT");
    }
    if mods.contains(gtk4::gdk::ModifierType::ALT_MASK) {
        parts.push("ALT");
    }
    if mods.contains(gtk4::gdk::ModifierType::SUPER_MASK) {
        parts.push("LOGO");
    }
    let name = key.name()?;
    let mut trigger = parts.join("+");
    if !trigger.is_empty() {
        trigger.push('+');
    }
    trigger.push_str(&name);
    Some(trigger)
}

enum PortalMsg {
    Activated,
    /// Bind failed or the activation stream ended: portal path is dead.
    Failed,
}

fn spawn_portal_bind(accel: String, is_x11: bool, on_activate: Rc<dyn Fn()>) {
    // Portal futures are Send (zbus internal executor); results are forwarded
    // to the GTK main thread through an async channel.
    let (tx, rx) = async_channel::unbounded::<PortalMsg>();
    let trigger = accel_to_portal_trigger(&accel);

    std::thread::Builder::new()
        .name("hashterm-portal".into())
        .spawn(move || {
            if let Err(e) = async_io_block_on(portal_loop(trigger, tx.clone())) {
                tracing::warn!("GlobalShortcuts portal failed: {e}");
                let _ = tx.send_blocking(PortalMsg::Failed);
            }
        })
        .expect("spawn portal thread");

    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            match msg {
                PortalMsg::Activated => on_activate(),
                PortalMsg::Failed => {
                    if is_x11 && x11_grab(&accel, on_activate.clone()) {
                        tracing::info!("global hotkey fell back to X11 grab");
                    } else {
                        tracing::warn!(
                            "no in-process global hotkey; bind `hashterm --toggle` \
                             to {accel} in your desktop's keyboard settings"
                        );
                    }
                    return;
                }
            }
        }
    });
}

fn async_io_block_on<F: std::future::Future>(fut: F) -> F::Output {
    async_io_shim::block_on(fut)
}

// ashpd's async-io feature pulls the async-io crate transitively; a tiny shim
// avoids adding it as a direct dependency just for block_on.
mod async_io_shim {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    pub fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = pin!(fut);
        let waker = Arc::new(ThreadWaker(std::thread::current())).into();
        let mut cx = Context::from_waker(&waker);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => std::thread::park(),
            }
        }
    }
}

/// GNOME's portal resolves the caller's app id from its systemd scope name
/// (`app-…-<AppID>-….scope`). A hashterm launched from a plain terminal lives
/// in the shell's scope -> empty app id -> "Discarded shortcut bind request".
/// Fix: move our own PID into a correctly-named transient scope first.
async fn ensure_app_scope() {
    use ashpd::zbus;
    let result: zbus::Result<()> = async {
        let conn = zbus::Connection::session().await?;
        let pid = std::process::id();
        let unit = format!(
            "app-hashterm-{APP_ID}-{pid}.scope",
            APP_ID = "com.redasgard.Hashterm"
        );
        let properties: Vec<(&str, zbus::zvariant::Value<'_>)> = vec![("PIDs", vec![pid].into())];
        let aux: Vec<(&str, Vec<(&str, zbus::zvariant::Value<'_>)>)> = vec![];
        conn.call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "StartTransientUnit",
            &(unit.as_str(), "fail", properties, aux),
        )
        .await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => tracing::debug!("moved into dedicated app scope for portal identity"),
        // Already in an app scope (normal desktop launch) or no systemd:
        // portal may still resolve the id; proceed either way.
        Err(e) => tracing::debug!("app scope setup skipped: {e}"),
    }
}

async fn portal_loop(
    trigger: Option<String>,
    tx: async_channel::Sender<PortalMsg>,
) -> ashpd::Result<()> {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};

    ensure_app_scope().await;
    let shortcuts = GlobalShortcuts::new().await?;
    let session = shortcuts.create_session(Default::default()).await?;

    // The shortcut ID encodes the configured trigger. Compositors persist the
    // approved trigger PER ID and ignore a changed preferred_trigger — with a
    // stable id "toggle", editing the hotkey in the config would never take
    // effect globally. A new id looks like a new shortcut: the compositor
    // asks once and binds the new trigger. Activations for other (stale) ids
    // are filtered out below, so the old key goes dead.
    let id = match &trigger {
        Some(t) => format!(
            "toggle-{}",
            t.to_lowercase()
                .replace(|c: char| !c.is_ascii_alphanumeric(), "-")
        ),
        None => "toggle".to_owned(),
    };

    let mut shortcut = NewShortcut::new(id.as_str(), "Toggle the hashterm drop-down terminal");
    if let Some(t) = &trigger {
        shortcut = shortcut.preferred_trigger(t.as_str());
    }
    let request = shortcuts
        .bind_shortcuts(&session, &[shortcut], None, Default::default())
        .await?;
    let bound = request.response()?;
    for s in bound.shortcuts() {
        tracing::info!(
            "portal shortcut bound: {} -> {}",
            s.id(),
            s.trigger_description()
        );
    }

    // A held key makes the compositor re-fire Activated on every auto-repeat.
    // Pair Activated with Deactivated (key release): exactly one toggle per
    // physical press. Safety: if a compositor never sends Deactivated, a >1s
    // gap since the last Activated re-arms (repeats come every few dozen ms).
    enum Ev {
        Act,
        Deact,
    }
    let expect = id.clone();
    let expect2 = id.clone();
    let activated = shortcuts.receive_activated().await?.filter_map(move |a| {
        let ours = a.shortcut_id() == expect;
        if !ours {
            tracing::debug!(
                "ignoring activation of stale shortcut '{}'",
                a.shortcut_id()
            );
        }
        futures_util::future::ready(ours.then_some(Ev::Act))
    });
    let deactivated = shortcuts.receive_deactivated().await?.filter_map(move |d| {
        futures_util::future::ready((d.shortcut_id() == expect2).then_some(Ev::Deact))
    });
    let mut events = futures_util::stream::select(activated, deactivated);

    let mut armed = true;
    let mut last_act = std::time::Instant::now() - std::time::Duration::from_secs(10);
    while let Some(ev) = events.next().await {
        match ev {
            Ev::Act => {
                let now = std::time::Instant::now();
                let stale = now.duration_since(last_act) > std::time::Duration::from_secs(1);
                last_act = now;
                if armed || stale {
                    armed = false;
                    if tx.send(PortalMsg::Activated).await.is_err() {
                        break;
                    }
                } else {
                    tracing::debug!("portal activation ignored (auto-repeat)");
                }
            }
            Ev::Deact => armed = true,
        }
    }
    Ok(())
}

/// XGrabKey path (X11 sessions without a working portal).
fn x11_grab(accel: &str, on_activate: Rc<dyn Fn()>) -> bool {
    use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};

    let Some(hotkey) = accel_to_hotkey(accel) else {
        tracing::warn!("cannot parse hotkey '{accel}' for X11 grab");
        return false;
    };
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("X11 hotkey manager unavailable: {e}");
            return false;
        }
    };
    if let Err(e) = manager.register(hotkey) {
        tracing::warn!("X11 grab of '{accel}' failed (already taken?): {e}");
        return false;
    }
    // The manager must outlive the app or the grab is released.
    std::mem::forget(manager);

    let (tx, rx) = async_channel::unbounded::<()>();
    let receiver = GlobalHotKeyEvent::receiver().clone();
    std::thread::Builder::new()
        .name("hashterm-x11-hotkey".into())
        .spawn(move || {
            // Press/Release pairing: a held key repeats Pressed events; only
            // the first per physical press toggles (>1s gap re-arms as safety).
            let mut armed = true;
            let mut last = std::time::Instant::now() - std::time::Duration::from_secs(10);
            while let Ok(event) = receiver.recv() {
                match event.state() {
                    global_hotkey::HotKeyState::Pressed => {
                        let now = std::time::Instant::now();
                        let stale = now.duration_since(last) > std::time::Duration::from_secs(1);
                        last = now;
                        if !armed && !stale {
                            continue; // auto-repeat of a held key
                        }
                        armed = false;
                        if tx.send_blocking(()).is_err() {
                            break;
                        }
                    }
                    global_hotkey::HotKeyState::Released => armed = true,
                }
            }
        })
        .expect("spawn hotkey thread");
    glib::spawn_future_local(async move {
        while rx.recv().await.is_ok() {
            on_activate();
        }
    });
    true
}

/// GTK accel -> global-hotkey's HotKey.
fn accel_to_hotkey(accel: &str) -> Option<global_hotkey::hotkey::HotKey> {
    use global_hotkey::hotkey::{Code, HotKey, Modifiers};
    let (key, mods) = gtk4::accelerator_parse(accel)?;
    let mut m = Modifiers::empty();
    if mods.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        m |= Modifiers::CONTROL;
    }
    if mods.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        m |= Modifiers::SHIFT;
    }
    if mods.contains(gtk4::gdk::ModifierType::ALT_MASK) {
        m |= Modifiers::ALT;
    }
    if mods.contains(gtk4::gdk::ModifierType::SUPER_MASK) {
        m |= Modifiers::SUPER;
    }
    let name = key.name()?;
    let code: Code = match name.as_str() {
        "F1" => Code::F1,
        "F2" => Code::F2,
        "F3" => Code::F3,
        "F4" => Code::F4,
        "F5" => Code::F5,
        "F6" => Code::F6,
        "F7" => Code::F7,
        "F8" => Code::F8,
        "F9" => Code::F9,
        "F10" => Code::F10,
        "F11" => Code::F11,
        "F12" => Code::F12,
        "grave" => Code::Backquote,
        "space" => Code::Space,
        "Escape" => Code::Escape,
        other if other.len() == 1 && other.chars().next().unwrap().is_ascii_alphabetic() => {
            let c = other.chars().next().unwrap().to_ascii_uppercase();
            // KeyA..KeyZ are contiguous in the Code enum's string form.
            format!("Key{c}").parse().ok()?
        }
        other => other.parse().ok()?,
    };
    Some(HotKey::new(if m.is_empty() { None } else { Some(m) }, code))
}

#[cfg(test)]
mod tests {
    // accel parsing needs GTK initialized; keep to pure-logic assertions.
    #[test]
    fn portal_probe_is_cheap() {
        let _ = super::portal_available();
    }
}
