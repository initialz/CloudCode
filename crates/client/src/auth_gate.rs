//! Session-scoped authorization gate for the browser channel.
//! Pure state machine: no IO, no clocks — callers pass `Instant::now()`.

use std::time::{Duration, Instant};

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant { Instant::now() }

    #[test]
    fn first_frame_asks_user() {
        let gate = AuthGate::new(Duration::from_secs(600));
        assert!(matches!(gate.check(t0()), Decision::AskUser));
    }

    #[test]
    fn granted_allows_within_idle_window() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        assert!(matches!(gate.check(now + Duration::from_secs(599)), Decision::Allow));
    }

    #[test]
    fn idle_timeout_expires_grant_and_asks_again() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        assert!(matches!(gate.check(now + Duration::from_secs(601)), Decision::AskUser));
    }

    #[test]
    fn allow_refreshes_idle_clock() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        // 9 分钟后一次活动……
        let later = now + Duration::from_secs(540);
        assert!(matches!(gate.check(later), Decision::Allow));
        gate.touch(later);
        // ……再过 9 分钟仍在窗口内(滑动)
        assert!(matches!(gate.check(later + Duration::from_secs(540)), Decision::Allow));
    }

    #[test]
    fn deny_resets_to_idle() {
        let mut gate = AuthGate::new(Duration::from_secs(600));
        let now = t0();
        gate.grant(now);
        gate.deny();
        assert!(matches!(gate.check(now), Decision::AskUser));
    }
}

/// What the relay should do with an inbound browser frame.
#[derive(Debug)]
pub enum Decision {
    /// Grant is live — forward the frame.
    Allow,
    /// No live grant — hold the frame and prompt the user.
    AskUser,
}

/// One grant per claude task, approximated by a sliding idle window
/// (per spec: the release predicate is idle-timeout only).
pub struct AuthGate {
    idle_timeout: Duration,
    granted_at: Option<Instant>,
    last_activity: Option<Instant>,
}

impl AuthGate {
    pub fn new(idle_timeout: Duration) -> Self {
        Self { idle_timeout, granted_at: None, last_activity: None }
    }

    pub fn check(&self, now: Instant) -> Decision {
        match self.last_activity.or(self.granted_at) {
            Some(last) if now.duration_since(last) <= self.idle_timeout => Decision::Allow,
            _ => Decision::AskUser,
        }
    }

    pub fn grant(&mut self, now: Instant) {
        self.granted_at = Some(now);
        self.last_activity = Some(now);
    }

    /// Record activity on an allowed frame (slides the idle window).
    pub fn touch(&mut self, now: Instant) {
        if self.granted_at.is_some() {
            self.last_activity = Some(now);
        }
    }

    pub fn deny(&mut self) {
        self.granted_at = None;
        self.last_activity = None;
    }
}
