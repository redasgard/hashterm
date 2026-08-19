//! `hashterm restore-pane` implementation: runs INSIDE a freshly created tmux
//! pane. Streams the saved scrollback to stdout (the pane pty — tmux ingests
//! it into history with colors intact), prints a separator, resets SGR state,
//! then exec()s the target program or shell so no wrapper process lingers.

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;

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
                Ok(mut dec) => {
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
    let err = if let Some(argv) = exec.filter(|a| !a.is_empty()) {
        std::process::Command::new(&argv[0]).args(&argv[1..]).exec()
    } else {
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
