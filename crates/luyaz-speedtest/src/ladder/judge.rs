//! From probe samples to findings: 250 ms windows, then the design's rules. No I/O and no clock.
use super::engine::StepKind;
use serde::Serialize;

pub(super) const WINDOW_MS: u32 = 250;
/// The probe's limit; a probe still unanswered then counts as this.
pub(super) const CAP_MS: u32 = 2000;
const MIN_WINDOWS: usize = 10;
/// Slower by this much, and at least twice as slow.
const SLOW_MS: f64 = 100.0;
const JITTER_MS: f64 = 100.0;
const RECOVER_MARGIN_MS: f64 = 20.0;
const RECOVER_RUN: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Ok,
    /// no answer within `CAP_MS`: a valid sample at the cap
    Timeout,
    /// failed at once: says nothing about the delay
    Failed,
    /// ended by a stop
    Interrupted,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    /// ms since the run's clock began
    pub sent: u32,
    pub rtt: u32,
    pub outcome: Outcome,
}
impl Sample {
    fn counts(&self) -> bool {
        matches!(self.outcome, Outcome::Ok | Outcome::Timeout)
    }
    fn end(&self) -> u32 {
        self.sent + self.rtt
    }
}
/// Each window's value over `[from, to)`: the largest delay of the probes answered in it. When
/// none was answered but some were waiting, the largest delay of those waiting, so a probe
/// lost for 2 s fills its windows at the cap. Otherwise the window has no value.
pub(super) fn windows(samples: &[Sample], from: u32, to: u32) -> Vec<Option<u32>> {
    let mut out = Vec::new();
    let mut at = from;
    while at + WINDOW_MS <= to {
        let end = at + WINDOW_MS;
        let valid = || samples.iter().filter(|s| s.counts());
        let answered = valid()
            .filter(|s| (at..end).contains(&s.end()))
            .map(|s| s.rtt)
            .max();
        let waiting = || {
            valid()
                .filter(|s| s.sent < end && s.end() >= end)
                .map(|s| s.rtt)
                .max()
        };
        out.push(answered.or_else(waiting));
        at = end;
    }
    out
}
/// Each figure is of the 250 ms windows (each the slowest probe in it), except where it says
/// probes. Shown only: the findings never read p95, jitter or the timeouts.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StepStats {
    pub p5_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p90_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    /// windows with a value
    pub windows: usize,
    /// more than half of the windows at the cap
    pub unresponsive: bool,
    /// probes sent in the step that answered or ran out of time
    pub probes: usize,
    /// probes with no answer within `CAP_MS`: not packet loss (TCP resends hide that)
    pub timeouts: usize,
    /// the mean change in delay from one answered probe to the next, in the order sent
    pub jitter_ms: Option<f64>,
    /// a load step's P90 less idle's P5, under 2 ms counted as none: the delay the load added,
    /// as test.libreqos.com reports it (it grades this; here it is a number only)
    pub increase_ms: Option<f64>,
}
fn percentile(sorted: &[u32], p: usize) -> Option<f64> {
    let last = sorted.len().checked_sub(1)?;
    sorted.get((last * p + 50) / 100).map(|v| f64::from(*v))
}
pub(super) fn stats(win: &[Option<u32>]) -> StepStats {
    let mut v: Vec<u32> = win.iter().flatten().copied().collect();
    v.sort_unstable();
    StepStats {
        p5_ms: percentile(&v, 5),
        p50_ms: percentile(&v, 50),
        p90_ms: percentile(&v, 90),
        p95_ms: percentile(&v, 95),
        windows: v.len(),
        unresponsive: v.iter().filter(|x| **x >= CAP_MS).count() * 2 > v.len(),
        ..StepStats::default()
    }
}
/// The delay a load step added over idle: its P90 less idle's P5, noise under 2 ms as none.
pub(super) fn increase(step: &StepStats, idle: &StepStats) -> Option<f64> {
    let d = step.p90_ms? - idle.p5_ms?;
    Some(if d < 2.0 { 0.0 } else { d })
}
/// The probes sent in `[from, to)`: how many counted, how many timed out, and their jitter.
pub(super) fn probe_stats(samples: &[Sample], from: u32, to: u32, s: &mut StepStats) {
    let mut sent: Vec<&Sample> = samples
        .iter()
        .filter(|x| x.counts() && (from..to).contains(&x.sent))
        .collect();
    sent.sort_by_key(|x| x.sent);
    s.probes = sent.len();
    s.timeouts = sent.iter().filter(|x| x.outcome == Outcome::Timeout).count();
    let ok: Vec<u32> = sent
        .iter()
        .filter(|x| x.outcome == Outcome::Ok)
        .map(|x| x.rtt)
        .collect();
    let steps: Vec<f64> = ok
        .windows(2)
        .map(|w| f64::from(w[0].abs_diff(w[1])))
        .collect();
    s.jitter_ms = (!steps.is_empty()).then(|| steps.iter().sum::<f64>() / steps.len() as f64);
}
fn enough(s: &StepStats) -> bool {
    s.windows >= MIN_WINDOWS
}
/// What the run found. These fields are also what calibration compares between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Findings {
    /// no finding at all: step 1 is not usable as the baseline
    pub no_baseline: bool,
    pub idle_jitter: bool,
    /// the first load step judged slower
    pub first_slow_step: Option<u8>,
    /// every load step judged slower: with both directions slower but upload alone not, it is
    /// the two together that queue
    pub slow_steps: Vec<u8>,
    /// load steps with too few windows to say
    pub short_steps: Vec<u8>,
    pub unresponsive_steps: Vec<u8>,
    /// ms from the start of the recovery step; `None` with `recovered` false = not in the window
    pub recovery_ms: Option<u32>,
    /// `None`: not judged
    pub recovered: Option<bool>,
    /// load steps far below their target: "not slower" means nothing there
    pub unloaded_steps: Vec<u8>,
    /// load steps short of their target with the delay up: the link queued at the rate reached
    pub queued_steps: Vec<u8>,
}
pub(super) fn slower(step: &StepStats, base: &StepStats, jitter: bool) -> Option<bool> {
    if step.unresponsive {
        return Some(true);
    }
    let (now, then) = if jitter {
        (step.p90_ms?, base.p90_ms?)
    } else {
        (step.p50_ms?, base.p50_ms?)
    };
    Some(now - then >= SLOW_MS && now >= then * 2.0)
}
/// `steps` are the run's steps in order, `kinds` what each loads: idle first, then the load
/// steps, recovery last (`recovery`: its windows). `from_step`: steps from this one on have no
/// usable baseline (cell or RAT changed there), and 1 means nothing can be said.
pub(super) fn judge(
    steps: &[StepStats],
    kinds: &[StepKind],
    recovery: &[Option<u32>],
    from_step: Option<u8>,
) -> Findings {
    let mut f = Findings::default();
    let Some(base) = steps.first() else {
        f.no_baseline = true;
        return f;
    };
    let numbered = || {
        steps
            .iter()
            .zip(kinds)
            .enumerate()
            .map(|(i, (s, k))| (i as u8 + 1, s, *k))
    };
    // no answer for most of a step needs no baseline: a cell change or a busy idle keeps it
    for (n, step, _) in numbered().filter(|(_, _, k)| k.loads()) {
        if step.unresponsive && enough(step) {
            f.unresponsive_steps.push(n);
        }
    }
    if !enough(base) || from_step == Some(1) {
        f.no_baseline = true;
        return f;
    }
    let (Some(p50), Some(p90)) = (base.p50_ms, base.p90_ms) else {
        f.no_baseline = true;
        return f;
    };
    f.idle_jitter = p90 - p50 > JITTER_MS;
    let usable = |n: u8| from_step.is_none_or(|from| n < from);
    for (n, step, _) in numbered().filter(|(_, _, k)| k.loads()) {
        if !usable(n) {
            continue;
        }
        if !enough(step) {
            f.short_steps.push(n);
        } else if slower(step, base, f.idle_jitter) == Some(true) {
            f.first_slow_step.get_or_insert(n);
            f.slow_steps.push(n);
        }
    }
    if let Some((n, step, _)) = numbered().find(|(_, _, k)| *k == StepKind::Recovery) {
        if usable(n) {
            if enough(step) {
                f.recovery_ms = recovered_at(recovery, p90 + RECOVER_MARGIN_MS);
                f.recovered = Some(f.recovery_ms.is_some());
            } else {
                f.short_steps.push(n);
            }
        }
    }
    f
}
/// When the first run of good windows began; a window without a value breaks a run.
fn recovered_at(win: &[Option<u32>], limit: f64) -> Option<u32> {
    let mut run = 0;
    for (i, w) in win.iter().enumerate() {
        run = if w.is_some_and(|v| f64::from(v) <= limit) {
            run + 1
        } else {
            0
        };
        if run == RECOVER_RUN {
            return u32::try_from(i + 1 - RECOVER_RUN)
                .ok()
                .map(|n| n * WINDOW_MS);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use StepKind::{Both, Down, Idle, Recovery, Up};
    /// the plan before the upload-only step
    const K4: [StepKind; 4] = [Idle, Down, Both, Recovery];

    fn ok(sent: u32, rtt: u32) -> Sample {
        Sample {
            sent,
            rtt,
            outcome: Outcome::Ok,
        }
    }
    fn flat(v: u32, n: usize) -> StepStats {
        stats(&vec![Some(v); n])
    }
    #[test]
    fn probes_give_timeouts_and_jitter_within_the_step_only() {
        let lost = Sample {
            sent: 300,
            rtt: CAP_MS,
            outcome: Outcome::Timeout,
        };
        let failed = Sample {
            sent: 350,
            rtt: 0,
            outcome: Outcome::Failed,
        };
        // sent out of order; 40 → 60 → 30: changes of 20 and 30; the one before `from` is out
        let s = [ok(0, 500), ok(400, 30), lost, ok(100, 40), failed, ok(200, 60)];
        let mut st = StepStats::default();
        probe_stats(&s, 100, 1000, &mut st);
        assert_eq!((st.probes, st.timeouts), (4, 1));
        assert_eq!(st.jitter_ms, Some(25.0));
        let mut none = StepStats::default();
        probe_stats(&s, 2000, 3000, &mut none);
        assert_eq!((none.probes, none.jitter_ms), (0, None));
        assert_eq!(flat(40, 20).p95_ms, Some(40.0));
        let mut idle = flat(40, 20);
        idle.p5_ms = Some(38.0);
        assert_eq!(increase(&flat(39, 20), &idle), Some(0.0));
        assert_eq!(increase(&flat(140, 20), &idle), Some(102.0));
    }
    #[test]
    fn a_window_takes_the_worst_answer() {
        let s = [ok(0, 20), ok(50, 100), ok(100, 40)];
        assert_eq!(windows(&s, 0, 250), vec![Some(100)]);
    }
    #[test]
    fn a_stall_keeps_its_time_weight() {
        // one probe answers fast, then one is lost for 2 s: 8 windows at the cap, not one sample
        let s = [
            ok(0, 20),
            Sample {
                sent: 250,
                rtt: CAP_MS,
                outcome: Outcome::Timeout,
            },
        ];
        let w = windows(&s, 0, 2250);
        assert_eq!(w[0], Some(20));
        assert_eq!(w[1..].iter().filter(|v| **v == Some(CAP_MS)).count(), 8);
    }
    #[test]
    fn instant_failures_and_stops_are_missing_not_fast() {
        let s = [
            Sample {
                sent: 10,
                rtt: 1,
                outcome: Outcome::Failed,
            },
            Sample {
                sent: 300,
                rtt: 900,
                outcome: Outcome::Interrupted,
            },
        ];
        assert_eq!(windows(&s, 0, 500), vec![None, None]);
    }
    #[test]
    fn the_rules() {
        let idle = flat(40, 40);
        let rec = vec![Some(40); 20];
        struct Case {
            name: &'static str,
            video: StepStats,
            backup: StepStats,
            slow: Option<u8>,
        }
        for c in [
            Case {
                name: "fine",
                video: flat(60, 60),
                backup: flat(100, 80),
                slow: None,
            },
            Case {
                name: "100 ms more but not twice",
                video: flat(40, 60),
                backup: flat(139, 80),
                slow: None,
            },
            Case {
                name: "twice but not 100 ms more",
                video: flat(70, 60),
                backup: flat(90, 80),
                slow: None,
            },
            Case {
                name: "only with the backup",
                video: flat(60, 60),
                backup: flat(140, 80),
                slow: Some(3),
            },
            Case {
                name: "already with the video",
                video: flat(300, 60),
                backup: flat(400, 80),
                slow: Some(2),
            },
            Case {
                name: "unresponsive",
                video: flat(60, 60),
                backup: flat(CAP_MS, 80),
                slow: Some(3),
            },
        ] {
            let f = judge(&[idle.clone(), c.video, c.backup, flat(40, 40)], &K4, &rec, None);
            assert_eq!(f.first_slow_step, c.slow, "{}", c.name);
            assert_eq!(f.recovered, Some(true), "{}", c.name);
        }
    }
    #[test]
    fn jitter_at_idle_compares_p90() {
        // idle p50 40, p90 200; a load p50 of 150 is not slower than that idle's p90
        let mut idle: Vec<Option<u32>> = vec![Some(40); 30];
        idle.extend(vec![Some(200); 10]);
        let idle = stats(&idle);
        let f = judge(
            &[idle.clone(), flat(150, 60), flat(150, 80), flat(40, 40)],
            &K4,
            &[],
            None,
        );
        assert!(f.idle_jitter);
        assert_eq!(f.first_slow_step, None);
        let f = judge(
            &[idle, flat(150, 60), flat(500, 80), flat(40, 40)],
            &K4,
            &[],
            None,
        );
        assert_eq!(f.first_slow_step, Some(3));
    }
    #[test]
    fn too_few_windows_says_so() {
        let f = judge(
            &[flat(40, 9), flat(60, 60), flat(60, 60), flat(40, 40)],
            &K4,
            &[],
            None,
        );
        assert!(f.no_baseline);
        assert_eq!(f.first_slow_step, None);
        let f = judge(
            &[flat(40, 40), flat(60, 60), flat(900, 9), flat(40, 40)],
            &K4,
            &[],
            None,
        );
        assert_eq!(f.short_steps, vec![3]);
        assert_eq!(f.first_slow_step, None);
    }
    #[test]
    fn recovery_needs_three_good_windows_in_a_row() {
        let base = [flat(40, 40), flat(60, 60), flat(60, 60), flat(40, 40)];
        let g = Some(50);
        let b = Some(500);
        let f = judge(&base, &K4, &[b, g, g, b, None, g, g, g, g], None);
        assert_eq!(f.recovery_ms, Some(5 * WINDOW_MS));
        let f = judge(&base, &K4, &[b, g, g, b, g, g, None, g], None);
        assert_eq!((f.recovered, f.recovery_ms), (Some(false), None));
    }
    #[test]
    fn a_cell_change_voids_that_step_and_later() {
        let steps = [flat(40, 40), flat(300, 60), flat(400, 80), flat(40, 40)];
        let f = judge(&steps, &K4, &[Some(40); 5], Some(3));
        assert_eq!(f.first_slow_step, Some(2));
        assert_eq!(f.recovered, None);
        let f = judge(&steps, &K4, &[Some(40); 5], Some(2));
        assert_eq!(f.first_slow_step, None);
        assert!(judge(&steps, &K4, &[], Some(1)).no_baseline);
    }
    #[test]
    fn the_upload_only_step_is_judged_like_the_others() {
        let steps = [flat(40, 40), flat(45, 60), flat(300, 80), flat(250, 60), flat(40, 40)];
        let kinds = [Idle, Down, Both, Up, Recovery];
        let f = judge(&steps, &kinds, &[Some(40); 5], None);
        assert_eq!(f.first_slow_step, Some(3));
        assert_eq!(f.slow_steps, vec![3, 4]);
        assert_eq!(f.recovered, Some(true));
        let mut dead = steps.clone();
        dead[3] = flat(CAP_MS, 60);
        assert_eq!(judge(&dead, &kinds, &[Some(40); 5], None).unresponsive_steps, vec![4]);
    }
}
