//! End-to-end resurrect test on a scratch tmux server: create session with
//! split panes and scrollback -> dump -> kill server -> restore -> verify
//! layout, titles, and replayed scrollback survived.

use hashterm_session::{DumpOptions, OnConflict, RestoreOptions, SessionStore};
use hashterm_tmux::controller::{NewSessionOpts, TmuxController};
use std::path::PathBuf;
use std::time::Duration;

fn have_tmux() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok()
}

/// Poll until `f` succeeds or ~5s elapse.
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..50 {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn dump_kill_restore_roundtrip() {
    if !have_tmux() {
        eprintln!("tmux not installed; skipping");
        return;
    }
    let socket = format!("hashterm-e2e-{}", std::process::id());
    let ctl = TmuxController::new(socket.clone(), PathBuf::from("/dev/null"));
    let store_root =
        std::env::temp_dir().join(format!("hashterm-e2e-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_root);
    let store = SessionStore::new(store_root.clone());

    // --- build state: one session, one window, two panes, marker scrollback
    let id = ctl
        .new_session(&NewSessionOpts {
            title: "e2e work".into(),
            cwd: Some("/tmp".into()),
            command: None, // default shell
            group: Some("resq".into()),
        })
        .expect("new session");
    ctl.run(&["send-keys", "-t", &id.0, "echo MARKER_e2e_12345", "Enter"])
        .unwrap();
    ctl.run(&["split-window", "-h", "-t", &id.0, "-c", "/"])
        .unwrap();
    assert!(
        eventually(|| {
            ctl.run(&["capture-pane", "-p", "-t", &format!("{}.0", id.0)])
                .map(|s| s.contains("MARKER_e2e_12345"))
                .unwrap_or(false)
        }),
        "marker never appeared in pane"
    );

    // Group color set before dump: both group fields must survive.
    ctl.set_group_color(&id, Some("#61afef")).unwrap();

    // --- dump
    let manifest =
        hashterm_session::dump_server(&ctl, &store, "e2e", false, &DumpOptions::default())
            .expect("dump");
    assert_eq!(manifest.sessions.len(), 1);
    let saved = &manifest.sessions[0];
    let saved_name = saved.name.clone();
    assert_eq!(saved.title, "e2e work");
    assert_eq!(saved.group.as_deref(), Some("resq"));
    assert_eq!(saved.group_color.as_deref(), Some("#61afef"));
    assert_eq!(saved.windows.len(), 1);
    assert_eq!(saved.windows[0].panes.len(), 2);
    assert!(
        saved.windows[0].layout.contains('x'),
        "layout string present"
    );
    let with_scrollback = saved.windows[0]
        .panes
        .iter()
        .filter(|p| p.scrollback.is_some())
        .count();
    assert_eq!(with_scrollback, 2, "both panes captured");

    // --- kill the server entirely (the "reboot")
    ctl.run(&["kill-server"]).unwrap();
    assert!(ctl.list_sessions().unwrap().is_empty());

    // --- restore
    let opts = RestoreOptions {
        on_conflict: OnConflict::Rename,
        programs: vec![],
        restart_arbitrary: false,
        pretype_unrestored: true,
        shell: Some("sh".into()),
        helper: PathBuf::from(env!("CARGO_BIN_EXE_hashterm")),
    };
    let report =
        hashterm_session::restore_save(&ctl, &store, "e2e", false, &opts).expect("restore");
    assert_eq!(report.restored.len(), 1);
    assert_eq!(report.restored[0].0, saved_name);
    let live_name = report.restored[0].1.clone();

    // Session is back with its title and both panes.
    let sessions = ctl.list_sessions().unwrap();
    let restored = sessions
        .iter()
        .find(|s| s.name == live_name)
        .expect("restored session exists");
    assert_eq!(restored.title, "e2e work");
    assert_eq!(restored.group.as_deref(), Some("resq"), "group survives");
    assert_eq!(
        restored.group_color.as_deref(),
        Some("#61afef"),
        "group color survives"
    );
    let panes = ctl
        .run(&["list-panes", "-t", &live_name, "-F", "#{pane_current_path}"])
        .unwrap();
    assert_eq!(panes.lines().count(), 2, "both panes recreated");

    // Replayed scrollback contains the marker again (restore-pane streamed it).
    assert!(
        eventually(|| {
            ctl.run(&[
                "capture-pane",
                "-p",
                "-S",
                "-",
                "-t",
                &format!("{live_name}.0"),
            ])
            .map(|s| s.contains("MARKER_e2e_12345"))
            .unwrap_or(false)
        }),
        "restored pane never showed replayed marker"
    );

    // --- cleanup
    let _ = ctl.run(&["kill-server"]);
    let _ = std::fs::remove_dir_all(&store_root);
}
