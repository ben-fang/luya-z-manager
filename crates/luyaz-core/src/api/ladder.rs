//! The load ladder's device side (`docs/designs/load-ladder.md`): admission, the once-a-second
//! watch over the run, and results in `/tmp/luyaz-ladder/` (memory only, gone on reboot).
//! `started` marks a run in progress: seen with nothing running, the core restarted during it.

use axum::{
    body::Bytes,
    extract::{Query, State},
    response::Response,
};
use luyaz_speedtest::ladder::{self, Event, EventKind, Node, Plan, Progress, Report, Stopper};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{reply, Compat};
use crate::heat;

/// Results, kept across reboots: one write a run, so the flash hardly notices.
const DIR: &str = "/data/luyaz/core/ladder";
/// The run in progress (`started`), in memory; results were kept here before 1.93.
const RUN_DIR: &str = "/tmp/luyaz-ladder";
const STARTED: &str = "started";
const KEEP: usize = 30;
/// runs listed by `/api/ladder/history`
const HISTORY: usize = 20;
const MAX_BYTES: usize = 64 * 1024;
/// A node picked this recently is used again, saving the trial's data.
const NODE_REUSE: Duration = Duration::from_secs(15 * 60);
const MIN_BATTERY_START: u8 = 20;
const MIN_BATTERY_RUN: u8 = 15;
/// Before the start: traffic above this is worth a question.
const BUSY_MBPS: f64 = 1.0;
const BUSY_LOOK: Duration = Duration::from_secs(3);
/// Temperatures unread this many ticks in a row end the run.
const HEAT_MISSES: u32 = 3;
const COUNTERS_EVERY: Duration = Duration::from_secs(2);
/// The vendor counter shows bytes up to about 1 s late (11-06, polled every 0.5 s): picking a
/// node just before step 1 would otherwise count as other traffic at idle.
const COUNTER_SETTLE: Duration = Duration::from_millis(1500);
/// A load step's first seconds are not compared, as its rates drop them: a new download fills
/// its receive buffer at once (about 2.8 MB within 1 s on 11-06), bytes the run reads later.
const LOAD_SETTLE: Duration = Duration::from_secs(2);
const OTHER_MIN_MBPS: f64 = 2.0;
const OTHER_SHARE: f64 = 0.2;
const CALL_GAP: Duration = Duration::from_secs(30);
/// columns in the screen's chart of a result
const SCREEN_POINTS: usize = 96;

#[derive(Default)]
pub struct Ladder {
    progress: Arc<Mutex<Progress>>,
    cancel: Mutex<Option<Stopper>>,
    active: AtomicBool,
    last_node: Mutex<Option<(Instant, Vec<Node>)>>,
}

/// What one look at the device found. `None` = not read this time.
#[derive(Clone, Debug, Default)]
pub(super) struct Look {
    pub cell: Option<String>,
    pub rat: Option<String>,
    pub carriers: Option<u8>,
    pub connected: Option<bool>,
    pub roaming: Option<bool>,
    /// `None`: no sensor answered
    pub heat: Option<HeatLook>,
    pub battery: Option<(u8, bool)>,
    /// rx + tx from the vendor counters
    pub counter: Option<u64>,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct HeatLook {
    pub too_hot: bool,
}
/// The event table, with no I/O. Each thing is reported once.
#[derive(Default)]
pub(super) struct Tracker {
    cell: Option<String>,
    rat: Option<String>,
    carriers: Option<u8>,
    heat_misses: u32,
    counted: Option<(u64, u64, Instant)>,
    /// the run's bytes at the tick before, compared with the late counter
    prev_app: u64,
    /// the step the counter base belongs to, and when it began
    counted_step: Option<(u8, Instant)>,
    other_steps: Vec<u8>,
    voided: bool,
    pub calibrating: bool,
    /// it was roaming from the start: the run was admitted as such
    pub roaming_ok: bool,
}
impl Tracker {
    /// step 1 starts over: its counter base and what it saw are dropped
    pub(super) fn redo_idle(&mut self) {
        self.other_steps.retain(|s| *s != 1);
        self.voided = false;
        self.counted_step = None;
        self.counted = None;
    }
    pub(super) fn step(&mut self, step: u8, app: u64, look: &Look, now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        let abort = |code| Event {
            kind: EventKind::Abort,
            code,
        };
        if look.connected == Some(false) {
            out.push(abort("network_down"));
        }
        if look.roaming == Some(true) && !self.roaming_ok {
            out.push(abort("became_roaming"));
        }
        match look.heat {
            Some(h) => {
                self.heat_misses = 0;
                if h.too_hot {
                    out.push(abort("overheated"));
                }
            }
            None => {
                self.heat_misses += 1;
                if self.heat_misses >= HEAT_MISSES {
                    out.push(abort("heat_unknown"));
                }
            }
        }
        if matches!(look.battery, Some((pct, false)) if pct < MIN_BATTERY_RUN) {
            out.push(abort("low_battery"));
        }
        let moved = |seen: &mut Option<String>, now: &Option<String>| match (&*seen, now) {
            (Some(a), Some(b)) if a != b => {
                *seen = Some(b.clone());
                true
            }
            (None, Some(b)) => {
                *seen = Some(b.clone());
                false
            }
            _ => false,
        };
        let cell = moved(&mut self.cell, &look.cell);
        let rat = moved(&mut self.rat, &look.rat);
        if cell || rat {
            out.push(Event {
                kind: EventKind::Void,
                code: if rat { "rat_changed" } else { "cell_changed" },
            });
        }
        if let Some(n) = look.carriers {
            if self.carriers.is_some_and(|c| c != n) {
                out.push(Event {
                    kind: EventKind::Note,
                    code: "carriers_changed",
                });
            }
            self.carriers = Some(n);
        }
        // each step is compared from its own base, as a whole: a 256 KB POST or a buffer
        // filling makes one 2 s window swing by more than the threshold
        let began = match self.counted_step {
            Some((s, at)) if s == step => at,
            _ => {
                self.counted_step = Some((step, now));
                self.counted = None;
                now
            }
        };
        let settle = if step == 1 {
            COUNTER_SETTLE
        } else {
            LOAD_SETTLE
        };
        let settled = now.duration_since(began) >= settle;
        // the counter is about a tick late: set it beside the bytes of the tick before
        let app = std::mem::replace(&mut self.prev_app, app);
        if let Some(counter) = look.counter.filter(|_| settled) {
            if let Some((c0, a0, t0)) = self.counted {
                let secs = now.duration_since(t0).as_secs_f64().max(0.001);
                let mbps = |b: u64| b as f64 * 8.0 / secs / 1e6;
                let test = mbps(app.saturating_sub(a0));
                let other = mbps(
                    counter
                        .saturating_sub(c0)
                        .saturating_sub(app.saturating_sub(a0)),
                );
                let over = other > OTHER_MIN_MBPS.max(test * OTHER_SHARE);
                if over && !self.calibrating && !self.other_steps.contains(&step) {
                    self.other_steps.push(step);
                    let voids = step == 1 && !self.voided;
                    self.voided |= voids;
                    out.push(Event {
                        kind: if voids {
                            EventKind::Void
                        } else {
                            EventKind::Note
                        },
                        code: "other_traffic",
                    });
                }
            }
            if self.counted.is_none() {
                self.counted = Some((counter, app, now));
            }
        }
        out
    }
}
fn counter_of(traffic: &Result<Value, String>) -> Option<u64> {
    let pre = crate::netrec::Pre::from_reads(
        traffic,
        &Err(String::new()),
        &Err(String::new()),
        &Err(String::new()),
    );
    pre.vendor.map(|v| v.rx + v.tx)
}
/// The stock values include `ipv4_ipv6_connected` as well as `connected`.
fn data_call_up(status: &str) -> bool {
    status.ends_with("connected") && !status.contains("disconnected")
}
fn battery_of(b: &Value) -> Option<(u8, bool)> {
    let pct = u8::try_from(b.get("percent")?.as_i64()?).ok()?;
    let charging = b.get("charging").and_then(Value::as_i64).unwrap_or(0) != 0;
    Some((pct, charging))
}
/// The device, read now. Runs on the ladder's thread; the reads go through the core's runtime.
fn look(c: &Compat, rt: &tokio::runtime::Handle, with_ubus: bool) -> Look {
    let mut l = Look {
        heat: heat::read_now().map(|r| HeatLook {
            too_hot: heat::too_hot_to_run(&r),
        }),
        ..Look::default()
    };
    let hub = c.src.hub.as_ref();
    let view = |name: &str| hub.and_then(|h| h.view(name)).filter(|v| !v.stale);
    if let Some(b) = view("battery") {
        l.battery = battery_of(&b.data);
    }
    if let Some(s) = view("signal") {
        // only a fresh block says roaming here; a stale one must not end a run
        l.roaming = Some(super::speed::roaming(Some(&s)));
        let v = crate::project::screen::net_view_op(&json!({ "net": s.data }), None, None);
        l.carriers = u8::try_from(v.act_n).ok();
    }
    if with_ubus {
        let (traffic, net, wan) = rt.block_on(crate::state::ladder_reads());
        l.counter = counter_of(&traffic);
        l.connected = wan
            .ok()
            .and_then(|w| w.get("connect_status")?.as_str().map(data_call_up));
        let pre = crate::netrec::Pre::from_reads(
            &Err(String::new()),
            &Err(String::new()),
            &Err(String::new()),
            &net,
        );
        if !pre.radio.rat.is_empty() {
            l.rat = Some(pre.radio.rat.clone());
            l.cell = Some(format!("{}/{}", pre.radio.pci, pre.radio.cell_id));
        }
    }
    l
}
struct DeviceWatch {
    c: Compat,
    rt: tokio::runtime::Handle,
    tracker: Tracker,
    next_counters: Instant,
}
impl ladder::Watch for DeviceWatch {
    fn tick(&mut self, step: u8, app: u64) -> Vec<Event> {
        let now = Instant::now();
        let due = now >= self.next_counters;
        if due {
            self.next_counters = now + COUNTERS_EVERY;
        }
        let l = look(&self.c, &self.rt, due);
        self.tracker.step(step, app, &l, now)
    }
    fn redo_idle(&mut self) {
        self.tracker.redo_idle();
    }
}

// ---------------------------------------------------------------------------------------------
// Storage

/// Serialized under the size limit: the raw probes go first, the summary always stays.
fn encode(id: u64, mut r: Report, counter: Option<u64>, context: &Value) -> Vec<u8> {
    let wrap = |r: &Report| {
        json!({"id": id, "counter_bytes": counter, "context": context, "report": r})
            .to_string()
            .into_bytes()
    };
    let full = wrap(&r);
    if full.len() <= MAX_BYTES {
        return full;
    }
    r.samples.clear();
    wrap(&r)
}
fn results(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut v: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let id = p.file_stem()?.to_str()?.parse().ok()?;
            (p.extension()? == "json").then_some((id, p))
        })
        .collect();
    v.sort_by_key(|(id, _)| *id);
    v
}
/// Results from before they were kept on `/data` move there.
fn migrate(c: &Compat) {
    let (from, to) = (c.path(RUN_DIR), c.path(DIR));
    let old = results(&from);
    if old.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(&to);
    for (id, p) in old {
        let dst = to.join(format!("{id}.json"));
        if dst.exists() || std::fs::copy(&p, &dst).is_ok() {
            let _ = std::fs::remove_file(&p);
        }
    }
}
/// Where the run was, as the device saw it at the start: what a comparison shows beside the
/// numbers. Never a place name: the device does not know one.
fn context_now(c: &Compat) -> Value {
    let hub = c.src.hub.as_ref();
    let signal = hub.and_then(|h| h.view("signal")).filter(|v| !v.stale);
    let mut ctx = json!({});
    if let Some(s) = signal {
        let d = &s.data;
        let rat = d["type"].as_str().unwrap_or("");
        // NR leads on SA and NSA alike; LTE otherwise
        let nr = rat.contains("SA") || rat.contains("NR") || rat.contains("5G");
        let (pci, rsrp, snr) = if nr {
            ("nr_pci", "nr_rsrp", "nr_snr")
        } else {
            ("lte_pci", "lte_rsrp", "lte_snr")
        };
        let num = |v: &Value| v.as_f64().or_else(|| v.as_str().and_then(|t| t.parse().ok()));
        ctx = json!({
            "rat": rat,
            "band": d["band"],
            "pci": d[pci],
            "rsrp": num(&d[rsrp]),
            "sinr": num(&d[snr]),
            "operator": d["operator"],
            "roaming": super::speed::roaming(Some(&s)),
        });
        let v = crate::project::screen::net_view_op(&json!({ "net": s.data }), None, None);
        ctx["carriers"] = json!(v.act_n);
    }
    if let Some(h) = heat::read_now() {
        ctx["chip_c"] = json!(h.chip);
        ctx["batt_c"] = json!(h.batt);
    }
    ctx
}
fn save(dir: &Path, id: u64, bytes: &[u8]) {
    let _ = std::fs::create_dir_all(dir);
    let tmp = dir.join(format!("{id}.tmp"));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(format!("{id}.json")));
    }
    let list = results(dir);
    for (_, old) in list.iter().take(list.len().saturating_sub(KEEP)) {
        let _ = std::fs::remove_file(old);
    }
}

// ---------------------------------------------------------------------------------------------
// Routes

#[derive(serde::Deserialize, Default)]
struct StartBody {
    /// the home ladder on a roaming device: asked twice by the screen
    #[serde(default)]
    roaming_ok: bool,
    /// go on although other traffic was seen
    #[serde(default)]
    busy_ok: bool,
    /// a call is connected in the gap after step 1; other traffic is not judged
    #[serde(default)]
    calibrate: bool,
    /// the light ladder wherever the device is
    #[serde(default)]
    light: bool,
}
/// Refused before anything runs, in the design's order after the running claim.
fn refuse_for_device(heat_now: Option<bool>, battery: Option<(u8, bool)>) -> Option<&'static str> {
    if heat_now != Some(false) {
        return Some(if heat_now.is_none() {
            "heat_unknown"
        } else {
            "too_warm"
        });
    }
    if matches!(battery, Some((pct, false)) if pct < MIN_BATTERY_START) {
        return Some("low_battery");
    }
    None
}

/// POST /api/ladder/start
pub async fn start(State(c): State<Compat>, body: Bytes) -> Response {
    let req: StartBody = if body.is_empty() {
        StartBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(_) => return reply(400, json!({"ok": false, "error": "bad_body"})),
        }
    };
    let rt = tokio::runtime::Handle::current();
    let refuse = |code: &str| reply(409, json!({"ok": false, "error": code}));
    let hot = heat::read_now().map(|r| heat::too_warm_to_start(&r));
    let first = {
        let (c, rt) = (c.clone(), rt.clone());
        tokio::task::spawn_blocking(move || look(&c, &rt, false))
            .await
            .unwrap_or_default()
    };
    if let Some(code) = refuse_for_device(hot, first.battery) {
        return refuse(code);
    }
    // roaming as the speed test reads it: unknown counts as roaming
    let signal = c.src.hub.as_ref().and_then(|h| h.view("signal"));
    let roaming = c.src.hub.is_some() && super::speed::roaming(signal.as_ref());
    let mut plan = if req.light || (roaming && !req.roaming_ok) {
        Plan::roaming()
    } else {
        Plan::home()
    };
    if req.calibrate {
        plan.gap_after_idle = CALL_GAP;
    }
    let (t0, ..) = crate::state::ladder_reads().await;
    let c0 = counter_of(&t0);
    if !req.busy_ok && !req.calibrate {
        tokio::time::sleep(BUSY_LOOK).await;
        let (t1, ..) = crate::state::ladder_reads().await;
        if let (Some(a), Some(b)) = (c0, counter_of(&t1)) {
            let mbps = b.saturating_sub(a) as f64 * 8.0 / BUSY_LOOK.as_secs_f64() / 1e6;
            if mbps > BUSY_MBPS {
                return reply(
                    409,
                    json!({"ok": false, "error": "other_traffic", "mbps": mbps}),
                );
            }
        }
    }
    if let Err((s, v)) = super::speed::admit(&c, &c.speed.running_flag()) {
        return reply(s, v);
    }
    let stop = Stopper::new();
    *c.ladder.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());
    *c.ladder.progress.lock().unwrap_or_else(|e| e.into_inner()) = Progress::default();
    c.ladder.active.store(true, Ordering::SeqCst);
    let dir = c.path(RUN_DIR);
    let _ = std::fs::create_dir_all(&dir);
    let id = crate::state::now_secs_for_ladder();
    let _ = std::fs::write(dir.join(STARTED), id.to_string());
    let calibrating = req.calibrate;
    let name = plan.name;
    let (c2, rt2) = (c.clone(), rt);
    let spawned = std::thread::Builder::new()
        .name("ladder".into())
        .spawn(move || {
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                work(&c2, &rt2, &plan, &stop, c0, id, calibrating, roaming);
            }));
            if done.is_err() {
                eprintln!("[ladder] run panicked");
            }
            let _ = std::fs::remove_file(c2.path(RUN_DIR).join(STARTED));
            c2.ladder.active.store(false, Ordering::SeqCst);
            c2.speed.running_flag().store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        let _ = std::fs::remove_file(dir.join(STARTED));
        c.ladder.active.store(false, Ordering::SeqCst);
        c.speed.running_flag().store(false, Ordering::SeqCst);
        return reply(500, json!({"ok": false, "error": "spawn_failed"}));
    }
    reply(200, json!({"ok": true, "id": id, "plan": name}))
}
#[allow(clippy::too_many_arguments)]
fn work(
    c: &Compat,
    rt: &tokio::runtime::Handle,
    plan: &Plan,
    stop: &Stopper,
    c0: Option<u64>,
    id: u64,
    calibrating: bool,
    roaming_ok: bool,
) {
    let reuse = c
        .ladder
        .last_node
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|(at, _)| at.elapsed() < NODE_REUSE)
        .map(|(_, n)| n.clone());
    let nodes = reuse.unwrap_or_else(|| {
        let picked = ladder::pick_nodes(plan, c.speed.ladder_candidates(), stop);
        if !picked.is_empty() {
            *c.ladder.last_node.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), picked.clone()));
        }
        picked
    });
    let mut watch = DeviceWatch {
        c: c.clone(),
        rt: rt.clone(),
        tracker: Tracker {
            calibrating,
            roaming_ok,
            ..Tracker::default()
        },
        next_counters: Instant::now(),
    };
    let context = context_now(c);
    let report = ladder::run(plan, &nodes, stop, &mut watch, &c.ladder.progress);
    let c1 = counter_of(&rt.block_on(crate::state::ladder_reads()).0);
    let used = c0.zip(c1).map(|(a, b)| b.saturating_sub(a));
    migrate(c);
    save(&c.path(DIR), id, &encode(id, report, used, &context));
}
/// GET /api/ladder/progress
pub async fn progress(State(c): State<Compat>) -> Response {
    let running = c.ladder.active.load(Ordering::SeqCst);
    let p = c
        .ladder
        .progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // the marker with nothing running: the core restarted during a run
    let unfinished = !running && c.path(RUN_DIR).join(STARTED).exists();
    reply(
        200,
        json!({"ok": true, "running": running, "unfinished": unfinished, "progress": p}),
    )
}
/// GET /api/ladder/result: the latest, or `?id=`.
pub async fn result(State(c): State<Compat>, Query(q): Query<HashMap<String, String>>) -> Response {
    migrate(&c);
    let list = results(&c.path(DIR));
    let path = match q.get("id") {
        Some(id) => list.iter().find(|(n, _)| n.to_string() == *id),
        None => list.last(),
    };
    let body = path.and_then(|(_, p)| std::fs::read(p).ok());
    match body.and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        Some(mut v) => {
            let chart = screen_chart(&v["report"]);
            v["chart"] = chart;
            v["prev"] = previous(&list, &v);
            reply(200, json!({"ok": true, "result": v}))
        }
        None => reply(404, json!({"ok": false, "error": "no_result"})),
    }
}
/// The whole run in `SCREEN_POINTS` columns, for a screen too small for the raw probes: each
/// column's slowest probe (a lost one at the cap), and the rates over it in tenths of a Mbps;
/// `-1` where nothing was measured. `marks`: the columns where each step after the first begins;
/// `kinds`: what each step loaded (a result from before kinds had idle, down, both, recovery). A
/// result from before the steps kept their spans covers its probes, without marks or rates.
fn screen_chart(r: &Value) -> Value {
    let num = |v: &Value| v.as_u64().map(|n| n as u32);
    let samples: Vec<(u32, u32, u64)> = r["samples"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| Some((num(&s[0])?, num(&s[1])?, s[2].as_u64()?)))
        .filter(|s| s.2 <= 1)
        .collect();
    let steps = r["steps"].as_array().map(Vec::as_slice).unwrap_or_default();
    let span = |s: &Value| Some((num(&s["from_ms"])?, num(&s["to_ms"])?)).filter(|(a, b)| b > a);
    let spans: Vec<Option<(u32, u32)>> = steps.iter().map(span).collect();
    let from = spans
        .first()
        .copied()
        .flatten()
        .map(|(a, _)| a)
        .or_else(|| samples.iter().map(|s| s.0).min());
    let to = spans
        .iter()
        .flatten()
        .map(|(_, b)| *b)
        .max()
        .or_else(|| samples.iter().map(|s| s.0 + s.1).max());
    let (Some(from), Some(to)) = (from, to) else {
        return Value::Null;
    };
    if to <= from || samples.is_empty() {
        return Value::Null;
    }
    let n = SCREEN_POINTS as u32;
    let width = u64::from(to - from);
    let col = |t: u32| (u64::from(t.saturating_sub(from)) * u64::from(n) / width) as u32;
    let mut delay = vec![-1i64; SCREEN_POINTS];
    for &(sent, rtt, _) in samples.iter().filter(|s| (from..to).contains(&s.0)) {
        let c = &mut delay[col(sent).min(n - 1) as usize];
        *c = (*c).max(i64::from(rtt));
    }
    // a rate covers the second before it: each column takes the first one at or after its middle
    let rates: Vec<(u32, u32, u32)> = r["rates"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|x| Some((num(&x[0])?, num(&x[1])?, num(&x[2])?)))
        .collect();
    let (mut down, mut up) = (Vec::new(), Vec::new());
    if !rates.is_empty() {
        for i in 0..n {
            let mid = from + (u64::from(2 * i + 1) * width / (2 * u64::from(n))) as u32;
            let got = rates.iter().find(|(t, _, _)| *t >= mid && *t - mid <= 1500);
            let tenth = |kbps: u32| i64::from(kbps.div_ceil(100));
            down.push(got.map_or(-1, |g| tenth(g.1)));
            up.push(got.map_or(-1, |g| tenth(g.2)));
        }
    }
    let marks: Vec<u32> = spans.iter().skip(1).flatten().map(|(a, _)| col(*a).min(n)).collect();
    const OLD: [&str; 4] = ["idle", "down", "both", "recovery"];
    let kinds: Vec<&str> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| s["kind"].as_str().unwrap_or(OLD.get(i).copied().unwrap_or("")))
        .collect();
    json!({"points": n, "delay": delay, "down": down, "up": up, "marks": marks, "kinds": kinds})
}
/// One run in a history list: enough to choose two and compare them, without the probes.
fn summary(v: &Value) -> Value {
    let r = &v["report"];
    let steps: Vec<Value> = r["steps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            const OLD: [&str; 4] = ["idle", "down", "both", "recovery"];
            let kind = s["kind"].as_str().unwrap_or(OLD.get(i).copied().unwrap_or(""));
            json!({
                "kind": kind,
                "p50_ms": s["stats"]["p50_ms"],
                "p90_ms": s["stats"]["p90_ms"],
                "increase_ms": s["stats"]["increase_ms"],
                "down_mbps": s["down_mbps"],
                "up_mbps": s["up_mbps"],
            })
        })
        .collect();
    json!({
        "id": v["id"],
        "plan": r["plan"],
        "aborted": r["aborted"],
        "findings": r["findings"],
        "notes": r["notes"].as_array().map(|a| a.iter().map(|n| n["code"].clone()).collect::<Vec<_>>()),
        "steps": steps,
        "context": v["context"],
        "counter_bytes": v["counter_bytes"],
    })
}
/// The run before `v` that it can be compared with: same plan, same steps, finished with a
/// baseline. A
/// comparison across plans or step lists would set different loads side by side.
fn previous(list: &[(u64, PathBuf)], v: &Value) -> Value {
    let this = summary(v);
    let kinds = |s: &Value| s["steps"].as_array().map(|a| a.iter().map(|x| x["kind"].clone()).collect::<Vec<_>>());
    let id = v["id"].as_u64().unwrap_or(0);
    list.iter()
        .rev()
        .filter(|(n, _)| *n < id)
        .filter_map(|(_, p)| serde_json::from_slice::<Value>(&std::fs::read(p).ok()?).ok())
        .map(|o| summary(&o))
        // one that concluded nothing (no baseline) says nothing to compare with either
        .find(|o| {
            o["plan"] == this["plan"]
                && kinds(o) == kinds(&this)
                && o["aborted"].is_null()
                && o["findings"]["no_baseline"] != true
        })
        .unwrap_or(Value::Null)
}
/// GET /api/ladder/history: the latest runs, newest first, without their probes.
pub async fn history(State(c): State<Compat>) -> Response {
    migrate(&c);
    let list = results(&c.path(DIR));
    let runs: Vec<Value> = list
        .iter()
        .rev()
        .take(HISTORY)
        .filter_map(|(_, p)| serde_json::from_slice::<Value>(&std::fs::read(p).ok()?).ok())
        .map(|v| summary(&v))
        .collect();
    reply(200, json!({"ok": true, "runs": runs}))
}
/// POST /api/ladder/cancel: says stopping now; the run ends within 2 s.
pub async fn cancel(State(c): State<Compat>) -> Response {
    let running = c.ladder.active.load(Ordering::SeqCst);
    if running {
        c.ladder
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopping = true;
        if let Some(s) = c
            .ladder
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            s.stop();
        }
    }
    reply(200, json!({"ok": true, "running": running}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calm() -> Look {
        Look {
            cell: Some("1/1".into()),
            rat: Some("SA".into()),
            carriers: Some(2),
            connected: Some(true),
            roaming: Some(false),
            heat: Some(HeatLook { too_hot: false }),
            battery: Some((80, false)),
            counter: None,
        }
    }
    fn codes(e: &[Event]) -> Vec<(&'static str, EventKind)> {
        e.iter().map(|e| (e.code, e.kind)).collect()
    }
    fn after(change: impl Fn(&mut Look)) -> Vec<(&'static str, EventKind)> {
        let mut t = Tracker::default();
        let now = Instant::now();
        assert!(t.step(1, 0, &calm(), now).is_empty());
        let mut l = calm();
        change(&mut l);
        codes(&t.step(2, 0, &l, now))
    }
    #[test]
    fn the_event_table() {
        use EventKind::*;
        assert_eq!(after(|_| {}), vec![]);
        // the same PCI with another cell id is another cell
        assert_eq!(
            after(|l| l.cell = Some("1/2".into())),
            vec![("cell_changed", Void)]
        );
        assert_eq!(
            after(|l| l.rat = Some("LTE".into())),
            vec![("rat_changed", Void)]
        );
        assert_eq!(
            after(|l| l.carriers = Some(1)),
            vec![("carriers_changed", Note)]
        );
        assert_eq!(
            after(|l| l.connected = Some(false)),
            vec![("network_down", Abort)]
        );
        assert_eq!(
            after(|l| l.roaming = Some(true)),
            vec![("became_roaming", Abort)]
        );
        assert_eq!(
            after(|l| l.heat = Some(HeatLook { too_hot: true })),
            vec![("overheated", Abort)]
        );
        assert_eq!(
            after(|l| l.battery = Some((14, false))),
            vec![("low_battery", Abort)]
        );
        assert_eq!(after(|l| l.battery = Some((14, true))), vec![]);
        // a read that failed is not a change
        assert_eq!(
            after(|l| {
                l.cell = None;
                l.connected = None;
                l.roaming = None;
            }),
            vec![]
        );
    }
    #[test]
    fn a_run_admitted_while_roaming_is_not_ended_by_roaming() {
        let mut t = Tracker {
            roaming_ok: true,
            ..Tracker::default()
        };
        let l = Look {
            roaming: Some(true),
            ..calm()
        };
        assert!(t.step(1, 0, &l, Instant::now()).is_empty());
    }
    #[test]
    fn heat_unread_three_times_in_a_row_ends_the_run() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let blind = Look {
            heat: None,
            ..calm()
        };
        assert!(t.step(1, 0, &blind, now).is_empty());
        assert!(t.step(1, 0, &blind, now).is_empty());
        assert!(t.step(1, 0, &calm(), now).is_empty());
        assert!(t.step(1, 0, &blind, now).is_empty());
        assert!(t.step(1, 0, &blind, now).is_empty());
        assert_eq!(
            codes(&t.step(1, 0, &blind, now)),
            vec![("heat_unknown", EventKind::Abort)]
        );
    }
    fn other(
        step: u8,
        calibrating: bool,
        counter_mb: u64,
        app_mb: u64,
    ) -> Vec<(&'static str, EventKind)> {
        let mut t = Tracker {
            calibrating,
            ..Tracker::default()
        };
        let t0 = Instant::now();
        let at = |c| Look {
            counter: Some(c),
            ..calm()
        };
        assert!(t.step(step, 0, &at(9_000_000), t0).is_empty()); // not settled: not the base
        assert!(t
            .step(step, 0, &at(0), t0 + Duration::from_secs(2))
            .is_empty());
        let late = Look {
            counter: None,
            ..calm()
        };
        let app = app_mb * 1_000_000;
        assert!(t
            .step(step, app, &late, t0 + Duration::from_secs(5))
            .is_empty());
        codes(&t.step(
            step,
            app,
            &at(counter_mb * 1_000_000),
            t0 + Duration::from_secs(6),
        ))
    }
    #[test]
    fn other_traffic_is_a_note_under_load_and_voids_the_baseline_at_idle() {
        use EventKind::*;
        assert_eq!(other(2, false, 12, 10), vec![]); // 4 Mbps beside 20: within the share
        assert_eq!(other(2, false, 20, 10), vec![("other_traffic", Note)]);
        assert_eq!(other(1, false, 2, 0), vec![("other_traffic", Void)]);
        assert_eq!(other(1, true, 2, 0), vec![]);
    }
    #[test]
    fn the_screen_chart_takes_each_columns_slowest_probe_and_the_rate_over_it() {
        let r = json!({
            "samples": [[0, 40, 0], [100, 60, 0], [500, 2000, 1], [600, 0, 2], [960, 30, 0]],
            "steps": [{"from_ms": 0, "to_ms": 250}, {"from_ms": 250, "to_ms": 500},
                {"from_ms": 500, "to_ms": 750}, {"from_ms": 750, "to_ms": 960}],
            "rates": [[400, 24_960, 0], [1400, 0, 0]],
        });
        let c = screen_chart(&r);
        let delay = c["delay"].as_array().unwrap();
        assert_eq!(delay.len(), SCREEN_POINTS);
        assert_eq!(delay[0], 40);
        assert_eq!(delay[10], 60);
        assert_eq!(delay[50], 2000); // lost: at the cap; the failed probe is left out
        assert_eq!(delay[62], -1);
        assert_eq!(c["marks"], json!([25, 50, 75]));
        assert_eq!(c["kinds"], json!(["idle", "down", "both", "recovery"]));
        assert_eq!(c["down"][0], 250);
        assert_eq!(c["down"][60], 0);
        // a result from before spans and rates: probes only
        let old = screen_chart(&json!({"samples": [[0, 40, 0], [1000, 50, 0]], "steps": [{}]}));
        assert_eq!((old["marks"].clone(), old["down"].clone()), (json!([]), json!([])));
        assert_eq!(screen_chart(&json!({})), Value::Null);
    }
    #[test]
    fn a_redone_idle_step_is_judged_afresh() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |c: Option<u64>| Look {
            counter: c,
            ..calm()
        };
        let sec = |n| t0 + Duration::from_secs(n);
        let busy = vec![("other_traffic", EventKind::Void)];
        t.step(1, 0, &at(Some(0)), sec(0));
        t.step(1, 0, &at(Some(0)), sec(2)); // the base
        assert_eq!(codes(&t.step(1, 0, &at(Some(2_000_000)), sec(4))), busy);
        t.redo_idle();
        // quiet from here: the burst before the redo is not held against it
        t.step(1, 0, &at(Some(2_000_000)), sec(5));
        t.step(1, 0, &at(Some(2_000_000)), sec(7)); // the new base
        assert!(codes(&t.step(1, 0, &at(Some(2_100_000)), sec(11))).is_empty());
        // busy again: reported again
        assert_eq!(codes(&t.step(1, 0, &at(Some(6_000_000)), sec(13))), busy);
    }
    #[test]
    fn bytes_the_counter_has_not_shown_yet_are_not_other_traffic() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |c: Option<u64>| Look {
            counter: c,
            ..calm()
        };
        let sec = |n| t0 + Duration::from_secs(n);
        t.step(2, 0, &at(None), sec(0));
        t.step(2, 0, &at(Some(0)), sec(2));
        t.step(2, 2_000_000, &at(None), sec(3));
        // the load stopped at 3 s; the counter shows its last second only now
        assert!(codes(&t.step(4, 2_000_000, &at(Some(2_000_000)), sec(4))).is_empty());
    }
    #[test]
    fn a_load_steps_first_seconds_and_single_windows_do_not_decide() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |c: Option<u64>| Look {
            counter: c,
            ..calm()
        };
        let sec = |n| t0 + Duration::from_secs(n);
        // 3 MB lands in the first second of step 2: a receive buffer filling
        t.step(2, 0, &at(Some(0)), sec(0));
        t.step(2, 400_000, &at(Some(3_000_000)), sec(1));
        t.step(2, 3_400_000, &at(Some(3_400_000)), sec(2)); // the base
        t.step(2, 3_800_000, &at(None), sec(3));
        // 1.1 MB beside 0.4 in the next 2 s (4.4 Mbps over): the step so far is what counts
        let mut got = codes(&t.step(2, 4_200_000, &at(Some(4_500_000)), sec(4)));
        got.extend(codes(&t.step(2, 4_600_000, &at(None), sec(5))));
        got.extend(codes(&t.step(2, 5_000_000, &at(Some(5_000_000)), sec(10))));
        assert!(got.is_empty(), "{got:?}");
    }
    #[test]
    fn battery_is_read_from_the_blocks_real_fields() {
        let block = crate::contract::value(&crate::contract::Battery::default());
        assert_eq!(battery_of(&block), Some((0, false)));
    }
    #[test]
    fn data_call_states() {
        for up in ["connected", "ipv4_connected", "ipv4_ipv6_connected"] {
            assert!(data_call_up(up), "{up}");
        }
        for down in [
            "disconnected",
            "ipv4_disconnected",
            "connecting",
            "init",
            "",
        ] {
            assert!(!data_call_up(down), "{down}");
        }
    }
    #[test]
    fn device_refusals() {
        assert_eq!(refuse_for_device(Some(false), Some((80, false))), None);
        assert_eq!(
            refuse_for_device(Some(true), Some((80, false))),
            Some("too_warm")
        );
        assert_eq!(
            refuse_for_device(None, Some((80, false))),
            Some("heat_unknown")
        );
        assert_eq!(
            refuse_for_device(Some(false), Some((19, false))),
            Some("low_battery")
        );
        assert_eq!(refuse_for_device(Some(false), Some((19, true))), None);
    }
    #[test]
    fn storage_drops_samples_first_and_keeps_thirty() {
        let dir = std::env::temp_dir().join(format!("ladder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let big = Report {
            samples: vec![(1, 2, 0); 20_000],
            ..Report::default()
        };
        let ctx = json!({"rat": "SA", "band": "n78"});
        let bytes = encode(1, big, Some(5), &ctx);
        assert!(bytes.len() < MAX_BYTES);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["counter_bytes"], 5);
        assert_eq!(v["context"]["band"], "n78");
        assert_eq!(v["report"]["samples"].as_array().unwrap().len(), 0);
        let small = Report {
            samples: vec![(1, 2, 0); 100],
            ..Report::default()
        };
        let v: Value = serde_json::from_slice(&encode(2, small, None, &ctx)).unwrap();
        assert_eq!(v["report"]["samples"].as_array().unwrap().len(), 100);
        for id in 0..35 {
            save(&dir, id, b"{}");
        }
        let list = results(&dir);
        assert_eq!(list.len(), KEEP);
        assert_eq!(list[0].0, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_run_is_compared_with_the_last_one_like_it() {
        let dir = std::env::temp_dir().join(format!("ladder-prev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let run = |id: u64, plan: &str, n: usize, aborted: Value| {
            let steps: Vec<Value> = (0..n).map(|_| json!({"kind": "idle", "stats": {}})).collect();
            json!({"id": id, "report": {"plan": plan, "aborted": aborted, "steps": steps}})
        };
        let mut unbased = run(5, "home", 5, Value::Null);
        unbased["report"]["findings"] = json!({"no_baseline": true});
        for v in [run(1, "home", 5, Value::Null), run(2, "roaming", 5, Value::Null),
            run(3, "home", 4, Value::Null), run(4, "home", 5, json!("stopped")), unbased] {
            save(&dir, v["id"].as_u64().unwrap(), v.to_string().as_bytes());
        }
        let list = results(&dir);
        assert_eq!(previous(&list, &run(6, "home", 5, Value::Null))["id"], 1);
        assert!(previous(&list, &run(1, "home", 5, Value::Null)).is_null());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn a_history_entry_has_each_steps_kind_and_no_probes() {
        // a result from before kinds: its four steps are named by place
        let v = json!({"id": 7, "counter_bytes": 9, "report": {"plan": "home", "aborted": null,
            "findings": {"first_slow_step": 3}, "notes": [{"code": "idle_redone", "kind": "note"}],
            "samples": [[0, 40, 0]],
            "steps": [{"stats": {"p50_ms": 40.0}}, {"stats": {"p50_ms": 45.0}, "down_mbps": 25.0},
                {"stats": {"p50_ms": 190.0}}, {"stats": {"p50_ms": 41.0}}]}});
        let s = summary(&v);
        let kinds: Vec<&str> = s["steps"].as_array().unwrap().iter().map(|x| x["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["idle", "down", "both", "recovery"]);
        assert_eq!(s["steps"][1]["down_mbps"], 25.0);
        assert_eq!(s["notes"], json!(["idle_redone"]));
        assert!(s.get("samples").is_none() && s["context"].is_null());
    }
}
