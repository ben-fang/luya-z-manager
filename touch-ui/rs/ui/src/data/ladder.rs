//! 这里撑得住吗: the load ladder on luyaz-core (/api/ladder/*). Read every second while its page
//! is up; the start goes out on its own thread because the core looks at other traffic first.
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

use super::core_api::{CoreApi, Reply};
use super::util::{jstr, mono_ms};
use crate::json;
use std::sync::{Arc, Mutex};
use ui_model::{ladder_chart_t, ladder_result_t, ladder_step_t, ladder_t};

const TTL_MS: i64 = 1000;
const CURVE_MAX: usize = 120;
const RESULT_MAX: usize = 70_000;
const CHART_MAX: usize = 4096;
const START_IO_MS: i32 = 12_000;

#[derive(Clone, Default)]
pub struct Ladder {
    m: ladder_t,
    last_ms: i64,
    seen_running: bool,
    /// bytes and clock at the last read, for the rates
    last: Option<(i64, i64, i64)>,
    want_history: bool,
    want_id: Option<i64>,
    reply: Arc<Mutex<Option<Reply>>>,
}

fn opt_f64(j: &[u8], key: &str) -> Option<f64> {
    match json::get(j, key, 32) {
        None => None,
        Some(v) if v == b"null" => None,
        Some(v) => Some(json::atof(&v)),
    }
}
fn flag(j: &[u8], key: &str) -> bool {
    json::get(j, key, 8).is_some_and(|v| v == b"true")
}
fn ints(j: &[u8], key: &str) -> Vec<i32> {
    ints_up_to(j, key, 64)
}
fn ints_up_to(j: &[u8], key: &str, cap: usize) -> Vec<i32> {
    let Some(a) = json::get(j, key, cap) else { return Vec::new() };
    let (mut out, mut p) = (Vec::new(), 0);
    while let Some((e, next)) = json::arr_next(&a, p, 16) {
        out.push(json::atol(&e) as i32);
        p = next;
    }
    out
}

/// An array of short strings.
fn strs(j: &[u8], key: &str) -> Vec<String> {
    let Some(a) = json::get(j, key, 256) else { return Vec::new() };
    let (mut out, mut p) = (Vec::new(), 0);
    while let Some((e, next)) = json::arr_next(&a, p, 24) {
        out.push(String::from_utf8_lossy(e.strip_prefix(b"\"").and_then(|e| e.strip_suffix(b"\"")).unwrap_or(&e)).into_owned());
        p = next;
    }
    out
}

/// One run of `/api/ladder/history` (or a result's `prev`): steps carry their P50 at the top.
fn parse_summary(r: &[u8]) -> Option<ladder_result_t> {
    let f = json::get(r, "findings", 2048).unwrap_or_default();
    let steps = json::get(r, "steps", 4096).unwrap_or_default();
    let ctx = json::get(r, "context", 1024).unwrap_or_default();
    Some(ladder_result_t {
        id: json::get_int(r, "id", 0),
        plan: jstr(r, "plan", 16),
        aborted: json::get_str(r, "aborted", 32).filter(|a| a != "null"),
        no_baseline: flag(&f, "no_baseline"),
        first_slow_step: opt_f64(&f, "first_slow_step").unwrap_or(0.0) as i32,
        recovered: json::get(&f, "recovered", 8).and_then(|v| match v.as_slice() {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }),
        recovery_ms: opt_f64(&f, "recovery_ms").map(|v| v as i64),
        queued_steps: ints(&f, "queued_steps"),
        unloaded_steps: ints(&f, "unloaded_steps"),
        unresponsive_steps: ints(&f, "unresponsive_steps"),
        slow_steps: ints(&f, "slow_steps"),
        steps: json::objects(&steps)
            .map(|s| ladder_step_t {
                kind: jstr(s, "kind", 16),
                p50_ms: opt_f64(s, "p50_ms"),
                down_mbps: opt_f64(s, "down_mbps"),
                up_mbps: opt_f64(s, "up_mbps"),
                ..Default::default()
            })
            .collect(),
        counter_bytes: opt_f64(r, "counter_bytes").map(|v| v as i64),
        notes: strs(r, "notes"),
        band: jstr(&ctx, "band", 16),
        pci: opt_f64(&ctx, "pci").map(|v| v as i64),
        ..Default::default()
    })
}
/// The body of `GET /api/ladder/history`.
pub fn parse_history(body: &[u8]) -> Vec<ladder_result_t> {
    let runs = json::get(body, "runs", RESULT_MAX).unwrap_or_default();
    json::objects(&runs).filter_map(parse_summary).collect()
}

/// The body of `GET /api/ladder/result`.
pub fn parse_result(body: &[u8]) -> Option<ladder_result_t> {
    let wrap = json::get(body, "result", RESULT_MAX)?;
    let r = json::get(&wrap, "report", RESULT_MAX)?;
    let f = json::get(&r, "findings", 2048)?;
    let steps = json::get(&r, "steps", 8192).unwrap_or_default();
    let notes = json::get(&r, "notes", 4096).unwrap_or_default();
    Some(ladder_result_t {
        id: json::get_int(&wrap, "id", 0),
        plan: jstr(&r, "plan", 16),
        aborted: json::get_str(&r, "aborted", 32).filter(|a| a != "null"),
        no_baseline: flag(&f, "no_baseline"),
        idle_jitter: flag(&f, "idle_jitter"),
        first_slow_step: opt_f64(&f, "first_slow_step").unwrap_or(0.0) as i32,
        recovered: json::get(&f, "recovered", 8).and_then(|v| match v.as_slice() {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }),
        recovery_ms: opt_f64(&f, "recovery_ms").map(|v| v as i64),
        queued_steps: ints(&f, "queued_steps"),
        unloaded_steps: ints(&f, "unloaded_steps"),
        unresponsive_steps: ints(&f, "unresponsive_steps"),
        slow_steps: ints(&f, "slow_steps"),
        steps: json::objects(&steps)
            .enumerate()
            .map(|(i, s)| {
                let stats = json::get(s, "stats", 512).unwrap_or_default();
                const OLD: [&str; 4] = ["idle", "down", "both", "recovery"];
                let kind = json::get_str(s, "kind", 16).unwrap_or_else(|| OLD.get(i).copied().unwrap_or("").into());
                ladder_step_t {
                    kind,
                    p50_ms: opt_f64(&stats, "p50_ms"),
                    down_mbps: opt_f64(s, "down_mbps"),
                    up_mbps: opt_f64(s, "up_mbps"),
                    target_down_mbps: opt_f64(s, "target_down_mbps"),
                    target_up_mbps: opt_f64(s, "target_up_mbps"),
                    short_of_target: flag(s, "short_of_target"),
                    failed: flag(s, "failed"),
                    probes: json::get_int(&stats, "probes", 0) as i32,
                    timeouts: json::get_int(&stats, "timeouts", 0) as i32,
                }
            })
            .collect(),
        counter_bytes: opt_f64(&wrap, "counter_bytes").map(|v| v as i64),
        notes: json::objects(&notes).map(|n| jstr(n, "code", 24)).collect(),
        chart: json::get(&wrap, "chart", CHART_MAX).filter(|c| c.first() == Some(&b'{')).map(|c| ladder_chart_t {
            delay: ints_up_to(&c, "delay", CHART_MAX),
            down: ints_up_to(&c, "down", CHART_MAX),
            up: ints_up_to(&c, "up", CHART_MAX),
            marks: ints(&c, "marks"),
            kinds: strs(&c, "kinds"),
        }),
        band: json::get(&wrap, "context", 1024).map_or(String::new(), |c| jstr(&c, "band", 16)),
        pci: json::get(&wrap, "context", 1024).and_then(|c| opt_f64(&c, "pci")).map(|v| v as i64),
        prev: json::get(&wrap, "prev", 8192).filter(|p| p.first() == Some(&b'{')).and_then(|p| parse_summary(&p)).map(Box::new),
    })
}

impl Ladder {
    pub fn model(&self) -> &ladder_t {
        &self.m
    }
    pub fn start(&mut self, mut agent: CoreApi, busy_ok: bool, roaming_ok: bool) {
        if self.m.starting || self.m.running {
            return;
        }
        self.m.starting = true;
        self.m.start_error.clear();
        self.m.show_result = false;
        let reply = self.reply.clone();
        let body = format!("{{\"busy_ok\":{busy_ok},\"roaming_ok\":{roaming_ok}}}");
        let sent = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
            let r = agent.api_ms("POST", "/api/ladder/start", Some(&body), 1024, START_IO_MS);
            *reply.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        });
        if sent.is_err() {
            self.m.starting = false;
            self.m.start_error = "spawn_failed".into();
        }
    }
    pub fn stop(&mut self, mut agent: CoreApi) {
        self.m.stopping = true;
        let _ = std::thread::Builder::new().stack_size(128 * 1024).spawn(move || {
            agent.api_ms("POST", "/api/ladder/cancel", Some("{}"), 1024, 3000);
        });
    }
    pub fn dismiss(&mut self) {
        self.m.show_result = false;
        self.m.start_error.clear();
        // a kept run closes back to the list
        if self.m.viewing {
            self.m.viewing = false;
            self.m.browsing = true;
        } else {
            self.m.browsing = false;
        }
    }
    /// Open (or close) the kept-runs list; the next poll reads it.
    pub fn browse(&mut self, open: bool) {
        if self.m.viewing {
            self.m.show_result = false;
        }
        self.m.browsing = open;
        self.m.viewing = false;
        self.want_history = open;
        self.last_ms = 0;
    }
    /// Open kept run `i` of the list; the next poll reads it.
    pub fn open(&mut self, i: usize) {
        if let Some(r) = self.m.history.get(i) {
            self.want_id = Some(r.id);
            self.last_ms = 0;
        }
    }
    /// The reads when active and a second has passed. true = a read ran.
    pub fn poll(&mut self, agent: &mut CoreApi, active: bool) -> bool {
        let answered = self.reply.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((code, body)) = answered {
            self.m.starting = false;
            if code == 200 {
                self.begin();
            } else {
                let b = body.unwrap_or_default();
                let e = jstr(&b, "error", 32);
                self.m.start_error = if e.is_empty() { "unreachable".into() } else { e };
            }
            self.last_ms = 0;
        }
        let now = mono_ms();
        if !active || now - self.last_ms < TTL_MS {
            return answered_any(self.m.starting);
        }
        self.last_ms = now;
        if std::mem::take(&mut self.want_history) {
            let (c, r) = agent.api_ms("GET", "/api/ladder/history", None, RESULT_MAX, 3000);
            self.m.history = r.filter(|_| c == 200).map(|b| parse_history(&b)).unwrap_or_default();
        }
        if let Some(id) = self.want_id.take() {
            let (c, r) = agent.api_ms("GET", &format!("/api/ladder/result?id={id}"), None, RESULT_MAX, 3000);
            if let Some(res) = r.filter(|_| c == 200).and_then(|b| parse_result(&b)) {
                self.m.result = Some(res);
                self.m.show_result = true;
                self.m.viewing = true;
                self.m.browsing = false;
            }
        }
        let (code, body) = agent.api("GET", "/api/ladder/progress", None, 2048);
        let Some(b) = body.filter(|_| code == 200) else { return true };
        self.progress(&b, now);
        if self.seen_running && !self.m.running {
            self.seen_running = false;
            let (c, r) = agent.api_ms("GET", "/api/ladder/result", None, RESULT_MAX, 3000);
            self.m.result = r.filter(|_| c == 200).and_then(|b| parse_result(&b));
            self.m.show_result = true;
            (self.m.viewing, self.m.browsing) = (false, false);
        }
        true
    }
    fn begin(&mut self) {
        self.m.running = true;
        self.m.stopping = false;
        self.m.step = 0;
        self.m.now_ms = None;
        self.m.down_mbps = 0.0;
        self.m.up_mbps = 0.0;
        self.m.used_bytes = 0;
        self.m.curve.clear();
        self.m.result = None;
        self.seen_running = true;
        self.last = None;
    }
    fn progress(&mut self, b: &[u8], now: i64) {
        let running = flag(b, "running");
        if running && !self.seen_running {
            self.begin(); // started elsewhere, or the page opened during a run
        }
        self.m.running = running;
        self.m.unfinished = flag(b, "unfinished");
        let Some(p) = json::get(b, "progress", 1024) else { return };
        if !running {
            self.m.stopping = false;
            return;
        }
        self.m.stopping |= flag(&p, "stopping");
        self.m.step = json::get_int(&p, "step", 0) as i32;
        self.m.kind = jstr(&p, "kind", 16);
        let (down, up) = (json::get_int(&p, "down_bytes", 0), json::get_int(&p, "up_bytes", 0));
        self.m.used_bytes = down + up;
        if let Some((d0, u0, t0)) = self.last.filter(|l| now > l.2) {
            let per = 8.0 / ((now - t0) as f64 / 1000.0) / 1e6;
            self.m.down_mbps = (down - d0).max(0) as f64 * per;
            self.m.up_mbps = (up - u0).max(0) as f64 * per;
        }
        self.last = Some((down, up, now));
        let ms = opt_f64(&p, "last_window_ms").map(|v| v as i32);
        self.m.now_ms = ms;
        if let Some(v) = ms {
            if self.m.curve.len() == CURVE_MAX {
                self.m.curve.remove(0);
            }
            self.m.curve.push(v);
        }
    }
}
/// Waiting for the start's answer: every round is worth a look.
fn answered_any(starting: bool) -> bool {
    starting
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_result_parses() {
        let body = br#"{"ok":true,"result":{"id":7,"counter_bytes":152000000,"report":{"plan":"home","aborted":null,
"findings":{"first_slow_step":3,"idle_jitter":false,"no_baseline":false,"recovered":true,"recovery_ms":500,"queued_steps":[3],"unloaded_steps":[]},
"notes":[{"kind":"note","code":"other_traffic"}],
"steps":[{"stats":{"p50_ms":46.0},"target_down_mbps":null,"down_mbps":null,"short_of_target":false,"failed":false},
{"stats":{"p50_ms":47.0},"target_down_mbps":25.0,"down_mbps":25.0,"short_of_target":false,"failed":false},
{"stats":{"p50_ms":194.0},"target_down_mbps":25.0,"down_mbps":25.0,"target_up_mbps":30.0,"up_mbps":11.4,"short_of_target":true,"failed":false},
{"stats":{"p50_ms":47.0}}],"samples":[[1,2,0]]}}}"#;
        let r = parse_result(body).unwrap();
        assert_eq!((r.id, r.first_slow_step, r.aborted.clone()), (7, 3, None));
        assert_eq!(r.queued_steps, vec![3]);
        assert_eq!(r.recovery_ms, Some(500));
        assert_eq!(r.steps.len(), 4);
        assert_eq!(r.steps[2].up_mbps, Some(11.4));
        assert_eq!(r.notes, vec!["other_traffic".to_string()]);
        // a result from an older core has no chart
        assert_eq!(r.chart, None);
    }
    #[test]
    fn the_cores_chart_and_timeouts_parse() {
        let delay = (0..96).map(|i| if i == 5 { "-1".to_string() } else { (40 + i).to_string() }).collect::<Vec<_>>().join(",");
        let body = format!(
            r#"{{"result":{{"id":2,"report":{{"plan":"home","findings":{{"queued_steps":[]}},
"steps":[{{"stats":{{"p50_ms":46.0,"p95_ms":50.0,"probes":80,"timeouts":6,"jitter_ms":3.5}}}}]}},
"chart":{{"points":96,"delay":[{delay}],"down":[250,-1],"up":[0,114],"marks":[17,43,78]}}}}}}"#
        );
        let r = parse_result(body.as_bytes()).unwrap();
        assert_eq!((r.steps[0].probes, r.steps[0].timeouts), (80, 6));
        let c = r.chart.unwrap();
        assert_eq!((c.delay.len(), c.delay[5], c.delay[95]), (96, -1, 135));
        assert_eq!((c.down, c.up, c.marks), (vec![250, -1], vec![0, 114], vec![17, 43, 78]));
    }
    #[test]
    fn an_aborted_run_keeps_why() {
        let body = br#"{"result":{"id":1,"counter_bytes":null,"report":{"plan":"roaming","aborted":"overheated","findings":{"recovered":null,"queued_steps":[],"unloaded_steps":[]},"steps":[]}}}"#;
        let r = parse_result(body).unwrap();
        assert_eq!(r.aborted.as_deref(), Some("overheated"));
        assert_eq!(r.recovered, None);
        assert_eq!(r.counter_bytes, None);
    }
}
