//! `hashterm restore-pane` implementation: runs INSIDE a freshly created tmux
//! pane. Streams the saved scrollback to stdout (the pane pty — tmux ingests
//! it into history with colors intact), prints a separator, resets SGR state,
//! then exec()s the target program or shell so no wrapper process lingers.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Restored programs (from an UNtrusted save) may only be exec'd from these
/// system directories or from under the user's own $HOME. A save file is
/// untrusted input; the point is to keep argv[0] out of WORLD-writable dirs
/// (/tmp, /var/tmp, /dev/shm, ...) where a low-privilege attacker could plant
/// a binary. $HOME is fine: exploiting it would already require the ability to
/// write there, i.e. same-user code execution — and it lets a legitimately
/// venv-/`~/.local/bin`-installed tool (jupyter, ipython) resume.
const EXEC_ALLOW: &[&str] = &[
    "/usr/bin",
    "/usr/local/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/opt",
];

/// Resolve argv[0] to a real path under an allow-listed dir, or None.
fn resolve_allowed(arg0: &str) -> Option<PathBuf> {
    let candidate = if arg0.contains('/') {
        PathBuf::from(arg0)
    } else {
        // Bare name: emulate PATH search so we validate what execvp would run.
        std::env::var_os("PATH")
            .map(|path| {
                std::env::split_paths(&path)
                    .map(|d| d.join(arg0))
                    .find(|p| p.is_file())
            })?
            ?
    };
    let real = std::fs::canonicalize(candidate).ok()?;
    let in_system = EXEC_ALLOW.iter().any(|dir| real.starts_with(dir));
    let in_home = under_home(&real);
    if !in_system && !in_home {
        return None;
    }
    // A binary under $HOME (the attacker-plant surface) must at least not be
    // group/world-writable — that rejects a payload dropped with loose modes.
    if in_home && !in_system {
        use std::os::unix::fs::MetadataExt;
        let writable = std::fs::metadata(&real).map(|m| m.mode() & 0o022 != 0).unwrap_or(true);
        if writable {
            return None;
        }
    }
    Some(real)
}

/// A resolved binary sits strictly BELOW $HOME. Rejects an empty/`/` HOME (which
/// would make `starts_with($HOME)` true for every path and void the allow-list)
/// and requires at least one path component beyond HOME (never HOME itself).
fn under_home(real: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    if home.as_os_str().is_empty() || home == Path::new("/") {
        return false;
    }
    let home = std::fs::canonicalize(&home).unwrap_or(home);
    real.strip_prefix(&home)
        .is_ok_and(|rest| rest.components().next().is_some())
}

pub fn run_restore_pane(
    scrollback: Option<&Path>,
    shell: Option<&str>,
    exec: Option<&[String]>,
    // Trusted argv (from user config) skips the exec-dir allow-list; untrusted
    // argv (from a save file) must resolve under a system dir.
    trusted: bool,
) -> std::io::Error {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    if let Some(path) = scrollback {
        match std::fs::File::open(path) {
            Ok(file) => match zstd::Decoder::new(file) {
                Ok(dec) => {
                    // Bound the decompressed stream: a tiny zstd file can
                    // expand to gigabytes (decompression bomb).
                    const MAX_REPLAY_BYTES: u64 = 32 * 1024 * 1024;
                    let mut dec = std::io::Read::take(dec, MAX_REPLAY_BYTES);
                    if let Err(e) = std::io::copy(&mut dec, &mut out) {
                        let _ = writeln!(out, "hashterm: scrollback replay failed: {e}");
                    }
                }
                Err(e) => {
                    let _ = writeln!(out, "hashterm: bad scrollback file: {e}");
                }
            },
            Err(e) => {
                let _ = writeln!(out, "hashterm: cannot open scrollback: {e}");
            }
        }
        // Guard against a truncated capture leaving open escape state, then a
        // dim separator marking where the restored history ends.
        let _ = writeln!(
            out,
            "\x1b[0m\x1b[2m── restored {} ──\x1b[0m",
            crate::dump::now_rfc3339()
        );
        let _ = out.flush();
    }

    // exec() replaces this process; on success nothing below runs.
    let want_exec = exec.filter(|a| !a.is_empty());
    // Trusted (user-config resume recipe): exec argv[0] via normal PATH lookup,
    // no allow-list. Untrusted (save-derived): resolve and require a system dir.
    let target = want_exec.and_then(|argv| {
        if trusted {
            Some((std::path::PathBuf::from(&argv[0]), argv))
        } else {
            resolve_allowed(&argv[0]).map(|real| (real, argv))
        }
    });
    let err = if let Some((prog, argv)) = target {
        std::process::Command::new(prog).args(&argv[1..]).exec()
    } else {
        if want_exec.is_some() {
            eprintln!("hashterm: refusing to exec restored program outside system dirs");
        }
        let sh = shell
            .map(str::to_owned)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".into());
        std::process::Command::new(sh).exec()
    };

    // exec failed (missing binary?): fall back to a plain shell so the user
    // keeps a usable pane instead of an instantly-dead one.
    eprintln!("hashterm: exec failed: {err}; starting /bin/sh");
    std::process::Command::new("/bin/sh").exec()
}
