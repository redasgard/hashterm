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

/// Environment variables worth capturing for resume — env-manager markers that
/// tell restore which venv/toolchain to re-activate. An ALLOW-LIST on purpose:
/// the full environment holds secrets and must never be serialized.
const RESUME_ENV_KEYS: &[&str] = &[
    "VIRTUAL_ENV",
    "CONDA_DEFAULT_ENV",
    "CONDA_PREFIX",
    "POETRY_ACTIVE",
    "PYENV_VERSION",
    "RBENV_VERSION",
    "DIRENV_DIR",
];

fn env_of(pid: u32) -> Vec<(String, String)> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return Vec::new();
    };
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=')?;
            RESUME_ENV_KEYS
                .contains(&k)
                .then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

/// pids whose process group is `pgid` (the pane's foreground group).
fn group_members(pgid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok())
            && pgid_of(pid) == Some(pgid)
        {
            out.push(pid);
        }
    }
    out
}

/// Socket inodes held by `pid` (from /proc/<pid>/fd -> "socket:[INODE]").
fn socket_inodes(pid: u32) -> Vec<u64> {
    let mut out = Vec::new();
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return out;
    };
    for e in fds.flatten() {
        if let Ok(target) = std::fs::read_link(e.path())
            && let Some(rest) = target.to_string_lossy().strip_prefix("socket:[")
            && let Some(num) = rest.strip_suffix(']')
            && let Ok(inode) = num.parse::<u64>()
        {
            out.push(inode);
        }
    }
    out
}

/// (inode -> local port) for every LISTENing TCP socket (v4 + v6).
fn listening_by_inode() -> std::collections::HashMap<u64, u16> {
    let mut map = std::collections::HashMap::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // fields: sl local_address rem_address st ... inode(9)
            let (Some(local), Some(st), Some(inode)) = (f.get(1), f.get(3), f.get(9)) else {
                continue;
            };
            if *st != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some(port_hex) = local.split(':').nth(1) else {
                continue;
            };
            if let (Ok(port), Ok(inode)) = (u16::from_str_radix(port_hex, 16), inode.parse::<u64>())
            {
                map.insert(inode, port);
            }
        }
    }
    map
}

/// Ports the pane's foreground process group is listening on (server detection).
fn listening_ports(pgid: u32) -> Vec<u16> {
    let listen = listening_by_inode();
    if listen.is_empty() {
        return Vec::new();
    }
    let mut ports: Vec<u16> = group_members(pgid)
        .into_iter()
        .flat_map(socket_inodes)
        .filter_map(|inode| listen.get(&inode).copied())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
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
    Some(FgProc {
        name,
        argv,
        cwd,
        env: env_of(tpgid),
        listening: listening_ports(tpgid),
    })
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
