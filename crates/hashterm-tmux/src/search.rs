//! Scrollback search through tmux copy-mode. tmux owns the history (VTE's own
//! buffer is empty), so searching means driving copy-mode on the active pane
//! of the tab's session and reading `#{search_present}` / `#{search_count}`.
//!
//! Facts measured on tmux 3.5a: the search is smart-case (all-lowercase
//! matches capitals too); `search_count` keeps its previous value after a miss
//! in the same copy-mode session, so `search_present` is read first and each
//! new query starts from a fresh copy-mode session.

use crate::controller::{TmuxController, TmuxError, TmuxSessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchResult {
    pub present: bool,
    /// Total matches (tmux >= 3.5); `None` when tmux does not report it.
    pub count: Option<u32>,
}

/// Parse the output of `display-message -p '#{search_present} #{search_count}'`.
pub fn parse_search_info(out: &str) -> SearchResult {
    let mut it = out.split_whitespace();
    let present = it.next() == Some("1");
    let count = it.next().and_then(|c| c.parse().ok());
    SearchResult {
        present,
        // A miss never reports a count, whatever tmux remembered.
        count: if present { count } else { Some(0) },
    }
}

impl TmuxController {
    /// Start (or restart) a search from the bottom of the history. Always
    /// leaves the pane in copy-mode on a hit; cancels it on an empty query.
    pub fn search_start(&self, id: &TmuxSessionId, text: &str) -> Result<SearchResult, TmuxError> {
        serialized(|| self.search_start_locked(id, text))
    }

    fn search_start_locked(
        &self,
        id: &TmuxSessionId,
        text: &str,
    ) -> Result<SearchResult, TmuxError> {
        let t = id.0.as_str();
        // Not in a mode yet is fine: cancel then reports an error we ignore.
        let _ = self.run(&["send-keys", "-t", t, "-X", "cancel"]);
        if text.is_empty() {
            return Ok(SearchResult {
                present: false,
                count: Some(0),
            });
        }
        self.run(&["copy-mode", "-t", t])?;
        // tmux parses a trailing ';' as a command separator and drops it:
        // "\\;" is the escape that keeps it as a literal character.
        let text = match text.strip_suffix(';') {
            Some(head) => format!("{head}\\;"),
            None => text.to_string(),
        };
        self.run(&["send-keys", "-t", t, "-X", "search-backward-text", &text])?;
        self.search_info(id)
    }

    /// Next older match (`reverse` = toward newer ones).
    pub fn search_step(
        &self,
        id: &TmuxSessionId,
        reverse: bool,
    ) -> Result<SearchResult, TmuxError> {
        let cmd = if reverse {
            "search-reverse"
        } else {
            "search-again"
        };
        self.run(&["send-keys", "-t", &id.0, "-X", cmd])?;
        self.search_info(id)
    }

    pub fn search_info(&self, id: &TmuxSessionId) -> Result<SearchResult, TmuxError> {
        let out = self.run(&[
            "display-message",
            "-p",
            "-t",
            &id.0,
            "#{search_present} #{search_count}",
        ])?;
        Ok(parse_search_info(&out))
    }

    /// Leave copy-mode; harmless when the pane is not in it.
    pub fn search_cancel(&self, id: &TmuxSessionId) {
        let _ = serialized(|| self.run(&["send-keys", "-t", &id.0, "-X", "cancel"]));
    }
}

/// Stale-query rejection, shareable with worker threads: a query that is no
/// longer the latest must not touch tmux or the UI. Clone it into the job.
#[derive(Debug, Clone, Default)]
pub struct Generation {
    current: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Generation {
    /// A new query was typed: returns its ticket.
    pub fn bump(&self) -> u64 {
        self.current
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }
    /// Is `ticket` still the latest query?
    pub fn is_current(&self, ticket: u64) -> bool {
        ticket == self.current.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One search tmux conversation at a time: cancel / start / step must not
/// interleave, or the pane can be left in copy-mode after the bar closed.
/// NOT reentrant: never call `search_*` from inside the closure (deadlock).
pub fn serialized<T>(f: impl FnOnce() -> T) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::NewSessionOpts;
    use std::path::PathBuf;

    #[test]
    fn parses_hits_misses_and_old_tmux() {
        assert_eq!(
            parse_search_info("1 1111\n"),
            SearchResult {
                present: true,
                count: Some(1111)
            }
        );
        // A miss: the remembered count is ignored.
        assert_eq!(
            parse_search_info("0 42"),
            SearchResult {
                present: false,
                count: Some(0)
            }
        );
        // tmux < 3.5 has no count.
        assert_eq!(
            parse_search_info("1 "),
            SearchResult {
                present: true,
                count: None
            }
        );
        assert!(!parse_search_info("").present);
    }

    #[test]
    fn stale_tickets_are_rejected() {
        let g = Generation::default();
        let a = g.bump();
        let b = g.bump();
        assert!(!g.is_current(a));
        assert!(g.is_current(b));
    }

    #[test]
    fn search_on_scratch_server() {
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let socket = format!("hashterm-search-{}", std::process::id());
        let ctl = TmuxController::new(socket, PathBuf::from("/dev/null"));
        let id = ctl
            .new_session(&NewSessionOpts {
                title: "s".into(),
                cwd: None,
                command: Some(vec![
                    "sh".into(),
                    "-c".into(),
                    "i=1; while [ $i -le 3000 ]; do echo line$i Needle; i=$((i+1)); done; echo 'tail;mark'; sleep 60"
                        .into(),
                ]),
                group: None,
            })
            .unwrap();
        // Let the loop finish printing.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let hit = ctl.search_start(&id, "line2999").unwrap();
        assert!(hit.present);
        // Smart-case: lowercase matches the capital N, a capital is exact.
        let lower = ctl.search_start(&id, "needle").unwrap();
        assert!(lower.present);
        assert!(lower.count.is_none_or(|c| c >= 2000));
        let exact = ctl.search_start(&id, "NEEDLE").unwrap();
        assert!(!exact.present);
        assert_eq!(exact.count, Some(0));
        // A trailing ';' is a literal character, not a tmux command separator.
        assert!(ctl.search_start(&id, "tail;").unwrap().present);
        assert!(!ctl.search_start(&id, "mark;").unwrap().present);
        assert!(ctl.search_start(&id, "mark").unwrap().present);
        // A miss right after a hit must not report the old count.
        ctl.search_start(&id, "needle").unwrap();
        let miss = ctl.search_start(&id, "zzzzqqq").unwrap();
        assert!(!miss.present);
        assert_eq!(miss.count, Some(0));
        // Stepping works while in copy-mode, and cancel leaves it.
        ctl.search_start(&id, "needle").unwrap();
        assert!(ctl.search_step(&id, false).unwrap().present);
        let mode = ctl
            .run(&["display-message", "-p", "-t", &id.0, "#{pane_in_mode}"])
            .unwrap();
        assert_eq!(mode.trim(), "1");
        ctl.search_cancel(&id);
        let mode = ctl
            .run(&["display-message", "-p", "-t", &id.0, "#{pane_in_mode}"])
            .unwrap();
        assert_eq!(mode.trim(), "0");
        let _ = ctl.run(&["kill-server"]);
    }
}
