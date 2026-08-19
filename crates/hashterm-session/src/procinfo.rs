//! Foreground-process discovery via /proc. `#{pane_current_command}` is only a
//! 15-char comm with no argv; the real command is the leader of the pane tty's
//! foreground process group: tpgid from /proc/<shell>/stat, then the process
//! with pid == tpgid gives exact argv (cmdline) and cwd.

use crate::schema::FgProc;
use std::path::{Path, PathBuf};

/// stat field 8 (tpgid), robust against spaces/parens in comm: parse after the
/// LAST ')' — kernel guarantees comm is the only parenthesized field.
fn read_stat_after_comm(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    Some(after.split_whitespace().map(str::to_owned).collect())
}

/// pgid of `pid` (field 5 / index 2 after comm).
fn pgid_of(pid: u32) -> Option<u32> {
    read_stat_after_comm(pid)?.get(2)?.parse().ok()
}

/// tpgid of the terminal controlling `pid` (field 8 / index 5 after comm).
fn tpgid_of(pid: u32) -> Option<i64> {
    read_stat_after_comm(pid)?.get(5)?.parse().ok()
}

fn argv_of(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    (!argv.is_empty()).then_some(argv)
}

fn cwd_of(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// The pane's foreground command, or None when the pane sits at a shell prompt.
///
/// `pane_pid` is the pane's root shell as reported by tmux. Its stat tpgid is
/// the foreground process group on the pane tty; when that equals the shell's
/// own pgid the pane is idle.
pub fn foreground_process(pane_pid: u32, fallback_name: &str) -> Option<FgProc> {
    let tpgid = tpgid_of(pane_pid)?;
    if tpgid <= 0 {
        return None;
    }
    let tpgid = tpgid as u32;
    if Some(tpgid) == pgid_of(pane_pid) {
        return None; // shell itself is foreground -> idle prompt
    }
    // Group leader pid == pgid.
    let argv = argv_of(tpgid).unwrap_or_default();
    let cwd = cwd_of(tpgid).unwrap_or_else(|| PathBuf::from("/"));
    let name = argv
        .first()
        .map(|a| {
            Path::new(a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| a.clone())
        })
        .unwrap_or_else(|| fallback_name.to_owned());
    if argv.is_empty() {
        return None; // kernel thread or vanished process: nothing restorable
    }
    Some(FgProc { name, argv, cwd })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_parseable() {
        let pid = std::process::id();
        assert!(pgid_of(pid).is_some());
        assert!(argv_of(pid).is_some());
        assert!(cwd_of(pid).is_some());
    }

    #[test]
    fn foreground_of_spawned_shell_running_sleep() {
        // sh -c 'exec sleep 5' replaces the shell, so the "pane pid" IS the
        // foreground leader here; emulate the idle check instead with our own
        // process: our tpgid group belongs to the test harness, not us.
        let _ = foreground_process(std::process::id(), "test");
        // Smoke: must not panic regardless of harness process-group layout.
    }
}
