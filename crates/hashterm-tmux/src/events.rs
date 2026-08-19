//! Unix-socket event bus: tmux hooks invoke `hashterm-ctl event ...`, which
//! writes one JSON line to our socket. A listener thread parses lines into
//! TmuxEvents and forwards them over an async-channel whose receiver the GUI
//! awaits inside glib::spawn_future_local. GTK-free.

use hashterm_core::ipc::TmuxEvent;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The largest TmuxEvent JSON line is well under this; anything longer is
/// hostile (slow-loris / memory exhaustion) and the connection is dropped.
const MAX_LINE: u64 = 4096;
/// Events buffered before back-pressure applies to senders (hooks).
const CHANNEL_CAP: usize = 256;

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

        let (tx, rx) = async_channel::bounded(CHANNEL_CAP);
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
    let self_uid = current_uid();
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        // Only accept peers with our own UID (defense in depth beyond the
        // 0700 dir): a co-UID caller is the trust boundary we accept.
        if let (Some(peer), Some(me)) = (peer_uid(&stream), self_uid)
            && peer != me
        {
            tracing::warn!("rejecting ipc connection from uid {peer}");
            continue;
        }
        // hashterm-ctl sends one short line and exits; a per-connection
        // thread means a slow/hostile sender can't stall the accept loop.
        let tx = tx.clone();
        std::thread::spawn(move || handle_conn(stream, &tx));
    }
}

fn handle_conn(stream: UnixStream, tx: &async_channel::Sender<TmuxEvent>) {
    // Cap the read so an endless line can't exhaust memory; one line is enough.
    let mut reader = BufReader::new(stream.take(MAX_LINE));
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {}
        Ok(_) => match serde_json::from_str::<TmuxEvent>(line.trim_end()) {
            Ok(event) => {
                let _ = tx.send_blocking(event);
            }
            Err(e) => tracing::warn!("ignoring malformed ipc line: {e}"),
        },
        Err(e) => tracing::warn!("ipc read error (line too long?): {e}"),
    }
}

fn current_uid() -> Option<u32> {
    // SAFETY: getuid() takes no arguments and cannot fail.
    Some(unsafe { libc::getuid() })
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid fd, correctly-sized ucred out-param and matching len.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
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

        // One line per connection (the real hashterm-ctl protocol).
        let send = |line: &[u8]| {
            let mut c = UnixStream::connect(&path).unwrap();
            c.write_all(line).unwrap();
        };
        send(b"{\"event\":\"bell\",\"session\":\"ht-1\"}\n");
        send(b"not json\n"); // malformed: dropped
        send(b"{\"event\":\"new-tab\"}\n");

        // Per-connection threads mean arrival order isn't guaranteed; assert
        // the set of the two valid events.
        let mut got = std::collections::HashSet::new();
        got.insert(format!("{:?}", bus.receiver.recv_blocking().unwrap()));
        got.insert(format!("{:?}", bus.receiver.recv_blocking().unwrap()));
        assert!(got.contains(&format!(
            "{:?}",
            TmuxEvent::Bell {
                session: "ht-1".into()
            }
        )));
        assert!(got.contains(&format!("{:?}", TmuxEvent::NewTab)));

        let sock = bus.path.clone();
        drop(bus);
        assert!(!sock.exists(), "socket cleaned up on drop");
    }
}
