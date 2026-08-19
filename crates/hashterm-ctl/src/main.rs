//! Invoked by tmux `run-shell` hooks: `hashterm-ctl event <name> [session]`.
//! Serializes a TmuxEvent as one JSON line to the GUI's unix socket and exits.
//! Failures are silent-but-logged-to-stderr: a hook must never break tmux.

use hashterm_core::ipc::{TmuxEvent, socket_path};
use std::io::Write;
use std::os::unix::net::UnixStream;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let event = match parse(&args) {
        Some(e) => e,
        None => {
            eprintln!("usage: hashterm-ctl event <name> [session]");
            std::process::exit(2);
        }
    };
    let path = socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        // GUI not running: nothing to notify, exit quietly.
        return;
    };
    let mut line = serde_json::to_string(&event).expect("event serializes");
    line.push('\n');
    if let Err(e) = stream.write_all(line.as_bytes()) {
        eprintln!("hashterm-ctl: write to {}: {e}", path.display());
    }
}

fn parse(args: &[String]) -> Option<TmuxEvent> {
    if args.first().map(String::as_str) != Some("event") {
        return None;
    }
    let name = args.get(1)?.as_str();
    let session = || args.get(2).cloned().unwrap_or_default();
    Some(match name {
        "session-created" => TmuxEvent::SessionCreated { session: session() },
        "session-closed" => TmuxEvent::SessionClosed { session: session() },
        "session-renamed" => TmuxEvent::SessionRenamed { session: session() },
        "client-attached" => TmuxEvent::ClientAttached { session: session() },
        "client-detached" => TmuxEvent::ClientDetached { session: session() },
        "bell" => TmuxEvent::Bell { session: session() },
        "activity" => TmuxEvent::Activity { session: session() },
        "new-tab" => TmuxEvent::NewTab,
        "next-tab" => TmuxEvent::NextTab,
        "prev-tab" => TmuxEvent::PrevTab,
        _ => return None,
    })
}
