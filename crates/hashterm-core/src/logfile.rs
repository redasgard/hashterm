//! Log file + panic hook. The desktop launches hashterm with stdout/stderr on
//! /dev/null, so without a file a failed hotkey registration or a crash left
//! no trace at all. The file lives in the XDG state dir (0700 dir, 0600 file),
//! is rotated once at startup (one backup), and never holds terminal content.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

static ENABLED: AtomicBool = AtomicBool::new(true);
static FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// `$XDG_STATE_HOME/hashterm` (default `~/.local/state/hashterm`).
pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".local/state"))
        })
        // No usable home: a fixed per-user spot, never the working directory.
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("hashterm-{}", unsafe { libc::geteuid() }))
        })
        .join("hashterm")
}

pub fn log_path() -> PathBuf {
    state_dir().join("hashterm.log")
}

/// `[general] log_to_file`: applied once the config is loaded.
pub fn set_file_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Rename `path` to `path.1` (replacing an older backup) when it is bigger than `max`.
pub fn rotate_if_big(path: &Path, max: u64) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > max => {
            let mut backup = path.as_os_str().to_owned();
            backup.push(".1");
            std::fs::rename(path, PathBuf::from(backup))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn open_log() -> Option<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = log_path();
    let dir = path.parent()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let _ = rotate_if_big(&path, MAX_BYTES);
    use std::os::unix::fs::PermissionsExt;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        // Never follow a planted symlink at the log path.
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .ok()?;
    // A file or dir left by an older version may be looser than we want.
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Some(file)
}

/// Writer handed to tracing: file (when enabled) and stderr (when it is a
/// terminal, or when there is no file to fall back on).
struct Tee {
    to_stderr: bool,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.to_stderr {
            let _ = std::io::stderr().write_all(buf);
        }
        if ENABLED.load(Ordering::Relaxed)
            && let Some(f) = FILE.get()
            && let Ok(mut f) = f.lock()
        {
            let _ = f.write_all(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Maker {
    to_stderr: bool,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Maker {
    type Writer = Tee;
    fn make_writer(&'a self) -> Tee {
        Tee {
            to_stderr: self.to_stderr,
        }
    }
}

/// Initialize tracing (stderr and the log file), honoring `HASHTERM_LOG`
/// (fallback `RUST_LOG`, then "info"), and install the panic hook.
pub fn init() {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("HASHTERM_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".into());
    let have_file = match open_log() {
        Some(f) => FILE.set(Mutex::new(f)).is_ok(),
        None => false,
    };
    let to_stderr = !have_file || std::io::stderr().is_terminal();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_ansi(false)
        .with_writer(Maker { to_stderr })
        .init();
    if !have_file {
        tracing::warn!("no log file: could not open {}", log_path().display());
    }
    install_panic_hook();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{bt}");
        previous(info);
    }));
}

/// Last `n` lines of the log (for `--doctor --log`).
pub fn tail(n: usize) -> Vec<String> {
    // Bytes, lossily: one non-UTF-8 path must not blank the whole output.
    let Ok(bytes) = std::fs::read(log_path()) else {
        return Vec::new();
    };
    let s = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ht-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rotation_moves_only_a_big_file_and_keeps_one_backup() {
        let d = scratch("rot");
        let p = d.join("hashterm.log");
        std::fs::write(&p, vec![b'x'; 100]).unwrap();
        assert!(!rotate_if_big(&p, 1000).unwrap());
        assert!(p.exists());
        assert!(rotate_if_big(&p, 50).unwrap());
        assert!(!p.exists());
        assert_eq!(std::fs::read(d.join("hashterm.log.1")).unwrap().len(), 100);
        // A second rotation replaces the backup.
        std::fs::write(&p, vec![b'y'; 200]).unwrap();
        assert!(rotate_if_big(&p, 50).unwrap());
        assert_eq!(std::fs::read(d.join("hashterm.log.1")).unwrap().len(), 200);
        // A missing file is fine.
        assert!(!rotate_if_big(&d.join("nope.log"), 1).unwrap());
        let _ = std::fs::remove_dir_all(d);
    }
}
