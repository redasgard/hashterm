//! Unix-socket event bus: tmux hooks invoke `hashterm-ctl event ...`, which
//! writes one JSON line to our socket. A listener thread parses lines into
//! TmuxEvents and forwards them over an async-channel whose receiver the GUI
//! awaits inside glib::spawn_future_local. GTK-free.

use hashterm_core::ipc::TmuxEvent;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

pub struct TmuxEventBus {
    pub receiver: async_channel::Receiver<TmuxEvent>,
    path: PathBuf,
}

impl TmuxEventBus {
    /// Bind the socket (replacing any stale one) and spawn the accept loop.
    pub fn start(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = std::fs::remove_file(path); // stale socket from a dead instance
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

        let (tx, rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("hashterm-ipc".into())
            .spawn(move || accept_loop(listener, tx))?;
        Ok(Self {
            receiver: rx,
            path: path.to_owned(),
        })
    }
}

impl Drop for TmuxEventBus {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(listener: UnixListener, tx: async_channel::Sender<TmuxEvent>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // hashterm-ctl sends one line and exits; handle inline, no per-conn thread.
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            match serde_json::from_str::<TmuxEvent>(&line) {
                Ok(event) => {
                    if tx.send_blocking(event).is_err() {
                        return; // GUI gone: stop listening
                    }
                }
                Err(e) => tracing::warn!("ignoring malformed ipc line: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    #[test]
    fn events_flow_through_socket() {
        let path = std::env::temp_dir()
            .join(format!("hashterm-bus-test-{}", std::process::id()))
            .join("ipc.sock");
        let bus = TmuxEventBus::start(&path).unwrap();

        let mut c1 = UnixStream::connect(&path).unwrap();
        c1.write_all(b"{\"event\":\"bell\",\"session\":\"ht-1\"}\n")
            .unwrap();
        drop(c1);
        let mut c2 = UnixStream::connect(&path).unwrap();
        c2.write_all(b"not json\n{\"event\":\"new-tab\"}\n")
            .unwrap();
        drop(c2);

        let ev1 = bus.receiver.recv_blocking().unwrap();
        assert_eq!(
            ev1,
            TmuxEvent::Bell {
                session: "ht-1".into()
            }
        );
        // Malformed line skipped, valid one after it still delivered.
        let ev2 = bus.receiver.recv_blocking().unwrap();
        assert_eq!(ev2, TmuxEvent::NewTab);

        let sock = bus.path.clone();
        drop(bus);
        assert!(!sock.exists(), "socket cleaned up on drop");
    }
}
