//! Dump pipeline: interrogate the dedicated tmux server with `-F` format
//! strings and stream per-pane scrollback through zstd into a save directory.
//! Panes/windows/sessions are addressed by immutable ids ($n/@n/%n) throughout;
//! names and indices can change mid-dump, ids cannot.

use crate::procinfo;
use crate::schema::*;
use crate::store::{SessionStore, StoreError};
use hashterm_tmux::controller::{
    ACCENT_OPTION, GROUP_COLOR_OPTION, GROUP_OPTION, TITLE_CUSTOM_OPTION, TITLE_OPTION,
};
use hashterm_tmux::{TmuxController, TmuxError};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum DumpError {
    #[error(transparent)]
    Tmux(#[from] TmuxError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unparseable tmux line: {0}")]
    Parse(String),
    #[error("nothing to dump: no hashterm sessions on the server")]
    Empty,
}

#[derive(Debug, Clone, Default)]
pub struct DumpOptions {
    /// "title:GLOB" / "cmd:GLOB" patterns whose panes skip scrollback capture.
    pub exclude_panes: Vec<String>,
    /// 0 = full history.
    pub max_scrollback_lines: u32,
}

/// Dump every `ht-*` session into a new save. Returns the manifest.
pub fn dump_server(
    ctl: &TmuxController,
    store: &SessionStore,
    name: &str,
    auto: bool,
    opts: &DumpOptions,
) -> Result<DumpManifest, DumpError> {
    let dir = store.begin(name, auto)?;
    let manifest = build_manifest(ctl, &dir, opts)?;
    SessionStore::write_manifest(&dir, &manifest)?;
    store.commit(&dir, name, auto)?;
    Ok(manifest)
}

fn build_manifest(
    ctl: &TmuxController,
    dir: &Path,
    opts: &DumpOptions,
) -> Result<DumpManifest, DumpError> {
    let tmux_version = version_string();

    // Largest attached client, for -x/-y at restore; sane default when headless.
    let client_size = ctl
        .run(&["list-clients", "-F", "#{client_width}\t#{client_height}"])
        .ok()
        .and_then(|out| {
            out.lines()
                .filter_map(|l| {
                    let (w, h) = l.split_once('\t')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .max()
        })
        .unwrap_or((200, 50));

    // Sessions: $id -> (name, custom flag, accent, group data, active window,
    // title LAST — the only field that may contain tabs).
    let fmt = format!(
        "#{{session_id}}\t#{{session_name}}\t#{{active_window_index}}\t#{{{TITLE_CUSTOM_OPTION}}}\t#{{{ACCENT_OPTION}}}\t#{{{GROUP_COLOR_OPTION}}}\t#{{{GROUP_OPTION}}}\t#{{{TITLE_OPTION}}}"
    );
    let mut sessions: BTreeMap<String, SessionDump> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in ctl.run(&["list-sessions", "-F", &fmt])?.lines() {
        let mut p = line.splitn(8, '\t');
        let (
            Some(id),
            Some(name),
            Some(active),
            Some(custom),
            Some(accent),
            Some(group_color),
            Some(group),
        ) = (
            p.next(),
            p.next(),
            p.next(),
            p.next(),
            p.next(),
            p.next(),
            p.next(),
        )
        else {
            return Err(DumpError::Parse(line.into()));
        };
        if !name.starts_with(hashterm_tmux::controller::SESSION_PREFIX) {
            continue;
        }
        order.push(id.to_owned());
        sessions.insert(
            id.to_owned(),
            SessionDump {
                name: name.to_owned(),
                title: p.next().unwrap_or("").to_owned(),
                title_custom: custom == "1",
                accent: (!accent.is_empty()).then(|| accent.to_owned()),
                group: (!group.is_empty()).then(|| group.to_owned()),
                group_color: (!group_color.is_empty()).then(|| group_color.to_owned()),
                active_window: active.parse().unwrap_or(0),
                windows: Vec::new(),
            },
        );
    }
    if sessions.is_empty() {
        return Err(DumpError::Empty);
    }

    // Windows: keyed by @id so panes can attach; window_layout is verbatim
    // (its leading 4-hex checksum is tmux's own and validates at restore).
    let mut windows: BTreeMap<String, (String, WindowDump)> = BTreeMap::new();
    let mut window_order: Vec<String> = Vec::new();
    let wfmt = "#{session_id}\t#{window_id}\t#{window_index}\t#{window_active}\t#{window_layout}\t#{window_name}";
    for line in ctl.run(&["list-windows", "-a", "-F", wfmt])?.lines() {
        let f: Vec<&str> = line.splitn(6, '\t').collect();
        let [sid, wid, idx, active, layout, wname] = f.as_slice() else {
            return Err(DumpError::Parse(line.into()));
        };
        if !sessions.contains_key(*sid) {
            continue;
        }
        window_order.push((*wid).to_owned());
        windows.insert(
            (*wid).to_owned(),
            (
                (*sid).to_owned(),
                WindowDump {
                    index: idx.parse().unwrap_or(0),
                    name: (*wname).to_owned(),
                    layout: (*layout).to_owned(),
                    active: *active == "1",
                    panes: Vec::new(),
                },
            ),
        );
    }

    // Panes. pane_current_path can contain tabs, so it is the LAST field.
    let pfmt = "#{session_id}\t#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_pid}\t#{pane_current_command}\t#{alternate_on}\t#{pane_in_mode}\t#{q:pane_title}\t#{pane_current_path}";
    // One /proc + /proc/net snapshot for the whole dump (not per pane).
    let scan = procinfo::ProcScan::new();
    for line in ctl.run(&["list-panes", "-a", "-F", pfmt])?.lines() {
        let f: Vec<&str> = line.splitn(11, '\t').collect();
        let [
            sid,
            wid,
            pid,
            idx,
            active,
            shell_pid,
            cmd,
            alt,
            in_mode,
            title,
            cwd,
        ] = f.as_slice()
        else {
            return Err(DumpError::Parse(line.into()));
        };
        let Some((_, window)) = windows.get_mut(*wid) else {
            continue;
        };
        debug_assert!(sessions.contains_key(*sid));

        let fg = shell_pid
            .parse::<u32>()
            .ok()
            .and_then(|p| procinfo::foreground_process_scanned(&scan, p, cmd));
        let alternate_on = *alt == "1";

        let scrollback = if excluded(opts, title, cmd) || alternate_on {
            // Alternate-screen panes (vim, htop): replay would show garbage;
            // program restart covers them instead.
            None
        } else {
            match capture_scrollback(ctl, pid, dir, opts.max_scrollback_lines) {
                Ok(r) => Some(r),
                Err(e) => {
                    // Pane may have vanished mid-dump: keep going without it.
                    tracing::warn!("scrollback capture failed for {pid}: {e}");
                    None
                }
            }
        };

        window.panes.push(PaneDump {
            index: idx.parse().unwrap_or(0),
            active: *active == "1",
            title: unquote_title(title),
            cwd: (*cwd).into(),
            alternate_on,
            in_mode: *in_mode == "1",
            fg,
            scrollback,
        });
    }

    // Assemble: windows into their sessions, in tmux listing order.
    for wid in window_order {
        if let Some((sid, w)) = windows.remove(&wid)
            && let Some(s) = sessions.get_mut(&sid)
            && !w.panes.is_empty()
        {
            s.windows.push(w);
        }
    }

    Ok(DumpManifest {
        schema_version: SCHEMA_VERSION,
        created_at: now_rfc3339(),
        created_local: now_local_display(),
        tmux_version,
        client_size,
        sessions: order
            .into_iter()
            .filter_map(|sid| sessions.remove(&sid))
            .filter(|s| !s.windows.is_empty())
            .collect(),
    })
}

fn excluded(opts: &DumpOptions, title: &str, cmd: &str) -> bool {
    opts.exclude_panes.iter().any(|pat| {
        if let Some(glob) = pat.strip_prefix("title:") {
            glob_match(glob, title)
        } else if let Some(glob) = pat.strip_prefix("cmd:") {
            glob_match(glob, cmd)
        } else {
            false
        }
    })
}

/// Minimal glob: '*' wildcards only.
fn glob_match(pat: &str, text: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == text;
    }
    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(found) => {
                if i == 0 && found != 0 {
                    return false; // pattern doesn't start with '*': must anchor
                }
                pos += found + part.len();
            }
            None => return false,
        }
    }
    parts.last() == Some(&"") || text.ends_with(parts.last().unwrap())
}

/// `capture-pane -epJ -S - -E -`: full history + visible screen, SGR colors
/// kept (-e), wrapped lines joined (-J) so replay reflows at any width.
/// Streamed straight into a zstd-3 encoder; never buffered whole.
fn capture_scrollback(
    ctl: &TmuxController,
    pane_id: &str,
    dir: &Path,
    max_lines: u32,
) -> Result<ScrollbackRef, DumpError> {
    let start = if max_lines == 0 {
        "-".to_owned()
    } else {
        format!("-{max_lines}")
    };
    let mut child = Command::new("tmux")
        .args([
            "-L",
            ctl.socket(),
            "capture-pane",
            "-epJ",
            "-S",
            &start,
            "-E",
            "-",
            "-t",
            pane_id,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let rel = format!("panes/{}.scrollback.zst", pane_id.trim_start_matches('%'));
    let out_path = dir.join(&rel);
    // Create 0600 up-front: scrollback may hold secrets, never even briefly
    // world-readable (was create-then-chmod).
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&out_path)?;
    let mut enc = zstd::Encoder::new(file, 3)?;

    let mut lines: u64 = 0;
    let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    for line in reader.split(b'\n') {
        let mut line = line?;
        line.push(b'\n');
        enc.write_all(&line)?;
        lines += 1;
    }
    enc.finish()?;
    let status = child.wait()?;
    if !status.success() {
        std::fs::remove_file(&out_path).ok();
        return Err(DumpError::Parse(format!(
            "capture-pane {pane_id} exited {status}"
        )));
    }
    Ok(ScrollbackRef {
        file: rel.into(),
        lines,
    })
}

/// #{q:pane_title} backslash-escapes #,",',$ and backslash itself.
fn unquote_title(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    let mut chars = q.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // A trailing lone backslash is literal, not an escape lead-in.
            match chars.next() {
                Some(n) => out.push(n),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn version_string() -> String {
    Command::new("tmux")
        .arg("-V")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_start_matches("tmux ")
                .to_owned()
        })
        .unwrap_or_default()
}

/// RFC3339 UTC from the system clock, no chrono dependency.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_to_rfc3339(secs)
}

/// Autosave directory name: filesystem-safe sortable timestamp.
pub fn now_stamp() -> String {
    now_rfc3339().replace(':', "-")
}

/// Human-friendly LOCAL time ("YYYY-MM-DD HH:MM") for display. Stored at dump
/// time (the dumping process is in the user's timezone) so the picker/prompt
/// never has to show UTC.
pub fn now_local_display() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid time_t in, valid tm out; localtime_r is thread-safe.
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return epoch_to_rfc3339(secs as u64);
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
    )
}

fn epoch_to_rfc3339(secs: u64) -> String {
    // Civil-from-days algorithm (Howard Hinnant), UTC only.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(epoch_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(1_755_129_600), "2025-08-14T00:00:00Z");
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("secret*", "secret-notes"));
        assert!(!glob_match("secret*", "my-secret"));
        assert!(glob_match("*pass*", "1password"));
        assert!(glob_match("gpg", "gpg"));
        assert!(!glob_match("gpg", "gpg2"));
    }

    #[test]
    fn title_unquote() {
        assert_eq!(unquote_title(r#"a\"b\\c"#), r#"a"b\c"#);
    }
}
