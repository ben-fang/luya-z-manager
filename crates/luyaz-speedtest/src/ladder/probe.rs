//! Kept-alive probes: 4 connections, one send every 50 ms between them.
use super::judge::{Outcome, Sample, CAP_MS};
use super::stop::Stopper;
use crate::runner::Pool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

pub(super) const CONNECTIONS: usize = 4;
const EVERY_MS: u64 = 50;
/// Budgeted for a probe whose real size is unknown because it failed.
const FAILED_BYTES: u64 = 512;

pub(super) struct Probes<'a> {
    pub url: &'a str,
    pub stop: &'a Stopper,
    pub clock: Instant,
    pub next_slot: AtomicU64,
    pub samples: Mutex<Vec<Sample>>,
    pub budget: &'a Pool,
}
impl Probes<'_> {
    /// Connect before step 1 (the handshake is not a sample), then send until stopped.
    pub(super) fn run(&self, n: usize) {
        let agent = self.stop.agent();
        let mut seq = n as u64 * 1_000_000;
        let _ = self.once(&agent, &mut seq);
        // set after a timeout: the next sample includes a handshake, so it is dropped
        let mut rebuilt = false;
        while !self.stop.is_stopped() {
            let slot = self.next_slot.fetch_add(1, Ordering::SeqCst);
            let due = self.clock + Duration::from_millis(slot * EVERY_MS);
            let now = Instant::now();
            if now >= due + Duration::from_millis(EVERY_MS) {
                continue; // everyone was busy: this slot's send never happened
            }
            while Instant::now() < due {
                thread::sleep(Duration::from_millis(5));
                if self.stop.is_stopped() {
                    return;
                }
            }
            let sample = self.once(&agent, &mut seq);
            if !std::mem::replace(&mut rebuilt, sample.outcome == Outcome::Timeout)
                || sample.outcome == Outcome::Timeout
            {
                self.samples
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(sample);
            }
        }
    }
    fn once(&self, agent: &ureq::Agent, seq: &mut u64) -> Sample {
        *seq += 1;
        let sent = Instant::now();
        let at = u32::try_from(sent.saturating_duration_since(self.clock).as_millis())
            .unwrap_or(u32::MAX);
        let cap = Duration::from_millis(u64::from(CAP_MS));
        let r = agent
            .get(format!("{}?ladder={seq}", self.url))
            .config()
            .timeout_global(Some(cap))
            .build()
            .call()
            .and_then(|r| r.into_body().with_config().limit(1024).read_to_vec());
        let took = u32::try_from(sent.elapsed().as_millis())
            .unwrap_or(u32::MAX)
            .min(CAP_MS);
        let (outcome, rtt) = match r {
            Ok(body) => {
                self.budget.add(body.len() as u64 + 256);
                (Outcome::Ok, took)
            }
            Err(_) if self.stop.is_stopped() => (Outcome::Interrupted, took),
            Err(ureq::Error::Timeout(_)) => {
                self.budget.add(FAILED_BYTES);
                (Outcome::Timeout, CAP_MS)
            }
            Err(_) => {
                self.budget.add(FAILED_BYTES);
                (Outcome::Failed, took)
            }
        };
        Sample {
            sent: at,
            rtt,
            outcome,
        }
    }
}
