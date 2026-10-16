//! Splits are tmux panes: a tab stays ONE session and ONE VTE client, and
//! tmux draws, resizes and persists the panes (they survive crashes and are
//! rebuilt by session restore). These are thin, argument-tested wrappers.
//!
//! Every split passes `-c '#{pane_current_path}'`: without it tmux starts the
//! new pane in the directory the tmux SERVER was launched from.

use crate::controller::{TmuxController, TmuxError, TmuxSessionId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn flag(self) -> &'static str {
        match self {
            Dir::Left => "-L",
            Dir::Right => "-R",
            Dir::Up => "-U",
            Dir::Down => "-D",
        }
    }
}

/// `horizontal` = side by side (tmux `-h`), else stacked (`-v`).
pub fn split_args(target: &str, horizontal: bool) -> Vec<String> {
    vec![
        "split-window".into(),
        if horizontal { "-h" } else { "-v" }.into(),
        "-t".into(),
        target.into(),
        "-c".into(),
        "#{pane_current_path}".into(),
    ]
}

pub fn select_args(target: &str, dir: Dir) -> Vec<String> {
    vec![
        "select-pane".into(),
        "-t".into(),
        target.into(),
        dir.flag().into(),
    ]
}

impl TmuxController {
    fn run_owned(&self, args: &[String]) -> Result<String, TmuxError> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }

    /// Panes in the tab's (single) window.
    pub fn pane_count(&self, id: &TmuxSessionId) -> Result<u32, TmuxError> {
        let out = self.run(&["display-message", "-p", "-t", &id.0, "#{window_panes}"])?;
        out.trim().parse().map_err(|_| TmuxError::Command {
            args: vec!["display-message".into(), "#{window_panes}".into()],
            stderr: format!("unexpected pane count {:?}", out.trim()),
        })
    }

    pub fn split(&self, id: &TmuxSessionId, horizontal: bool) -> Result<(), TmuxError> {
        self.run_owned(&split_args(&id.0, horizontal)).map(|_| ())
    }

    pub fn select_pane(&self, id: &TmuxSessionId, dir: Dir) -> Result<(), TmuxError> {
        self.run_owned(&select_args(&id.0, dir)).map(|_| ())
    }

    /// Toggle zoom on the active pane; returns whether it is zoomed now.
    pub fn zoom_pane(&self, id: &TmuxSessionId) -> Result<bool, TmuxError> {
        self.run(&["resize-pane", "-Z", "-t", &id.0])?;
        let out = self.run(&[
            "display-message",
            "-p",
            "-t",
            &id.0,
            "#{window_zoomed_flag}",
        ])?;
        Ok(out.trim() == "1")
    }

    pub fn kill_active_pane(&self, id: &TmuxSessionId) -> Result<(), TmuxError> {
        self.run(&["kill-pane", "-t", &id.0]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::NewSessionOpts;
    use std::path::PathBuf;

    #[test]
    fn split_always_inherits_the_pane_directory() {
        for h in [true, false] {
            let a = split_args("$1", h);
            assert!(a.windows(2).any(|w| w == ["-c", "#{pane_current_path}"]));
            assert_eq!(a[1], if h { "-h" } else { "-v" });
        }
        assert_eq!(select_args("$1", Dir::Left)[3], "-L");
        assert_eq!(select_args("$1", Dir::Down)[3], "-D");
    }

    #[test]
    fn split_select_zoom_kill_on_scratch_server() {
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let socket = format!("hashterm-panes-{}", std::process::id());
        let ctl = TmuxController::new(socket, PathBuf::from("/dev/null"));
        let id = ctl
            .new_session(&NewSessionOpts {
                title: "p".into(),
                cwd: Some(PathBuf::from("/tmp")),
                command: Some(vec!["sleep".into(), "60".into()]),
                group: None,
            })
            .unwrap();
        assert_eq!(ctl.pane_count(&id).unwrap(), 1);
        ctl.split(&id, true).unwrap();
        ctl.split(&id, false).unwrap();
        assert_eq!(ctl.pane_count(&id).unwrap(), 3);
        // The new panes started in the active pane's folder, not the server's.
        let paths = ctl
            .run(&["list-panes", "-t", &id.0, "-F", "#{pane_current_path}"])
            .unwrap();
        assert!(paths.lines().all(|p| p == "/tmp"), "{paths}");
        let before = ctl
            .run(&["display-message", "-p", "-t", &id.0, "#{pane_id}"])
            .unwrap();
        ctl.select_pane(&id, Dir::Up).unwrap();
        let after = ctl
            .run(&["display-message", "-p", "-t", &id.0, "#{pane_id}"])
            .unwrap();
        assert_ne!(before, after);
        assert!(ctl.zoom_pane(&id).unwrap());
        assert!(!ctl.zoom_pane(&id).unwrap());
        ctl.kill_active_pane(&id).unwrap();
        assert_eq!(ctl.pane_count(&id).unwrap(), 2);
        let _ = ctl.run(&["kill-server"]);
    }
}
