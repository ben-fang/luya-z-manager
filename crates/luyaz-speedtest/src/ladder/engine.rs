//! One ladder run: idle, video load, video + backup load, recovery. The load threads and the
//! probes have separate stop switches, so load can end at step 4 while the probes go on.
use super::bucket::Bucket;
use super::judge::{
    increase, judge, probe_stats, slower, stats, windows, Findings, Outcome, Sample, StepStats,
};
use super::probe::{Probes, CONNECTIONS};
use super::stop::Stopper;
use crate::runner::Pool;
use crate::TestServer;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use ureq::ResponseExt;

const STACK: usize = 512 * 1024;
const TICK: Duration = Duration::from_millis(100);
/// Rates are computed after dropping this much of a step.
const RAMP: Duration = Duration::from_secs(2);
/// A step reaching less than this share of its target says so.
const REACHED: f64 = 0.9;
/// Below this share the load did not happen, and nothing can be said about holding up under it.
const LOADED: f64 = 0.5;
/// No byte in this long, in one direction: the report says stalled, the connections stay.
const STALL: Duration = Duration::from_secs(5);
/// A socket read or write silent this long has failed; one reconnect is allowed.
const IO_LIMIT: Duration = Duration::from_secs(10);
const UPLOAD_POST: usize = 256 * 1024;
const READ: usize = 64 * 1024;
const TRIAL_TIME: Duration = Duration::from_secs(3);
const CHECK_POST: usize = 64 * 1024;
const MAX_TRIALS: usize = 3;
const NICE: libc::c_int = 10;
/// step 1 disturbed (other traffic, a cell change) is measured again this many times before
/// the run gives up its baseline: phones and laptops on the LAN sync in bursts
const IDLE_REDOS: u8 = 2;

/// What a step loads. A run is idle, then its load steps in order, then recovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    #[default]
    Idle,
    /// download only (video)
    Down,
    /// download and upload together
    Both,
    /// upload only: tells an upload that queues by itself from one that needs both
    Up,
    Recovery,
}
impl StepKind {
    pub fn down(self) -> bool {
        matches!(self, Self::Down | Self::Both)
    }
    pub fn up(self) -> bool {
        matches!(self, Self::Both | Self::Up)
    }
    pub fn loads(self) -> bool {
        self.down() || self.up()
    }
}
const HOME_STEPS: &[(StepKind, Duration)] = &[
    (StepKind::Idle, Duration::from_secs(10)),
    (StepKind::Down, Duration::from_secs(15)),
    (StepKind::Both, Duration::from_secs(20)),
    (StepKind::Up, Duration::from_secs(15)),
    (StepKind::Recovery, Duration::from_secs(10)),
];
const ROAMING_STEPS: &[(StepKind, Duration)] = &[
    (StepKind::Idle, Duration::from_secs(5)),
    (StepKind::Down, Duration::from_secs(8)),
    (StepKind::Both, Duration::from_secs(8)),
    (StepKind::Up, Duration::from_secs(8)),
    (StepKind::Recovery, Duration::from_secs(5)),
];
/// The fixed loads. Different places are compared on these, so they are not tuned per run.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub name: &'static str,
    /// the steps and their lengths: idle first, recovery last
    pub steps: &'static [(StepKind, Duration)],
    pub down_mbps: f64,
    pub down_conns: usize,
    pub up_mbps: f64,
    pub up_conns: usize,
    /// application bytes, reserved before use
    pub budget: u64,
    /// how much a node's download trial may read; small where data is dear
    pub trial_bytes: u64,
    /// a gap after step 1, not measured: calibration connects its call here
    pub gap_after_idle: Duration,
}
impl Plan {
    pub const fn home() -> Self {
        Self {
            name: "home",
            steps: HOME_STEPS,
            down_mbps: 25.0,
            down_conns: 2,
            up_mbps: 30.0,
            up_conns: 4,
            budget: 240_000_000,
            trial_bytes: 5 * 1024 * 1024,
            gap_after_idle: Duration::ZERO,
        }
    }
    pub const fn roaming() -> Self {
        Self {
            name: "roaming",
            steps: ROAMING_STEPS,
            down_mbps: 3.0,
            down_conns: 1,
            up_mbps: 3.0,
            up_conns: 1,
            budget: 12_000_000,
            trial_bytes: 256 * 1024,
            gap_after_idle: Duration::ZERO,
        }
    }
}
/// Public sources needing nothing set up: load and probes need not share a host, because the
/// queue being measured is on the cellular link. 2026-11-05 from the main device: about 88 Mbps
/// from each mirror, 9 Mbps to Cloudflare, so an upload short of target says so in the report.
pub fn public_nodes() -> Vec<Node> {
    const PROBE: &str = "http://connect.rom.miui.com/generate_204";
    const UPLOAD: &str = "https://speed.cloudflare.com/__up";
    [
        (
            "TUNA mirror + Cloudflare",
            "https://mirrors.tuna.tsinghua.edu.cn/ubuntu/ls-lR.gz",
        ),
        (
            "Huawei mirror + Cloudflare",
            "https://repo.huaweicloud.com/ubuntu/ls-lR.gz",
        ),
    ]
    .into_iter()
    .map(|(name, download)| Node {
        name: name.into(),
        ping_url: PROBE.into(),
        download_url: download.into(),
        upload_url: UPLOAD.into(),
    })
    .collect()
}
/// Where a run connects.
#[derive(Clone, Debug, Serialize)]
pub struct Node {
    pub name: String,
    #[serde(skip)]
    pub ping_url: String,
    #[serde(skip)]
    pub download_url: String,
    #[serde(skip)]
    pub upload_url: String,
}
impl From<&TestServer> for Node {
    fn from(s: &TestServer) -> Self {
        Self {
            name: s.name.clone(),
            ping_url: s.ping_url.clone(),
            download_url: s.download_url.clone(),
            upload_url: s.url.clone(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// end the run: this run does not count (network down, overheating, roaming began)
    Abort,
    /// this step and later ones have no baseline (cell or RAT changed, other traffic at idle)
    Void,
    /// a note in the report only
    Note,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub code: &'static str,
}
/// The device side, supplied by the core: called about once a second during a run.
pub trait Watch: Send {
    /// `step` is 1..=4; `app_bytes` the run's application bytes so far.
    fn tick(&mut self, step: u8, app_bytes: u64) -> Vec<Event>;
    /// step 1 begins again: what was seen in it so far is forgotten
    fn redo_idle(&mut self) {}
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Progress {
    /// 0 before the first step
    pub step: u8,
    /// what the step loads (the plan says which number is which)
    pub kind: StepKind,
    pub elapsed_ms: u64,
    pub stopping: bool,
    pub last_window_ms: Option<u32>,
    pub down_bytes: u64,
    pub up_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stall {
    pub step: u8,
    pub upload: bool,
    pub secs: u32,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct StepReport {
    pub kind: StepKind,
    pub stats: StepStats,
    /// where the step's measuring began and ended, in the run's clock (as `samples`)
    pub from_ms: u32,
    pub to_ms: u32,
    pub target_down_mbps: Option<f64>,
    pub target_up_mbps: Option<f64>,
    pub down_mbps: Option<f64>,
    pub up_mbps: Option<f64>,
    /// below 90% of a target
    pub short_of_target: bool,
    /// a load direction gave up: nothing is concluded from this step
    pub failed: bool,
    /// the budget ran out and ended this step
    pub budget_ended: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub plan: &'static str,
    pub node: Option<Node>,
    pub steps: Vec<StepReport>,
    pub findings: Findings,
    pub notes: Vec<Event>,
    pub stalls: Vec<Stall>,
    /// why the run ended early; `None`: it ran to the end
    pub aborted: Option<&'static str>,
    pub app_bytes: u64,
    /// `[ms since the run's clock began, delay ms, outcome]`, for the detail page
    pub samples: Vec<(u32, u32, u8)>,
    /// `[ms, download kbps, upload kbps]` over the second before, about once a second
    pub rates: Vec<(u32, u32, u32)>,
}

fn lower_priority() {
    // SAFETY: changes this thread's nice value only; takes no pointers.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, 0, NICE);
    }
}
fn spawn<'s, 'e>(sc: &'s thread::Scope<'s, 'e>, f: impl FnOnce() + Send + 's) {
    let _ = thread::Builder::new()
        .stack_size(STACK)
        .spawn_scoped(sc, move || {
            lower_priority();
            f();
        });
}
fn with_query(url: &str, n: u64) -> String {
    format!(
        "{url}{}ladder={n}",
        if url.contains('?') { '&' } else { '?' }
    )
}

#[derive(Default)]
struct Dir {
    bytes: AtomicU64,
    failed: AtomicBool,
}
struct Load<'a> {
    stop: &'a Stopper,
    budget: &'a Pool,
    spent: &'a AtomicBool,
}
impl Load<'_> {
    /// Requests one after another on a kept-alive connection, read at the bucket's rate.
    fn download(&self, url: &str, bucket: &Bucket, dir: &Dir, base: u64) {
        let agent = self.stop.agent_idle(Some(IO_LIMIT));
        let mut buf = vec![0u8; READ];
        let chunk = bucket.round_bytes().min(READ);
        let (mut n, mut fails) = (base, 0);
        'requests: while !self.stop.is_stopped() {
            n += 1;
            let mut body = match agent.get(with_query(url, n)).call() {
                Ok(r) => r.into_body(),
                Err(_) if self.stop.is_stopped() => break,
                Err(_) => {
                    fails += 1;
                    if fails > 1 {
                        dir.failed.store(true, Ordering::SeqCst);
                        break;
                    }
                    continue;
                }
            };
            let mut reader = body.with_config().limit(u64::MAX).reader();
            loop {
                if self.stop.is_stopped() {
                    break 'requests;
                }
                let want = self.budget.take(chunk as u64);
                if want == 0 {
                    self.spent.store(true, Ordering::SeqCst);
                    break 'requests;
                }
                let dst = &mut buf[..usize::try_from(want).unwrap_or(chunk).min(chunk)];
                match std::io::Read::read(&mut reader, dst) {
                    Ok(0) => {
                        self.budget.give(want);
                        continue 'requests;
                    }
                    Ok(k) => {
                        self.budget.give(want.saturating_sub(k as u64));
                        dir.bytes.fetch_add(k as u64, Ordering::SeqCst);
                        fails = 0;
                        // paid by what arrived: reads often return less than asked
                        if !bucket.pay(k as u64, self.stop) {
                            break 'requests;
                        }
                    }
                    Err(_) => {
                        self.budget.give(want);
                        if self.stop.is_stopped() {
                            break 'requests;
                        }
                        fails += 1;
                        if fails > 1 {
                            dir.failed.store(true, Ordering::SeqCst);
                            break 'requests;
                        }
                        continue 'requests;
                    }
                }
            }
        }
    }
    /// Fixed POSTs on a kept-alive connection; what was sent is spent, acknowledged or not.
    fn upload(&self, url: &str, bucket: &Bucket, dir: &Dir, base: u64) {
        let agent = self.stop.agent_idle(Some(IO_LIMIT));
        let payload = vec![0u8; UPLOAD_POST];
        let (mut n, mut fails) = (base, 0);
        while bucket.pay(UPLOAD_POST as u64, self.stop) {
            if self.budget.take(UPLOAD_POST as u64) < UPLOAD_POST as u64 {
                self.spent.store(true, Ordering::SeqCst);
                break;
            }
            n += 1;
            let r = agent
                .post(with_query(url, n))
                .send(&payload[..])
                .and_then(|r| r.into_body().with_config().limit(64 * 1024).read_to_vec());
            match r {
                Ok(_) => {
                    dir.bytes.fetch_add(UPLOAD_POST as u64, Ordering::SeqCst);
                    fails = 0;
                }
                Err(_) if self.stop.is_stopped() => break,
                Err(_) => {
                    fails += 1;
                    if fails > 1 {
                        dir.failed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
    }
}
/// A direction's stall tracker, fed every tick.
struct StallWatch {
    upload: bool,
    last_bytes: u64,
    last_at: Instant,
}
impl StallWatch {
    fn new(upload: bool, now: Instant) -> Self {
        Self {
            upload,
            last_bytes: 0,
            last_at: now,
        }
    }
    fn tick(&mut self, bytes: u64, now: Instant, step: u8, out: &mut Vec<Stall>) {
        if bytes != self.last_bytes {
            self.last_bytes = bytes;
            self.close(now, step, out);
            self.last_at = now;
        }
    }
    fn close(&self, now: Instant, step: u8, out: &mut Vec<Stall>) {
        let silent = now.saturating_duration_since(self.last_at);
        if silent >= STALL {
            out.push(Stall {
                step,
                upload: self.upload,
                secs: silent.as_secs() as u32,
            });
        }
    }
}
fn rate(from: u64, to: u64, secs: f64) -> Option<f64> {
    (secs > 0.0).then(|| (to.saturating_sub(from)) as f64 * 8.0 / secs / 1e6)
}
fn outcome_code(o: Outcome) -> u8 {
    match o {
        Outcome::Ok => 0,
        Outcome::Timeout => 1,
        Outcome::Failed => 2,
        Outcome::Interrupted => 3,
    }
}
/// What became of an attempt on one node.
enum Attempt {
    /// step 1 or 2 could not run on this node: another may do
    NodeFailed,
    Done(Box<Report>),
}

/// Run the ladder on the first node that carries steps 1 and 2 (at most 3 are tried). `cancel`
/// ends everything within the design's 2 s; `progress` is overwritten as the run goes.
pub fn run(
    plan: &Plan,
    nodes: &[Node],
    cancel: &Stopper,
    watch: &mut dyn Watch,
    progress: &Mutex<Progress>,
) -> Report {
    let budget = Pool::new(plan.budget);
    for node in nodes.iter().take(MAX_TRIALS) {
        match attempt(plan, node, cancel, watch, progress, &budget) {
            Attempt::Done(r) => return *r,
            Attempt::NodeFailed if cancel.is_stopped() => break,
            Attempt::NodeFailed => {}
        }
    }
    Report {
        plan: plan.name,
        aborted: Some(if cancel.is_stopped() {
            "stopped"
        } else {
            "no_usable_node"
        }),
        app_bytes: plan.budget - budget.room(),
        ..Report::default()
    }
}
fn attempt(
    plan: &Plan,
    node: &Node,
    cancel: &Stopper,
    watch: &mut dyn Watch,
    progress: &Mutex<Progress>,
    budget: &Pool,
) -> Attempt {
    let clock = Instant::now();
    let probe_stop = Stopper::new();
    let load_stop = Stopper::new();
    let spent = AtomicBool::new(false);
    let (down, up) = (Dir::default(), Dir::default());
    let probes = Probes {
        url: &node.ping_url,
        stop: &probe_stop,
        clock,
        next_slot: AtomicU64::new(0),
        samples: Mutex::new(Vec::new()),
        budget,
    };
    let load = Load {
        stop: &load_stop,
        budget,
        spent: &spent,
    };
    // downloads end before the upload-only step; everything ends at recovery
    let down_stop = Stopper::new();
    let down_load = Load {
        stop: &down_stop,
        budget,
        spent: &spent,
    };
    let down_bucket = Bucket::new(plan.down_mbps, clock);
    let up_bucket = Bucket::new(plan.up_mbps, clock);
    let start_up = AtomicBool::new(false);
    let start_down = AtomicBool::new(false);
    let mut out = Report {
        plan: plan.name,
        node: Some(node.clone()),
        ..Report::default()
    };
    let mut bounds = vec![0u32; plan.steps.len()];
    let mut ends = vec![0u32; plan.steps.len()];
    let mut void_from: Option<u8> = None;
    let mut idle_redos = 0u8;
    let mut rates: Vec<(u32, u32, u32)> = Vec::new();
    let mut rate_at: Option<(Instant, u64, u64)> = None;
    let mut node_failed = false;
    let mut steps: Vec<StepReport> = Vec::new();

    thread::scope(|sc| {
        for i in 0..CONNECTIONS {
            let probes = &probes;
            spawn(sc, move || probes.run(i));
        }
        for i in 0..plan.down_conns {
            let (load, start, stop, b, d, url) = (
                &down_load,
                &start_down,
                &down_stop,
                &down_bucket,
                &down,
                &node.download_url,
            );
            spawn(sc, move || {
                while !start.load(Ordering::SeqCst) {
                    if stop.is_stopped() {
                        return;
                    }
                    thread::sleep(TICK / 5);
                }
                load.download(url, b, d, i as u64 * 1_000_000);
            });
        }
        for i in 0..plan.up_conns {
            let (load, start, stop, b, d, url) = (
                &load,
                &start_up,
                &load_stop,
                &up_bucket,
                &up,
                &node.upload_url,
            );
            spawn(sc, move || {
                while !start.load(Ordering::SeqCst) {
                    if stop.is_stopped() {
                        return;
                    }
                    thread::sleep(TICK / 5);
                }
                load.upload(url, b, d, i as u64 * 1_000_000);
            });
        }
        let stop_load = || {
            down_stop.stop();
            load_stop.stop();
        };
        let stop_all = || {
            stop_load();
            probe_stop.stop();
        };
        // the step each direction's load is in, while it runs
        let (mut down_on, mut up_on): (Option<u8>, Option<u8>) = (None, None);
        let mut stall_down = StallWatch::new(false, clock);
        let mut stall_up = StallWatch::new(true, clock);
        let mut next_watch = Instant::now();
        // the probes' connections are made before step 1
        thread::sleep(Duration::from_millis(300));

        'steps: for (i, (kind, len)) in plan.steps.iter().enumerate() {
            let (kind, n) = (*kind, i as u8 + 1);
            let mut began = Instant::now();
            let began_ms = u32::try_from(clock.elapsed().as_millis()).unwrap_or(u32::MAX);
            bounds[i] = began_ms;
            let mut report = StepReport {
                kind,
                ..StepReport::default()
            };
            let loaded = kind.loads();
            if loaded && load_stop.is_stopped() {
                // the budget ended an earlier step: this one without its load would read as a pass
                report.budget_ended = true;
                steps.push(report);
                continue;
            }
            // a stall still going when its load ends was never closed by new bytes
            if let Some(was) = down_on.filter(|_| !kind.down()) {
                stall_down.close(began, was, &mut out.stalls);
                down_stop.stop();
                down_on = None;
            }
            if let Some(was) = up_on.filter(|_| !kind.up()) {
                stall_up.close(began, was, &mut out.stalls);
                up_on = None;
            }
            if kind == StepKind::Recovery {
                stop_load();
            }
            if kind.down() {
                if down_on.is_none() && !down_stop.is_stopped() {
                    stall_down = StallWatch::new(false, began);
                    start_down.store(true, Ordering::SeqCst);
                }
                down_on = Some(n);
                report.target_down_mbps = Some(plan.down_mbps);
            }
            if kind.up() {
                if up_on.is_none() {
                    stall_up = StallWatch::new(true, began);
                    start_up.store(true, Ordering::SeqCst);
                }
                up_on = Some(n);
                report.target_up_mbps = Some(plan.up_mbps);
            }
            let mut ramped: Option<(Instant, u64, u64)> = None;
            let mut end = began + *len;
            let mut budget_ended = false;
            while Instant::now() < end {
                thread::sleep(TICK.min(end.saturating_duration_since(Instant::now())));
                let now = Instant::now();
                if cancel.is_stopped() {
                    progress.lock().unwrap_or_else(|e| e.into_inner()).stopping = true;
                    out.aborted = Some("stopped");
                    break 'steps;
                }
                let (d, u) = (
                    down.bytes.load(Ordering::SeqCst),
                    up.bytes.load(Ordering::SeqCst),
                );
                match rate_at {
                    Some((t, d0, u0)) if now.duration_since(t) >= Duration::from_secs(1) => {
                        let secs = now.duration_since(t).as_secs_f64();
                        let kbps =
                            |a: u64, b: u64| (b.saturating_sub(a) as f64 * 8.0 / secs / 1e3) as u32;
                        let ms = u32::try_from(clock.elapsed().as_millis()).unwrap_or(u32::MAX);
                        rates.push((ms, kbps(d0, d), kbps(u0, u)));
                        rate_at = Some((now, d, u));
                    }
                    None => rate_at = Some((now, d, u)),
                    _ => {}
                }
                if loaded && ramped.is_none() && now.duration_since(began) >= RAMP {
                    ramped = Some((now, d, u));
                }
                if kind.down() {
                    stall_down.tick(d, now, n, &mut out.stalls);
                }
                if kind.up() {
                    stall_up.tick(u, now, n, &mut out.stalls);
                }
                let app = plan.budget - budget.room();
                {
                    let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                    p.step = n;
                    p.kind = kind;
                    p.elapsed_ms = clock.elapsed().as_millis() as u64;
                    p.down_bytes = d;
                    p.up_bytes = u;
                    p.last_window_ms = probes
                        .samples
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .last()
                        .map(|s| s.rtt);
                }
                if now >= next_watch {
                    next_watch = now + Duration::from_secs(1);
                    let mut redo = false;
                    for mut e in watch.tick(n, app) {
                        match e.kind {
                            EventKind::Abort => out.aborted = Some(e.code),
                            EventKind::Void if n == 1 && idle_redos < IDLE_REDOS => {
                                redo = true;
                                continue;
                            }
                            EventKind::Void => {
                                if n == 1 && e.code == "other_traffic" {
                                    // still busy after the redos: say so, not just "no baseline"
                                    e.code = "idle_busy";
                                }
                                void_from.get_or_insert(n);
                            }
                            EventKind::Note => {}
                        }
                        out.notes.push(e);
                    }
                    if redo && out.aborted.is_none() {
                        idle_redos += 1;
                        watch.redo_idle();
                        out.notes.push(Event {
                            kind: EventKind::Note,
                            code: "idle_redone",
                        });
                        began = now;
                        end = now + *len;
                        bounds[i] = u32::try_from(clock.elapsed().as_millis()).unwrap_or(u32::MAX);
                    }
                    if out.aborted.is_some() {
                        break 'steps;
                    }
                }
                let down_failed = kind.down() && down.failed.load(Ordering::SeqCst);
                let up_failed = kind.up() && up.failed.load(Ordering::SeqCst);
                if loaded && (down_failed || up_failed) {
                    report.failed = true;
                    // the first load step failing: the node, not the link
                    if n == 2 {
                        node_failed = true;
                        break 'steps;
                    }
                    break;
                }
                if loaded && spent.load(Ordering::SeqCst) {
                    budget_ended = true;
                    break;
                }
            }
            ends[i] = u32::try_from(clock.elapsed().as_millis()).unwrap_or(u32::MAX);
            report.budget_ended = budget_ended;
            if let Some((at, d0, u0)) = ramped {
                let secs = at.elapsed().as_secs_f64();
                if kind.down() {
                    report.down_mbps = rate(d0, down.bytes.load(Ordering::SeqCst), secs);
                }
                if kind.up() {
                    report.up_mbps = rate(u0, up.bytes.load(Ordering::SeqCst), secs);
                }
                let short = |got: Option<f64>, target: Option<f64>| match (got, target) {
                    (Some(g), Some(t)) => g < t * REACHED,
                    _ => false,
                };
                report.short_of_target = short(report.down_mbps, report.target_down_mbps)
                    || short(report.up_mbps, report.target_up_mbps);
            }
            steps.push(report);
            if n == 1 {
                // no answers at all to the idle probes: this node is not going to do
                let valid = probes
                    .samples
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|s| s.outcome == Outcome::Ok);
                if !valid {
                    node_failed = true;
                    break 'steps;
                }
            }
            if budget_ended || steps.last().is_some_and(|s| s.failed) {
                stop_load(); // straight to recovery
            }
            if n == 1 && !plan.gap_after_idle.is_zero() {
                let until = Instant::now() + plan.gap_after_idle;
                while Instant::now() < until {
                    thread::sleep(TICK);
                    if cancel.is_stopped() {
                        out.aborted = Some("stopped");
                        break 'steps;
                    }
                }
            }
        }
        stop_all();
        // a cancel during the threads' winding down must not hold the join
        if cancel.is_stopped() {
            progress.lock().unwrap_or_else(|e| e.into_inner()).stopping = true;
        }
    });
    if node_failed && !cancel.is_stopped() && out.aborted.is_none() {
        return Attempt::NodeFailed;
    }
    let samples = probes
        .samples
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    out.rates = rates;
    finish(
        &mut out, plan, &samples, &bounds, &ends, steps, void_from, budget,
    );
    Attempt::Done(Box::new(out))
}
/// A step below `LOADED` whose delay did not rise says nothing: listed as unloaded. One whose
/// delay rose has found the link's limit: the probes share only the cellular link with the load,
/// so a slow far end alone would not queue them. If no load step carried its load and nothing
/// was slower anyway, there is no recovery to speak of either.
fn apply_unloaded(f: &mut Findings, steps: &[StepReport], void_from: Option<u8>) {
    let low = |got: Option<f64>, target: Option<f64>| {
        target.is_some_and(|t| got.unwrap_or(0.0) < t * LOADED)
    };
    let base = steps.first().map(|s| &s.stats);
    // a step with a target carried a load (results from before kinds have targets too)
    let loaded = |s: &&StepReport| s.target_down_mbps.is_some() || s.target_up_mbps.is_some();
    let loads = steps.iter().filter(loaded).count();
    for (i, s) in steps.iter().enumerate().filter(|(_, s)| loaded(s)) {
        let n = i as u8 + 1;
        if !(low(s.down_mbps, s.target_down_mbps) || low(s.up_mbps, s.target_up_mbps)) {
            continue;
        }
        // the link stopped answering: that is the finding, not a load that did not happen
        if f.unresponsive_steps.contains(&n) {
            continue;
        }
        let usable = void_from.is_none_or(|from| n < from);
        let slow = usable
            && f.first_slow_step.is_some_and(|first| first <= n)
            && base.and_then(|b| slower(&s.stats, b, f.idle_jitter)) == Some(true);
        if slow {
            f.queued_steps.push(n);
        } else {
            f.unloaded_steps.push(n);
        }
    }
    if f.unloaded_steps.len() == loads && f.first_slow_step.is_none() {
        f.recovered = None;
        f.recovery_ms = None;
    }
}
#[allow(clippy::too_many_arguments)]
fn finish(
    out: &mut Report,
    plan: &Plan,
    samples: &[Sample],
    bounds: &[u32],
    ends: &[u32],
    mut steps: Vec<StepReport>,
    void_from: Option<u8>,
    budget: &Pool,
) {
    // steps never reached (a node that failed, an abort) are reported empty, of their kind
    for &(kind, _) in plan.steps.iter().skip(steps.len()) {
        steps.push(StepReport {
            kind,
            ..StepReport::default()
        });
    }
    let win = |i: usize| windows(samples, bounds[i], ends[i]);
    for (i, s) in steps.iter_mut().enumerate() {
        s.stats = stats(&win(i));
        probe_stats(samples, bounds[i], ends[i], &mut s.stats);
        (s.from_ms, s.to_ms) = (bounds[i], ends[i]);
    }
    let idle = steps.first().map(|s| s.stats.clone()).unwrap_or_default();
    for s in steps.iter_mut().filter(|s| s.kind.loads()) {
        s.stats.increase_ms = increase(&s.stats, &idle);
    }
    let stat: Vec<StepStats> = steps.iter().map(|s| s.stats.clone()).collect();
    let kinds: Vec<StepKind> = steps.iter().map(|s| s.kind).collect();
    let recovery = kinds
        .iter()
        .position(|k| *k == StepKind::Recovery)
        .map(win)
        .unwrap_or_default();
    // A failed step says nothing, so it and later steps are not judged. An aborted run counts
    // for nothing at all.
    let failed_at = steps.iter().position(|s| s.failed).map(|i| i as u8 + 1);
    let void = match (void_from, failed_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if out.aborted.is_none() {
        out.findings = judge(&stat, &kinds, &recovery, void);
        apply_unloaded(&mut out.findings, &steps, void);
    }
    out.steps = steps;
    out.app_bytes = plan.budget - budget.room();
    out.samples = samples
        .iter()
        .map(|s| (s.sent, s.rtt, outcome_code(s.outcome)))
        .collect();
}

/// Order the candidates for a run: those that answer, can take an upload, and (with `trial`)
/// download fastest. A server redirecting elsewhere is replaced by where it redirects to.
pub fn pick_nodes(plan: &Plan, candidates: Vec<Node>, cancel: &Stopper) -> Vec<Node> {
    let mut reach: Vec<(Duration, Node)> = thread::scope(|sc| {
        let handles: Vec<_> = candidates
            .into_iter()
            .filter_map(|n| {
                thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(sc, move || reach(&n, cancel))
                    .ok()
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    });
    reach.sort_by_key(|(rtt, _)| *rtt);
    let mut tried: Vec<(f64, Node)> = Vec::new();
    for (_, node) in reach.into_iter().take(MAX_TRIALS) {
        if cancel.is_stopped() {
            break;
        }
        // low latency says nothing about carrying a load, so every plan tries both directions
        let (Some(up), Some(down)) = (
            upload_mbps(&node, cancel),
            trial_mbps(&node, plan.trial_bytes, cancel),
        ) else {
            continue;
        };
        let score = up.min(down);
        tried.push((score, node));
    }
    tried.sort_by(|a, b| b.0.total_cmp(&a.0));
    tried.into_iter().map(|(_, n)| n).collect()
}
/// One request on the latency URL. A redirect moves all three URLs to the new place, since the
/// probes and the POSTs must not spend every request on it.
fn reach(node: &Node, cancel: &Stopper) -> Option<(Duration, Node)> {
    const LATENCY: &str = "latency.txt";
    let t = Instant::now();
    let r = cancel
        .agent()
        .get(with_query(&node.ping_url, 0))
        .config()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .call()
        .ok()?;
    let rtt = t.elapsed();
    let final_url = r.get_uri().to_string();
    let _ = r.into_body().with_config().limit(1024).read_to_vec().ok()?;
    let mut node = node.clone();
    // public sources are three unrelated hosts: nothing to move together
    let Some(old_base) = node.ping_url.strip_suffix(LATENCY).map(str::to_string) else {
        return Some((rtt, node));
    };
    let new_base = final_url
        .split('?')
        .next()?
        .strip_suffix(LATENCY)?
        .to_string();
    if old_base != new_base {
        for u in [
            &mut node.ping_url,
            &mut node.download_url,
            &mut node.upload_url,
        ] {
            let moved = u.strip_prefix(&old_base)?.to_string();
            *u = format!("{new_base}{moved}");
        }
    }
    Some((rtt, node))
}
fn upload_mbps(node: &Node, cancel: &Stopper) -> Option<f64> {
    let body = vec![0u8; CHECK_POST];
    let t = Instant::now();
    cancel
        .agent()
        .post(with_query(&node.upload_url, 0))
        .config()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .send(&body[..])
        .and_then(|r| r.into_body().with_config().limit(64 * 1024).read_to_vec())
        .ok()?;
    Some(CHECK_POST as f64 * 8.0 / t.elapsed().as_secs_f64().max(0.001) / 1e6)
}
fn trial_mbps(node: &Node, cap: u64, cancel: &Stopper) -> Option<f64> {
    let t = Instant::now();
    let r = cancel
        .agent()
        .get(with_query(&node.download_url, 0))
        .config()
        .timeout_global(Some(TRIAL_TIME))
        .build()
        .call()
        .ok()?;
    let mut body = r.into_body();
    let mut reader = body.with_config().limit(u64::MAX).reader();
    let (mut got, mut buf) = (0u64, vec![0u8; READ]);
    while got < cap {
        match std::io::Read::read(&mut reader, &mut buf) {
            Ok(k) if k > 0 => got += k as u64,
            _ => break,
        }
    }
    (got > 0).then(|| got as f64 * 8.0 / t.elapsed().as_secs_f64().max(0.001) / 1e6)
}

#[cfg(test)]
mod tests;
