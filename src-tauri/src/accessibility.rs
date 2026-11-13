//! Accessibility is optional. Every long-lived event-tap owner checks it and
//! drops its listener on revocation, including when no keyboard events arrive.

use std::time::{Duration, Instant};

pub fn is_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        handy_keys::check_accessibility()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Bound permission checks independently of the 10 ms event-processing loops.
/// This uses AXIsProcessTrusted, never CGEventTapIsEnabled (see #1827).
pub struct PermissionPoll {
    next_check: Instant,
}

impl PermissionPoll {
    pub fn new() -> Self {
        Self {
            next_check: Instant::now(),
        }
    }

    pub fn poll(&mut self) -> Option<bool> {
        self.poll_at(Instant::now(), is_granted)
    }

    fn poll_at(&mut self, now: Instant, check: impl FnOnce() -> bool) -> Option<bool> {
        if now < self.next_check {
            return None;
        }
        self.next_check = now + Duration::from_secs(1);
        Some(check())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_revocation_and_regrant_without_input_events() {
        let mut poll = PermissionPoll::new();
        let now = Instant::now();
        assert_eq!(poll.poll_at(now, || true), Some(true));
        assert_eq!(
            poll.poll_at(now + Duration::from_millis(10), || panic!(
                "polled too often"
            )),
            None
        );
        assert_eq!(
            poll.poll_at(now + Duration::from_secs(1), || false),
            Some(false)
        );
        assert_eq!(
            poll.poll_at(now + Duration::from_secs(2), || true),
            Some(true)
        );
    }
}
