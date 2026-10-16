//! Minimal repro: bare GTK4 window + one VTE widget; feed OSC 0 title
//! changes once per second. Print RSS every 10 titles.
//! Run: cargo run -p hashterm-ui --example title_leak_repro --release

use gtk4::prelude::*;
use vte4::prelude::*;

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

fn main() {
    let app = gtk4::Application::builder()
        .application_id("org.repro.titleleak")
        .build();
    app.connect_activate(|app| {
        let use_pty = std::env::var_os("HT_REPRO_PTY").is_some();
        let win = gtk4::ApplicationWindow::new(app);
        win.set_default_size(400, 200);
        let vte = if use_pty {
            vte4::Terminal::builder()
                .hexpand(true)
                .vexpand(true)
                .scrollback_lines(0)
                .build()
        } else {
            vte4::Terminal::new()
        };
        win.set_child(Some(&vte));
        win.set_visible(true);
        // Mirror hashterm's page.rs wiring: consume each xterm.title change.
        {
            let vte2 = vte.clone();
            vte.connect_termprop_changed(Some("xterm.title"), move |t, prop| {
                let _ = &vte2;
                if let (Some(title), _) = t.termprop_string(prop) {
                    let _clean: String = title
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(256)
                        .collect();
                }
            });
        }
        if use_pty {
            // Real pty child emitting a fresh OSC 0 title every second,
            // mirroring hashterm's tmux-attach path exactly.
            let envv: Vec<&str> = vec![];
            // Grid damage every second -> real VTE renders (this is the case
            // the feed()-only repro never exercised).
            let argv: Vec<&str> = if std::env::var_os("HT_REPRO_DAMAGE").is_some() {
                vec![
                    "bash",
                    "-c",
                    "while true; do echo damage-$(date +%s); sleep 1; done",
                ]
            } else {
                vec!["bash"]
            };
            vte.spawn_async(
                vte4::PtyFlags::DEFAULT,
                None,
                &argv,
                &envv,
                gtk4::glib::SpawnFlags::SEARCH_PATH,
                || {},
                -1,
                gtk4::gio::Cancellable::NONE,
                |result| {
                    if let Err(e) = result {
                        eprintln!("spawn failed: {e}");
                    }
                },
            );
            let n = std::cell::Cell::new(0u64);
            glib_run_pty(&vte, &n);
        } else {
            let n = std::cell::Cell::new(0u64);
            glib_run(&vte, &n);
        }
    });
    app.run();
}

fn glib_run_pty(vte: &vte4::Terminal, n: &std::cell::Cell<u64>) {
    let vte = vte.clone();
    let n = n.clone();
    gtk4::glib::timeout_add_seconds_local(1, move || {
        let i = n.get();
        vte.feed_child(format!("\x1b]0;leak-pty-{}\x07", i).as_bytes());
        n.set(i + 1);
        if i.is_multiple_of(10) {
            println!("pty titles={} rss={}", i, rss_kb());
        }
        if i >= 150 {
            println!("DONE pty titles=150 rss={}", rss_kb());
            return gtk4::glib::ControlFlow::Break;
        }
        gtk4::glib::ControlFlow::Continue
    });
}

fn glib_run(vte: &vte4::Terminal, n: &std::cell::Cell<u64>) {
    let vte = vte.clone();
    let n = n.clone();
    gtk4::glib::timeout_add_seconds_local(1, move || {
        let i = n.get();
        // OSC 0: set window title (changes every time -> real change).
        let seq = format!("\x1b]0;leak-title-{}\x07", i);
        vte.feed(seq.as_bytes());
        n.set(i + 1);
        if i.is_multiple_of(10) {
            println!("titles={} rss={}", i, rss_kb());
        }
        if i >= 150 {
            println!("DONE titles=150 rss={} (baseline was 2312)", rss_kb());
            return gtk4::glib::ControlFlow::Break;
        }
        gtk4::glib::ControlFlow::Continue
    });
}
