//! The knock decisions, separated from timers and threads so they can be
//! tested with an injected clock. Same rules as the Go version (ADR-0005):
//!
//! - at most one knock in flight; a knock starts only when the latest egress
//!   address differs from the last one successfully knocked;
//! - when a knock finishes and the address has moved on meanwhile, knock the
//!   newer address at once (converge to latest); a failed knock for the still
//!   current address waits for the next event or reconcile instead of looping;
//! - grant refresh re-knocks an unchanged address, measured from the last
//!   successful knock.

use std::net::Ipv6Addr;
use std::time::{Duration, Instant};

use crate::log::OrNone;
use crate::{log_error, log_info};

#[derive(Debug)]
pub struct Controller {
    /// Most recently observed egress address (the desired target).
    latest: Option<Ipv6Addr>,
    /// Address of the last successful knock.
    last_fired: Option<Ipv6Addr>,
    /// A knock is in flight.
    running: bool,
    refresh: Option<Duration>,
    refresh_due: Option<Instant>,
}

impl Controller {
    pub fn new(refresh: Option<Duration>, now: Instant) -> Controller {
        Controller {
            latest: None,
            last_fired: None,
            running: false,
            refresh,
            refresh_due: refresh.map(|r| now + r),
        }
    }

    /// When the grant refresh is next due, if it is enabled.
    pub fn refresh_due(&self) -> Option<Instant> {
        self.refresh_due
    }

    /// Records a freshly computed egress address. Returns the address to knock
    /// if a knock should start now.
    pub fn observe(&mut self, reason: &str, addr: Ipv6Addr) -> Option<Ipv6Addr> {
        if self.latest != Some(addr) {
            log_info!("egress address changed", "reason" => reason, "addr" => addr,
                "previous" => OrNone(self.latest));
        }
        self.latest = Some(addr);
        self.maybe_fire()
    }

    /// The refresh interval elapsed: forget the last knock so the next
    /// observation knocks again even though the address is unchanged, and
    /// re-arm the interval.
    pub fn refresh_elapsed(&mut self, now: Instant) {
        log_info!("refresh interval elapsed; re-knocking", "last_fired" => OrNone(self.last_fired));
        self.last_fired = None;
        self.refresh_due = self.refresh.map(|r| now + r);
    }

    /// A knock for `target` finished. Returns the address to knock next, if the
    /// egress address moved on while it ran.
    pub fn fire_done(
        &mut self,
        target: Ipv6Addr,
        result: Result<(), String>,
        now: Instant,
    ) -> Option<Ipv6Addr> {
        self.running = false;
        match result {
            Ok(()) => {
                self.last_fired = Some(target);
                log_info!("fire succeeded", "addr" => target);
                // Measure the refresh interval from this success.
                self.refresh_due = self.refresh.map(|r| now + r);
            }
            Err(e) => {
                log_error!("fire failed; will retry on next rotation or reconcile",
                    "addr" => target, "err" => e);
            }
        }
        if self.latest != Some(target) && self.latest != self.last_fired {
            return self.maybe_fire();
        }
        None
    }

    fn maybe_fire(&mut self) -> Option<Ipv6Addr> {
        let target = self.latest?;
        if self.running || self.last_fired == Some(target) {
            return None;
        }
        self.running = true;
        log_info!("firing", "addr" => target);
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    #[test]
    fn fires_once_per_address() {
        let t0 = Instant::now();
        let mut c = Controller::new(None, t0);
        assert_eq!(
            c.observe("startup", a("2001:db8::1")),
            Some(a("2001:db8::1"))
        );
        assert_eq!(
            c.observe("reconcile", a("2001:db8::1")),
            None,
            "already running"
        );
        assert_eq!(c.fire_done(a("2001:db8::1"), Ok(()), t0), None);
        assert_eq!(
            c.observe("reconcile", a("2001:db8::1")),
            None,
            "already knocked"
        );
        assert_eq!(
            c.observe("address-event", a("2001:db8::2")),
            Some(a("2001:db8::2"))
        );
    }

    #[test]
    fn converges_to_the_latest_address() {
        let t0 = Instant::now();
        let mut c = Controller::new(None, t0);
        assert_eq!(
            c.observe("startup", a("2001:db8::1")),
            Some(a("2001:db8::1"))
        );
        // The address rotates twice while the first knock is in flight.
        assert_eq!(c.observe("address-event", a("2001:db8::2")), None);
        assert_eq!(c.observe("address-event", a("2001:db8::3")), None);
        // When it finishes, only the newest address is knocked.
        assert_eq!(
            c.fire_done(a("2001:db8::1"), Ok(()), t0),
            Some(a("2001:db8::3"))
        );
        assert_eq!(c.fire_done(a("2001:db8::3"), Ok(()), t0), None);
    }

    #[test]
    fn converges_even_after_a_failed_knock() {
        let t0 = Instant::now();
        let mut c = Controller::new(None, t0);
        c.observe("startup", a("2001:db8::1"));
        c.observe("address-event", a("2001:db8::2"));
        assert_eq!(
            c.fire_done(a("2001:db8::1"), Err("boom".into()), t0),
            Some(a("2001:db8::2"))
        );
    }

    #[test]
    fn a_failed_knock_for_the_current_address_does_not_loop() {
        let t0 = Instant::now();
        let mut c = Controller::new(None, t0);
        c.observe("startup", a("2001:db8::1"));
        assert_eq!(
            c.fire_done(a("2001:db8::1"), Err("unreachable".into()), t0),
            None
        );
        // The next reconcile retries it.
        assert_eq!(
            c.observe("reconcile", a("2001:db8::1")),
            Some(a("2001:db8::1"))
        );
    }

    #[test]
    fn refresh_is_measured_from_the_last_success() {
        let t0 = Instant::now();
        let ten = Duration::from_secs(600);
        let mut c = Controller::new(Some(ten), t0);
        assert_eq!(c.refresh_due(), Some(t0 + ten));

        c.observe("startup", a("2001:db8::1"));
        let t1 = t0 + Duration::from_secs(3);
        c.fire_done(a("2001:db8::1"), Ok(()), t1);
        assert_eq!(c.refresh_due(), Some(t1 + ten), "re-armed from the success");

        // A failure does not move the deadline.
        let t2 = t1 + Duration::from_secs(60);
        c.observe("address-event", a("2001:db8::2"));
        c.fire_done(a("2001:db8::2"), Err("x".into()), t2);
        assert_eq!(c.refresh_due(), Some(t1 + ten));

        // When it elapses the unchanged address is knocked again.
        let t3 = t1 + ten;
        c.refresh_elapsed(t3);
        assert_eq!(c.refresh_due(), Some(t3 + ten));
        assert_eq!(
            c.observe("refresh", a("2001:db8::2")),
            Some(a("2001:db8::2"))
        );
        c.fire_done(a("2001:db8::2"), Ok(()), t3);
        assert_eq!(c.observe("refresh", a("2001:db8::2")), None);
    }

    #[test]
    fn refresh_while_a_knock_is_in_flight_is_covered_by_that_knock() {
        let t0 = Instant::now();
        let ten = Duration::from_secs(600);
        let mut c = Controller::new(Some(ten), t0);
        c.observe("startup", a("2001:db8::1"));
        c.refresh_elapsed(t0 + ten);
        assert_eq!(
            c.observe("refresh", a("2001:db8::1")),
            None,
            "one knock at a time"
        );
        let t1 = t0 + ten + Duration::from_secs(1);
        assert_eq!(c.fire_done(a("2001:db8::1"), Ok(()), t1), None);
        assert_eq!(c.refresh_due(), Some(t1 + ten));
    }

    #[test]
    fn refresh_disabled() {
        let mut c = Controller::new(None, Instant::now());
        assert_eq!(c.refresh_due(), None);
        c.observe("startup", a("2001:db8::1"));
        c.fire_done(a("2001:db8::1"), Ok(()), Instant::now());
        assert_eq!(c.refresh_due(), None);
    }
}
