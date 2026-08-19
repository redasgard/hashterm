//! `hashterm restore-pane` implementation: runs INSIDE a freshly created tmux
//! pane. Streams the saved scrollback to stdout (the pane pty — tmux ingests
//! it into history with colors intact), prints a separator, resets SGR state,
//! then exec()s the target program or shell so no wrapper process lingers.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Restored programs may only be exec'd from these system directories. A
/// save file is untrusted input; without this a manifest (or a poisoned
/// `$PATH` entry like a writable `~/.local/bin`) could point argv[0] at an
/// attacker-controlled binary.
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
    EXEC_ALLOW
        .iter()
        .any(|dir| real.starts_with(dir))
        .then_some(real)
}

pub fn run_restore_pane(
    scrollback: Option<&Path>,
    shell: Option<&str>,
    exec: Option<&[String]>,
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

    // exec() replaces this process; on success nothing below runs. Resolve
    // and allow-list the target first; a rejected or unresolvable program
    // falls through to a plain shell rather than running something untrusted.
    let allowed = exec
        .filter(|a| !a.is_empty())
        .and_then(|argv| resolve_allowed(&argv[0]).map(|real| (real, argv)));
    let err = if let Some((real, argv)) = allowed {
        std::process::Command::new(real).args(&argv[1..]).exec()
    } else {
        if exec.is_some_and(|a| !a.is_empty()) {
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
