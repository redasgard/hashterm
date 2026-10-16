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

/// What the global hotkey is really doing right now (the portal answers
/// asynchronously, so the first value is usually `Pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyStatus {
    Pending,
    Active {
        source: HotkeySourceKind,
        trigger: String,
    },
    /// The desktop itself runs `hashterm --toggle` for this key (a custom
    /// shortcut in its keyboard settings): works, just not through hashterm.
    External {
        trigger: String,
    },
    /// Registered, but with a known limitation.
    Degraded {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

impl HotkeyStatus {
    /// One sentence for Settings, the log and `--doctor`.
    pub fn describe(&self) -> String {
        match self {
            Self::Pending => "Waiting for the desktop to confirm the shortcut…".into(),
            Self::Active {
                source: HotkeySourceKind::Portal,
                trigger,
            } => format!("Active through the desktop portal ({trigger})"),
            Self::Active { trigger, .. } => format!("Active through an X11 key grab ({trigger})"),
            Self::External { trigger } => format!(
                "Active: your desktop runs `hashterm --toggle` on {trigger} (custom shortcut)"
            ),
            Self::Degraded { reason } => format!("Limited: {reason}"),
            Self::Unavailable { reason } => format!(
                "Not active: {reason}. Bind `hashterm --toggle` to a key in your \
                 desktop's keyboard settings."
            ),
        }
    }

    /// Worth telling the user about unprompted.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::Degraded { .. } | Self::Unavailable { .. })
    }
}

/// The key of a GNOME custom shortcut whose command runs `hashterm --toggle`.
/// Read-only; `None` on other desktops or when there is none.
pub fn external_toggle_binding() -> Option<String> {
    use gtk4::gio::prelude::*;
    let src = gtk4::gio::SettingsSchemaSource::default()?;
    let list_schema = "org.gnome.settings-daemon.plugins.media-keys";
    let item_schema = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
    src.lookup(list_schema, true)?;
    src.lookup(item_schema, true)?;
    let list = gtk4::gio::Settings::new(list_schema).strv("custom-keybindings");
    list.iter().find_map(|path| {
        let item = gtk4::gio::Settings::with_path(item_schema, path.as_str());
        is_toggle_command(item.string("command").as_str())
            .then(|| item.string("binding").to_string())
            .filter(|b| !b.is_empty())
    })
}

/// `hashterm --toggle`, with or without a path or extra flags.
pub fn is_toggle_command(cmd: &str) -> bool {
    let mut words = cmd.split_whitespace();
    let bin = words.next().unwrap_or("");
    (bin == "hashterm" || bin.ends_with("/hashterm")) && words.any(|w| w == "--toggle")
}

/// An X11 key grab only sees keys while an X11 client has focus when the
/// session itself is Wayland (GNOME on Wayland runs hashterm under XWayland).
pub fn x11_grab_status(session_type: Option<&str>, trigger: &str) -> HotkeyStatus {
    if session_type == Some("wayland") {
        HotkeyStatus::Degraded {
            reason: "this is a Wayland session, so an X11 key grab only works while an X11 \
                     window has focus"
                .into(),
        }
    } else {
        HotkeyStatus::Active {
            source: HotkeySourceKind::X11Grab,
            trigger: trigger.into(),
        }
    }
}

fn session_type() -> Option<String> {
    std::env::var("XDG_SESSION_TYPE").ok()
}

/// Try portal first, then X11 grab; report what actually worked so the UI can
/// show setup instructions for External. `on_status` hears every change,
/// including the asynchronous portal answer.
pub fn register_toggle_hotkey(
    accel: &str,
    is_x11: bool,
    on_activate: Rc<dyn Fn()>,
    on_status: Rc<dyn Fn(HotkeyStatus)>,
) -> HotkeySourceKind {
    // Portal binding is async (consent dialog); optimistically start it. If
    // the bind conclusively fails, the async path falls back to an X11 grab
    // (X11 sessions) or reports the key as unavailable.
    if portal_available() {
        on_status(HotkeyStatus::Pending);
        spawn_portal_bind(accel.to_owned(), is_x11, on_activate, on_status);
        return HotkeySourceKind::Portal;
    }
    if is_x11 && x11_grab(accel, on_activate) {
        on_status(x11_grab_status(session_type().as_deref(), accel));
        return HotkeySourceKind::X11Grab;
    }
    on_status(HotkeyStatus::Unavailable {
        reason: if std::env::var_os("HASHTERM_NO_PORTAL").is_some() {
            "the portal is disabled (HASHTERM_NO_PORTAL) and no X11 grab is possible".into()
        } else {
            "no desktop portal and no X11 session".into()
        },
    });
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
    /// The desktop accepted the shortcut; carries its trigger description.
    Bound(String),
    /// Bind failed or the activation stream ended: portal path is dead.
    Failed(String),
}

fn spawn_portal_bind(
    accel: String,
    is_x11: bool,
    on_activate: Rc<dyn Fn()>,
    on_status: Rc<dyn Fn(HotkeyStatus)>,
) {
    // Portal futures are Send (zbus internal executor); results are forwarded
    // to the GTK main thread through an async channel.
    let (tx, rx) = async_channel::unbounded::<PortalMsg>();
    let trigger = accel_to_portal_trigger(&accel);

    std::thread::Builder::new()
        .name("hashterm-portal".into())
        .spawn(move || {
            if let Err(e) = async_io_block_on(portal_loop(trigger, tx.clone())) {
                tracing::warn!("GlobalShortcuts portal failed: {e}");
                let _ = tx.send_blocking(PortalMsg::Failed(e.to_string()));
            }
        })
        .expect("spawn portal thread");

    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            match msg {
                PortalMsg::Activated => on_activate(),
                PortalMsg::Bound(desc) => {
                    on_status(HotkeyStatus::Active {
                        source: HotkeySourceKind::Portal,
                        trigger: if desc.is_empty() { accel.clone() } else { desc },
                    });
                }
                PortalMsg::Failed(why) => {
                    if is_x11 && x11_grab(&accel, on_activate.clone()) {
                        tracing::info!("global hotkey fell back to X11 grab ({why})");
                        on_status(x11_grab_status(session_type().as_deref(), &accel));
                    } else {
                        tracing::warn!(
                            "no in-process global hotkey ({why}); bind `hashterm --toggle` \
                             to {accel} in your desktop's keyboard settings"
                        );
                        on_status(HotkeyStatus::Unavailable {
                            reason: format!("the portal refused it ({why})"),
                        });
                    }
                    return;
                }
            }
        }
    });
}

/// Does the GlobalShortcuts portal answer on the session bus? (`--doctor`;
/// asks for the interface only, binds nothing.) Returns its version.
pub fn probe_global_shortcuts_portal() -> Result<u32, String> {
    if std::env::var_os("HASHTERM_NO_PORTAL").is_some() {
        return Err("disabled by HASHTERM_NO_PORTAL".into());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("hashterm-doctor".into())
        .spawn(move || {
            let r: Result<u32, String> = async_io_block_on(async {
                let g = ashpd::desktop::global_shortcuts::GlobalShortcuts::new()
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(g.version())
            });
            let _ = tx.send(r);
        });
    if let Err(e) = spawned {
        return Err(e.to_string());
    }
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or_else(|_| Err("no answer within 3 s".into()))
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
    if bound.shortcuts().is_empty() {
        let _ = tx
            .send(PortalMsg::Failed(
                "the desktop did not grant the shortcut".into(),
            ))
            .await;
        return Ok(());
    }
    for s in bound.shortcuts() {
        let _ = tx
            .send(PortalMsg::Bound(s.trigger_description().to_string()))
            .await;
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
    use super::*;

    #[test]
    fn portal_probe_is_cheap() {
        let _ = super::portal_available();
    }

    #[test]
    fn toggle_command_detection() {
        assert!(is_toggle_command("hashterm --toggle"));
        assert!(is_toggle_command("/usr/bin/hashterm --toggle"));
        assert!(!is_toggle_command("hashterm"));
        assert!(!is_toggle_command("echo hashterm --toggle"));
        assert!(!is_toggle_command(""));
    }

    #[test]
    fn x11_grab_on_a_wayland_session_is_degraded() {
        assert!(x11_grab_status(Some("wayland"), "F12").needs_attention());
        assert_eq!(
            x11_grab_status(Some("x11"), "F12"),
            HotkeyStatus::Active {
                source: HotkeySourceKind::X11Grab,
                trigger: "F12".into()
            }
        );
        assert!(!x11_grab_status(None, "F12").needs_attention());
    }

    #[test]
    fn status_text_tells_the_user_what_to_do() {
        let un = HotkeyStatus::Unavailable { reason: "x".into() };
        assert!(un.describe().contains("hashterm --toggle"));
        assert!(un.needs_attention());
        assert!(!HotkeyStatus::Pending.needs_attention());
        assert!(
            HotkeyStatus::Active {
                source: HotkeySourceKind::Portal,
                trigger: "F12".into()
            }
            .describe()
            .contains("portal")
        );
    }
}
