//! "Does a tab need me?" as pure logic: which badge wins, when background
//! output counts as activity, and how often one tab may notify. The window
//! feeds these real events and clocks; tests feed them fake ones.

use crate::tabbar::Badge;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Bell beats Activity beats External.
fn rank(b: Badge) -> u8 {
    match b {
        Badge::Bell => 3,
        Badge::Activity => 2,
        Badge::External => 1,
    }
}

/// The strongest badge among a group's tabs, and how many tabs have one.
pub fn summarize(badges: impl IntoIterator<Item = Option<Badge>>) -> (Option<Badge>, usize) {
    let mut best: Option<Badge> = None;
    let mut n = 0;
    for b in badges.into_iter().flatten() {
        n += 1;
        if best.is_none_or(|cur| rank(b) > rank(cur)) {
            best = Some(b);
        }
    }
    (best, n)
}

/// Does a badge deserve a notification, given the user's switches?
pub fn notify_wanted(badge: Badge, on_bell: bool, on_activity: bool) -> bool {
    match badge {
        Badge::Bell => on_bell,
        Badge::Activity => on_activity,
        Badge::External => false,
    }
}

/// Background output counts as activity on the LEADING edge only: a change
/// after `quiet` of silence fires, a continuous stream fires once. So `top`
/// lights a tab once instead of re-arming forever.
#[derive(Debug, Default)]
pub struct ActivityGate {
    last_change: Option<Instant>,
}

impl ActivityGate {
    pub fn on_change(&mut self, now: Instant, quiet: Duration) -> bool {
        let fire = self
            .last_change
            .is_none_or(|t| now.saturating_duration_since(t) >= quiet);
        self.last_change = Some(now);
        fire
    }
}

/// At most one notification per key within `min_interval`.
#[derive(Debug, Default)]
pub struct RateLimiter {
    last: HashMap<String, Instant>,
}

impl RateLimiter {
    pub fn allow(&mut self, key: &str, now: Instant, min_interval: Duration) -> bool {
        match self.last.get(key) {
            Some(t) if now.saturating_duration_since(*t) < min_interval => false,
            _ => {
                self.last.insert(key.to_string(), now);
                true
            }
        }
    }

    pub fn forget(&mut self, key: &str) {
        self.last.remove(key);
    }
}

/// "2 tabs need attention" / "1 tab needs attention".
pub fn attention_tip(n: usize) -> String {
    if n == 1 {
        "1 tab needs attention".into()
    } else {
        format!("{n} tabs need attention")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn strongest_badge_wins_and_tabs_are_counted() {
        assert_eq!(summarize([None, None]), (None, 0));
        assert_eq!(
            summarize([Some(Badge::Activity), None, Some(Badge::Bell)]),
            (Some(Badge::Bell), 2)
        );
        assert_eq!(
            summarize([Some(Badge::External), Some(Badge::Activity)]),
            (Some(Badge::Activity), 2)
        );
    }

    #[test]
    fn notifications_follow_the_switches() {
        assert!(notify_wanted(Badge::Bell, true, false));
        assert!(!notify_wanted(Badge::Bell, false, true));
        assert!(!notify_wanted(Badge::Activity, true, false));
        assert!(notify_wanted(Badge::Activity, false, true));
        assert!(!notify_wanted(Badge::External, true, true));
    }

    #[test]
    fn activity_fires_on_the_leading_edge_only() {
        let t0 = Instant::now();
        let mut g = ActivityGate::default();
        let quiet = 5 * S;
        assert!(g.on_change(t0, quiet), "first output fires");
        // A continuous stream never re-fires, however long it runs.
        for i in 1..100 {
            assert!(!g.on_change(t0 + i * S, quiet));
        }
        // After 5 s of silence the next output fires again.
        assert!(g.on_change(t0 + 99 * S + 5 * S, quiet));
        assert!(!g.on_change(t0 + 99 * S + 6 * S, quiet));
    }

    #[test]
    fn rate_limiter_is_per_key() {
        let t0 = Instant::now();
        let mut r = RateLimiter::default();
        let gap = 10 * S;
        assert!(r.allow("a", t0, gap));
        assert!(!r.allow("a", t0 + 3 * S, gap));
        assert!(r.allow("b", t0 + 3 * S, gap));
        assert!(r.allow("a", t0 + 10 * S, gap));
        r.forget("a");
        assert!(r.allow("a", t0 + 11 * S, gap));
    }

    #[test]
    fn tips_pluralize() {
        assert_eq!(attention_tip(1), "1 tab needs attention");
        assert_eq!(attention_tip(3), "3 tabs need attention");
    }
}
