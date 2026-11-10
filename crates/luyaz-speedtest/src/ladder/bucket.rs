//! The rate limit several connections share. Time is passed in, so the arithmetic is testable.
use super::stop::Stopper;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// One round: a burst never exceeds this much of the rate.
pub(super) const ROUND: Duration = Duration::from_millis(100);
/// A wait looks at the stop switch this often.
const NAP: Duration = Duration::from_millis(50);

/// Bytes may go negative: a POST larger than a round is sent whole and paid for by waiting
/// before the next one, which keeps the average on the rate.
struct State {
    tokens: f64,
    at: Instant,
}
pub(super) struct Bucket {
    rate: f64,
    state: Mutex<State>,
}
impl Bucket {
    pub(super) fn new(mbps: f64, now: Instant) -> Self {
        Self {
            rate: mbps * 1e6 / 8.0,
            state: Mutex::new(State {
                tokens: 0.0,
                at: now,
            }),
        }
    }
    /// A read should ask for at most this, so the rate stays smooth.
    pub(super) fn round_bytes(&self) -> usize {
        ((self.rate * ROUND.as_secs_f64()) as usize).max(1024)
    }
    /// `Ok`: `want` is paid for, go. `Err(d)`: nothing taken, ask again in `d`.
    fn try_pay(&self, want: u64, now: Instant) -> Result<(), Duration> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let dt = now.saturating_duration_since(s.at).as_secs_f64();
        s.at = now;
        s.tokens = (s.tokens + self.rate * dt).min(self.rate * ROUND.as_secs_f64());
        if s.tokens <= 0.0 {
            return Err(
                Duration::from_secs_f64(-s.tokens / self.rate).max(Duration::from_millis(1))
            );
        }
        s.tokens -= want as f64;
        Ok(())
    }
    /// Wait for `want` to be paid; false = stopped first.
    pub(super) fn pay(&self, want: u64, stop: &Stopper) -> bool {
        loop {
            if stop.is_stopped() {
                return false;
            }
            match self.try_pay(want, Instant::now()) {
                Ok(()) => return true,
                Err(d) => thread::sleep(d.min(NAP)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything a greedy sender gets in `secs`, at millisecond steps.
    fn sent(b: &Bucket, t0: Instant, want: u64, secs: u64) -> u64 {
        let mut total = 0;
        let mut ms = 0;
        while ms < secs * 1000 {
            let now = t0 + Duration::from_millis(ms);
            match b.try_pay(want, now) {
                Ok(()) => total += want,
                Err(_) => ms += 1,
            }
        }
        total
    }
    #[test]
    fn holds_the_rate_for_small_reads_and_for_posts_larger_than_a_round() {
        let t0 = Instant::now();
        for (mbps, want) in [(25.0, 64 * 1024), (30.0, 256 * 1024), (3.0, 256 * 1024)] {
            let b = Bucket::new(mbps, t0);
            let got = sent(&b, t0, want, 20) as f64 * 8.0 / 20.0 / 1e6;
            assert!((got - mbps).abs() < mbps * 0.1, "{mbps}: {got}");
        }
    }
    #[test]
    fn idle_time_does_not_save_up_more_than_one_round() {
        let t0 = Instant::now();
        let b = Bucket::new(8.0, t0); // 1 MB/s, 100 KB a round
        let later = t0 + Duration::from_secs(60);
        assert!(b.try_pay(100_000, later).is_ok());
        assert!(b.try_pay(1000, later).is_err());
    }
    #[test]
    fn a_wait_ends_on_stop() {
        let stop = Stopper::new();
        let b = Bucket::new(0.008, Instant::now());
        stop.stop();
        assert!(!b.pay(1, &stop));
    }
}
