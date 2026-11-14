//! 这里撑得住吗 (docs/designs/load-ladder.md): the cost, the ladder as it runs, what it found.
//! Facts about this moment only: nothing here says a call or a meeting will work.
// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0

use super::Ui;
use crate::cf;
use crate::kit::{self, BtnKind, CARD_W, MARGIN, PAD};
use crate::lang::tr;
use crate::logic::Arm;
use crate::lv::{self, sys, Obj, Series};
use crate::theme::{t, uf};
use ui_model::{ladder_chart_t, ladder_result_t, ladder_t};

const ROWS: usize = 5;
const ROW_STEP: i32 = 28;
const BTN_H: i32 = 42;
const CHART_H: i32 = 90;
const CHART_POINTS: u32 = 60;
/// the chart's top: above this the line stays at the top
const CHART_MS: i32 = 1000;
const ARM_MS: u32 = 5000;
const ARM_START: i32 = 0;
const ARM_FULL: i32 = 1;
const TW: i32 = CARD_W - 2 * PAD;
/// the result's charts: the whole run in the core's columns
const RES_POINTS: u32 = 96;
const RES_H: i32 = 64;
const RATE_H: i32 = 40;
/// kept runs listed on the screen (the core keeps more; the web page lists them all)
const HIST_ROWS: usize = 8;
const HIST_H: i32 = 52;

#[derive(Default)]
pub struct Ladder {
    scroll: Obj,
    card: Obj,
    title: Obj,
    cost: Obj,
    chart_name: Obj,
    chart: Obj,
    series: Series,
    /// what the chart shows now
    drawn: Vec<i32>,
    res_name: Obj,
    res_chart: Obj,
    res_delay: Series,
    /// where steps 2, 3 and 4 begin
    res_ticks: Vec<Obj>,
    res_steps: Vec<Obj>,
    rate_name: Obj,
    rate_down_l: Obj,
    rate_up_l: Obj,
    rate_chart: Obj,
    rate_down: Series,
    rate_up: Series,
    res_drawn: Option<ladder_chart_t>,
    key: Vec<Obj>,
    val: Vec<Obj>,
    note: Obj,
    fixed: Obj,
    btn: Obj,
    btn_l: Obj,
    btn2: Obj,
    btn2_l: Obj,
    /// the start screen's way to the kept runs
    btn3: Obj,
    btn3_l: Obj,
    /// the kept-runs list: a row, its time and plan, its headline
    hist: Vec<(Obj, Obj, Obj)>,
    /// under a result: against the last run like it
    cmp: Obj,
    arm: Arm,
    /// other traffic was seen and the next start goes on anyway
    busy_ok: bool,
    /// the layout set last: a refresh that changes nothing moves nothing
    laid: Vec<u8>,
    kinds: (Option<u8>, Option<u8>),
}

unsafe extern "C" fn btn_cb(_e: *mut sys::lv_event_t) {
    super::with(|u| u.ladder_btn());
}
unsafe extern "C" fn btn2_cb(_e: *mut sys::lv_event_t) {
    super::with(|u| u.ladder_btn2());
}
unsafe extern "C" fn btn3_cb(_e: *mut sys::lv_event_t) {
    super::with(|u| {
        u.src.ladder_browse(true);
        u.sync();
    });
}
unsafe extern "C" fn hist_cb(e: *mut sys::lv_event_t) {
    // SAFETY: LVGL's click event on a row
    let i = unsafe { lv::event_user(e) };
    super::with(|u| {
        u.src.ladder_open(i);
        u.sync();
    });
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Start,
    Run,
    Result,
    History,
}
fn view_of(m: &ladder_t) -> View {
    if m.running {
        View::Run
    } else if m.show_result {
        View::Result
    } else if m.browsing {
        View::History
    } else {
        View::Start
    }
}

/// "MM-DD HH:MM" of a device-clock id (local wall time labelled UTC).
fn when(id: i64) -> Vec<u8> {
    let (days, rem) = (id.div_euclid(86_400), id.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    cf!("%02d-%02d %02d:%02d", m as i32, d as i32, (rem / 3600) as i32, (rem / 60 % 60) as i32)
}

/// Against the last run like this one: each step's P50 then → now, and a cell change named.
fn comparison(r: &ladder_result_t) -> Vec<u8> {
    let Some(p) = r.prev.as_deref() else { return Vec::new() };
    let mut out = cf!(tr("和 %s 那次比："), when(p.id).as_slice());
    for i in 0..r.steps.len().min(p.steps.len()) {
        let k = kind_of(r, i as i32 + 1);
        let line = if k == "recovery" {
            match (p.recovery_ms, r.recovery_ms) {
                (Some(a), Some(b)) => cf!(tr("%s %.1f → %.1f 秒"), row_name(k).as_bytes(), a as f64 / 1000.0, b as f64 / 1000.0),
                _ => continue,
            }
        } else {
            match (p.steps[i].p50_ms, r.steps[i].p50_ms) {
                (Some(a), Some(b)) => cf!(tr("%s %.0f → %.0f ms"), row_name(k).as_bytes(), a, b),
                _ => continue,
            }
        };
        out.push(b'\n');
        out.extend(line);
    }
    if !p.band.is_empty() && (p.band != r.band || p.pci != r.pci) {
        out.push(b'\n');
        out.extend_from_slice(tr("两次不在同一个小区，差别可能来自小区").as_bytes());
    }
    out
}

/// The load that reached least of its target in a step, with its name: for the words about it.
fn short_load(r: &ladder_result_t, step: i32) -> Option<(bool, f64)> {
    let s = r.steps.get(usize::try_from(step - 1).ok()?)?;
    let share = |got: Option<f64>, target: Option<f64>| Some(got.unwrap_or(0.0) / target?);
    match (share(s.up_mbps, s.target_up_mbps), share(s.down_mbps, s.target_down_mbps)) {
        (Some(u), Some(d)) if d < u => Some((false, s.down_mbps.unwrap_or(0.0))),
        (Some(_), _) => Some((true, s.up_mbps.unwrap_or(0.0))),
        (None, Some(_)) => Some((false, s.down_mbps.unwrap_or(0.0))),
        _ => None,
    }
}

/// What step `n` (1-based) loaded; "" when there is no such step. A step without its kind is
/// from before kinds: idle, down, both, recovery by its place.
fn kind_of(r: &ladder_result_t, n: i32) -> &str {
    const OLD: [&str; 4] = ["idle", "down", "both", "recovery"];
    let Some(i) = usize::try_from(n - 1).ok().filter(|i| *i < r.steps.len()) else { return "" };
    match r.steps[i].kind.as_str() {
        "" => OLD.get(i).copied().unwrap_or(""),
        k => k,
    }
}

/// The headline: what this run found, about this moment.
pub(crate) fn headline(r: &ladder_result_t) -> Vec<u8> {
    if let Some(a) = &r.aborted {
        return match a.as_str() {
            "stopped" => tr("已停止，这次不算数"),
            "network_down" => tr("网络中断，这次不算数"),
            "overheated" => tr("设备过热，已停止"),
            "heat_unknown" => tr("读不到温度，已停止"),
            "low_battery" => tr("电量太低，已停止"),
            "became_roaming" => tr("开始漫游了，已停止"),
            _ => tr("这次没跑完，不算数"),
        }
        .into();
    }
    // the plainest fact first: it needs no baseline
    if let Some(&n) = r.unresponsive_steps.first() {
        return match kind_of(r, n) {
            "down" => tr("此刻一看影片，\n网络就没反应了"),
            "up" => tr("此刻只上传，\n网络就没反应了"),
            _ => tr("此刻一加上传，\n网络就没反应了"),
        }
        .into();
    }
    if r.no_baseline {
        // the usual cause on the move: say it plainly
        if r.notes.iter().any(|n| n == "cell_changed" || n == "rat_changed") {
            return tr("测试时换了基站，\n这次不下结论").into();
        }
        if r.notes.iter().any(|n| n == "idle_busy") {
            return tr("空闲时一直有别的流量，\n这次不下结论").into();
        }
        return tr("空闲基准没量好，这次不下结论").into();
    }
    if r.first_slow_step > 0 {
        if r.queued_steps.contains(&r.first_slow_step)
            && let Some((up, mbps)) = short_load(r, r.first_slow_step)
        {
            let f = if up { tr("此刻上传跑到 %.0f Mbps\n就开始排队") } else { tr("此刻下载跑到 %.0f Mbps\n就开始排队") };
            return cf!(f, mbps);
        }
        return match kind_of(r, r.first_slow_step) {
            "down" => tr("此刻一看影片，反应就变慢"),
            "up" => tr("此刻只上传，反应就变慢"),
            _ => tr("此刻一加上传，反应就变慢"),
        }
        .into();
    }
    if let Some(&n) = r.unloaded_steps.first()
        && let Some((up, mbps)) = short_load(r, n)
    {
        let f = if up { tr("此刻上传只跑到 %.0f Mbps\n这一步不下结论") } else { tr("此刻下载只跑到 %.0f Mbps\n这一步不下结论") };
        return cf!(f, mbps);
    }
    if r.steps.iter().any(|s| s.failed) {
        return tr("有一步没跑成，不下结论").into();
    }
    let loads = (1..=r.steps.len() as i32).filter(|n| matches!(kind_of(r, *n), "down" | "both" | "up")).count();
    if loads > 2 { tr("此刻这三档负载下，反应都没变慢") } else { tr("此刻这两档负载下，反应没变慢") }.into()
}

/// The line under the rows: rates short of target, events, what it used.
fn facts(r: &ladder_result_t) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut add = |line: Vec<u8>| {
        if !out.is_empty() {
            out.push(b'\n');
        }
        out.extend(line);
    };
    if r.idle_jitter {
        add(tr("这里空闲时就抖").into());
    }
    // with both directions slower: is it the upload by itself, or the two together?
    let step_of = |k: &str| (1..=r.steps.len() as i32).find(|n| kind_of(r, *n) == k);
    if let (Some(both), Some(up)) = (step_of("both"), step_of("up"))
        && r.slow_steps.contains(&both)
        && !r.unresponsive_steps.contains(&up)
        && !r.unloaded_steps.contains(&up)
        && r.steps.get(up as usize - 1).is_some_and(|s| s.p50_ms.is_some())
    {
        add(if r.slow_steps.contains(&up) { tr("只上传也变慢：是上传本身在排队") } else { tr("只上传时没变慢：是上下一起才排队") }.into());
    }
    // probes 2 s unanswered: not packet loss, which TCP hides; a step mostly so has its headline
    for (i, s) in r.steps.iter().enumerate() {
        let f = match kind_of(r, i as i32 + 1) {
            "down" => tr("看影片时 %d%% 的探测 2 秒没回应"),
            "both" => tr("加上传时 %d%% 的探测 2 秒没回应"),
            "up" => tr("只上传时 %d%% 的探测 2 秒没回应"),
            _ => continue,
        };
        let pct = if s.probes > 0 { s.timeouts * 100 / s.probes } else { 0 };
        if pct >= 2 && !r.unresponsive_steps.contains(&(i as i32 + 1)) {
            add(cf!(f, pct));
        }
    }
    if let Some(s) = r.steps.iter().rev().find(|s| s.short_of_target) {
        let short = |got: Option<f64>, target: Option<f64>| target.filter(|t| got.unwrap_or(0.0) < t * 0.9);
        if let Some(t) = short(s.down_mbps, s.target_down_mbps) {
            add(cf!(tr("下载只跑到 %.1f Mbps（目标 %.0f）"), s.down_mbps.unwrap_or(0.0), t));
        }
        if let Some(t) = short(s.up_mbps, s.target_up_mbps) {
            add(cf!(tr("上传只跑到 %.1f Mbps（目标 %.0f）"), s.up_mbps.unwrap_or(0.0), t));
        }
    }
    if r.notes.iter().any(|n| n == "cell_changed" || n == "rat_changed") {
        add(tr("测试中切了小区，之后的步骤不下结论").into());
    }
    if r.notes.iter().any(|n| n == "other_traffic") {
        add(tr("期间有别的流量，数字可能偏悲观").into());
    }
    if !r.no_baseline && r.notes.iter().any(|n| n == "idle_redone") {
        add(tr("空闲时被打断过，已重新量").into());
    }
    if let Some(b) = r.counter_bytes.filter(|b| *b > 0) {
        add(cf!(tr("这次用了约 %.0f MB"), b as f64 / 1e6));
    }
    out
}

fn step_title(kind: &str, step: i32) -> &'static str {
    match (kind, step) {
        ("idle", _) | ("", 1) => tr("第 1 步：先量空闲"),
        ("down", _) | ("", 2) => tr("第 2 步：看影片的负载"),
        ("both", _) | ("", 3) => tr("第 3 步：再加上传"),
        ("up", _) => tr("第 4 步：只上传"),
        ("recovery", 5) => tr("第 5 步：停下看恢复"),
        ("recovery", _) | ("", 4) => tr("第 4 步：停下看恢复"),
        _ => tr("正在选测试来源…"),
    }
}

/// A step's row name, by what it loaded.
fn row_name(kind: &str) -> &'static str {
    match kind {
        "idle" => tr("空闲"),
        "down" => tr("看影片的负载"),
        "both" => tr("再加上传"),
        "up" => tr("只上传"),
        _ => tr("停止后恢复"),
    }
}

/// A step's name under the chart.
fn chart_name(kind: &str) -> &'static str {
    match kind {
        "idle" => crate::trc!("阶梯", "空闲"),
        "down" => crate::trc!("阶梯", "看影片"),
        "both" => crate::trc!("阶梯", "加上传"),
        "up" => crate::trc!("阶梯", "只上传"),
        _ => crate::trc!("阶梯", "恢复"),
    }
}

fn refusal(code: &str) -> &'static str {
    match code {
        "other_traffic" => tr("还有别的流量，测出来可能偏慢。可以继续，或等它停了再测。"),
        "too_warm" => tr("设备有点热，凉下来再测。"),
        "heat_unknown" => tr("读不到温度，现在不能开始。"),
        "low_battery" => tr("电量低于 20%，插上电再测。"),
        "unreachable" => tr("连不上核心服务，稍后再试。"),
        _ => tr("正在运行别的测试，等它结束再开始。"),
    }
}

impl Ui {
    pub(super) fn build_sub_ladder(&mut self, p: Obj) {
        let th = t();
        let f = uf();
        let sc = kit::scroll(p, 0, super::SUB_VIEW, super::SUB_VIEW);
        let c = kit::card(sc, MARGIN, 4, CARD_W, 300);
        let s = &mut self.lad;
        s.scroll = sc;
        s.card = c;
        s.title = kit::label_w(c, f.cj22b, th.t1, PAD, 12, TW, true, "");
        s.cost = kit::label_w(c, f.cj15, th.t1, PAD, 0, TW, true, "");
        s.chart_name = kit::label(c, f.cj12, th.t3, PAD, 0, tr("反应时间"));
        (s.chart, s.series, _) = kit::chart(c, PAD, 0, TW, CHART_H, CHART_POINTS, th.blue, 0);
        s.chart.chart_set_axis_range(sys::lv_chart_axis_t::LV_CHART_AXIS_PRIMARY_Y, 0, CHART_MS);
        s.res_name = kit::label(c, f.cj12, th.t3, PAD, 0, tr("全程反应时间 · 图顶 1000 ms"));
        (s.res_chart, s.res_delay, _) = kit::chart(c, PAD, 0, TW, RES_H, RES_POINTS, th.blue, 0);
        s.res_chart.chart_set_axis_range(sys::lv_chart_axis_t::LV_CHART_AXIS_PRIMARY_Y, 0, CHART_MS);
        s.res_ticks = (0..ROWS - 1).map(|_| kit::bx(c, 0, 0, 1, RES_H, th.t3, 0)).collect();
        for t in &s.res_ticks {
            t.set_style_bg_opa(lv::OPA_50, 0);
        }
        s.res_steps = (0..ROWS).map(|_| kit::label(c, f.cj12, th.t3, 0, 0, "")).collect();
        s.rate_name = kit::label(c, f.cj12, th.t3, PAD, 0, "");
        s.rate_up_l = kit::label_r(c, f.cj12, th.orange, CARD_W - PAD, 0, crate::trc!("阶梯", "上传"));
        s.rate_down_l = kit::label(c, f.cj12, th.green, 0, 0, crate::trc!("阶梯", "下载"));
        (s.rate_chart, s.rate_down, s.rate_up) = kit::chart(c, PAD, 0, TW, RATE_H, RES_POINTS, th.green, th.orange);
        for _ in 0..ROWS {
            s.key.push(kit::label(c, f.cj14, th.t2, PAD, 0, ""));
            s.val.push(kit::label_r(c, f.n15, th.t1, CARD_W - PAD, 0, ""));
        }
        s.note = kit::label_w(c, f.cj13, th.t2, PAD, 0, TW, true, "");
        s.fixed = kit::label_w(c, f.cj12, th.t3, PAD, 0, TW, true, "");
        (s.btn, s.btn_l) = kit::button(c, PAD, 0, TW, BTN_H, "", BtnKind::Primary, Some(btn_cb), 0);
        (s.btn2, s.btn2_l) = kit::button(c, PAD, 0, TW, BTN_H, "", BtnKind::Plain, Some(btn2_cb), 0);
        (s.btn3, s.btn3_l) = kit::button(c, PAD, 0, TW, BTN_H, tr("以前的结果"), BtnKind::Plain, Some(btn3_cb), 0);
        s.cmp = kit::label_w(c, f.cj13, th.t2, PAD, 0, TW, true, "");
        for i in 0..HIST_ROWS {
            let row = kit::bx(c, PAD, 0, TW, HIST_H, th.card, 8);
            kit::tappable(row, Some(hist_cb), i);
            // a hairline above each row, a "›" at its right: the list reads as rows to tap
            kit::bx(row, 0, 0, TW, 1, th.sep, 0);
            kit::label_r(row, f.cj15, th.t3, TW, 16, "›");
            let top = kit::label(row, f.cj14, th.t1, 0, 8, "");
            let sub = kit::label_w(row, f.cj12, th.t3, 0, 30, TW - 20, false, "");
            sub.set_long_mode(lv::LV_LABEL_LONG_MODE_DOTS);
            s.hist.push((row, top, sub));
        }
    }

    fn ladder_model(&self) -> ladder_t {
        self.model.ladder.clone().unwrap_or_default()
    }

    fn ladder_btn(&mut self) {
        let m = self.ladder_model();
        match view_of(&m) {
            View::Run => {
                // answer first
                self.ladder_label(self.lad.btn_l, tr("正在停止…"));
                lv::refr_now();
                self.src.ladder_stop();
            }
            View::Result => {
                // a kept run goes back to the list, a fresh one leaves the page
                let kept = m.viewing;
                self.src.ladder_dismiss();
                if kept {
                    self.sync();
                } else {
                    self.sub_back();
                }
                return;
            }
            View::History => {
                self.src.ladder_browse(false);
            }
            View::Start => {
                if m.starting {
                    return;
                }
                let busy = self.lad.busy_ok || m.start_error == "other_traffic";
                if self.dg.net_roam && !self.lad.arm.tap(ARM_START, lv::tick_get(), ARM_MS) {
                    self.ladder_paint();
                    return;
                }
                self.ladder_go(busy, false);
                return;
            }
        }
        self.sync();
    }

    fn ladder_btn2(&mut self) {
        let m = self.ladder_model();
        match view_of(&m) {
            View::Result => self.src.ladder_dismiss(),
            View::Start if self.dg.net_roam && !m.starting => {
                if !self.lad.arm.tap(ARM_FULL, lv::tick_get(), ARM_MS) {
                    self.ladder_paint();
                    return;
                }
                self.ladder_go(true, true);
                return;
            }
            _ => {}
        }
        self.sync();
    }

    fn ladder_go(&mut self, busy_ok: bool, roaming_ok: bool) {
        self.lad.arm.at = 0;
        self.ladder_label(self.lad.btn_l, tr("正在开始…"));
        lv::refr_now();
        self.src.ladder_start(busy_ok, roaming_ok);
        self.sync();
    }

    fn ladder_label(&mut self, l: Obj, text: &str) {
        l.set_text(text);
        self.label_cache.remove(&(l.0 as usize));
    }

    fn ladder_kind(&mut self, second: bool, k: BtnKind, code: u8) {
        let (b, l) = if second { (self.lad.btn2, self.lad.btn2_l) } else { (self.lad.btn, self.lad.btn_l) };
        let was = if second { &mut self.lad.kinds.1 } else { &mut self.lad.kinds.0 };
        if *was != Some(code) {
            *was = Some(code);
            kit::button_kind(b, l, k);
        }
    }

    fn ladder_chart(&mut self, curve: &[i32]) {
        if self.lad.drawn == curve {
            return;
        }
        let (ch, se) = (self.lad.chart, self.lad.series);
        let keep = curve.len().saturating_sub(CHART_POINTS as usize);
        let tail = &curve[keep..];
        ch.chart_set_all_values(se, lv::CHART_POINT_NONE);
        for &v in tail {
            ch.chart_set_next_value(se, v.min(CHART_MS));
        }
        self.lad.drawn = curve.to_vec();
    }

    /// The result's charts; false when the result has none (one from an older core).
    fn ladder_result_charts(&mut self, chart: Option<&ladder_chart_t>) -> (bool, bool) {
        let Some(c) = chart.filter(|c| !c.delay.is_empty()) else { return (false, false) };
        let has_rate = !c.down.is_empty();
        if self.lad.res_drawn.as_ref() == Some(c) {
            return (true, has_rate);
        }
        let s = &self.lad;
        let (ch, se) = (s.res_chart, s.res_delay);
        ch.chart_set_all_values(se, lv::CHART_POINT_NONE);
        for &v in c.delay.iter().take(RES_POINTS as usize) {
            ch.chart_set_next_value(se, if v < 0 { lv::CHART_POINT_NONE } else { v.min(CHART_MS) });
        }
        // the steps: a tick where each begins, the name centred in its span
        let x = |col: i32| PAD + col.clamp(0, RES_POINTS as i32) * TW / RES_POINTS as i32;
        let marks = &c.marks;
        // a mark per step after the first, and a kind per step
        let on = !marks.is_empty() && marks.len() < ROWS && c.kinds.len() == marks.len() + 1;
        for (i, t) in s.res_ticks.iter().enumerate() {
            let shown = on && i < marks.len();
            kit::show(*t, shown);
            if shown {
                t.set_x(x(marks[i]));
            }
        }
        let f = uf();
        let mut edges = vec![0];
        edges.extend(marks.iter().copied());
        edges.push(RES_POINTS as i32);
        let labels = s.res_steps.clone();
        for (i, l) in labels.iter().enumerate() {
            let shown = on && i < c.kinds.len();
            kit::show(*l, shown);
            if shown {
                self.set(*l, chart_name(&c.kinds[i]));
                let w = lv::text_w(&l.text(), f.cj12);
                let mid = (x(edges[i]) + x(edges[i + 1])) / 2;
                l.set_x((mid - w / 2).clamp(PAD, PAD + TW - w));
            }
        }
        let s = &self.lad;
        if has_rate {
            let peak = c.down.iter().chain(&c.up).copied().max().unwrap_or(0).max(10);
            // the top in whole 10 Mbps, so the scale reads as a round number
            let top = (peak + 99) / 100 * 100;
            let (rc, d, u) = (s.rate_chart, s.rate_down, s.rate_up);
            rc.chart_set_axis_range(sys::lv_chart_axis_t::LV_CHART_AXIS_PRIMARY_Y, 0, top);
            for (se, vals) in [(d, &c.down), (u, &c.up)] {
                rc.chart_set_all_values(se, lv::CHART_POINT_NONE);
                for &v in vals.iter().take(RES_POINTS as usize) {
                    rc.chart_set_next_value(se, if v < 0 { lv::CHART_POINT_NONE } else { v });
                }
            }
            let name = s.rate_name;
            self.set(name, cf!(tr("速率 · 图顶 %d Mbps"), top / 10));
        }
        self.lad.res_drawn = Some(c.clone());
        (true, has_rate)
    }

    pub(super) fn ladder_paint(&mut self) {
        let m = self.ladder_model();
        let view = view_of(&m);
        let now = lv::tick_get();
        if self.lad.arm.expire(now, ARM_MS) {
            self.lad.busy_ok = false;
        }
        let (title, cost, note, fixed) = (self.lad.title, self.lad.cost, self.lad.note, self.lad.fixed);
        let (btn_l, btn2_l) = (self.lad.btn_l, self.lad.btn2_l);
        let keys = self.lad.key.clone();
        let vals = self.lad.val.clone();
        let mut rows: Vec<(&'static str, Vec<u8>)> = Vec::new();
        let mut note_text: Vec<u8> = Vec::new();
        let mut fixed_text: &str = "";
        let mut cost_text: Vec<u8> = Vec::new();
        let two;
        let mut charts = (false, false);
        let mut cmp_text: Vec<u8> = Vec::new();
        let mut hist_n = 0usize;
        match view {
            View::Start => {
                self.set(title, tr("这里撑得住吗"));
                cost_text = if self.dg.net_roam {
                    tr("漫游中只看轻负载\n约 35 秒\n约用 15 MB\n期间网络会变慢").into()
                } else {
                    tr("约 75 秒\n约用 270 MB\n期间网络会变慢").into()
                };
                if !m.start_error.is_empty() {
                    note_text = refusal(&m.start_error).as_bytes().to_vec();
                } else if m.unfinished {
                    note_text = tr("上次没跑完。").as_bytes().to_vec();
                }
                let armed = self.lad.arm.live(ARM_START, now, ARM_MS);
                let label = if m.starting {
                    tr("正在开始…")
                } else if armed {
                    tr("再按一次开始")
                } else if m.start_error == "other_traffic" {
                    tr("仍要开始")
                } else {
                    tr("开始")
                };
                self.set(btn_l, label);
                self.ladder_kind(false, if armed { BtnKind::Armed } else { BtnKind::Primary }, armed as u8);
                two = self.dg.net_roam;
                if two {
                    let a = self.lad.arm.live(ARM_FULL, now, ARM_MS);
                    self.set(btn2_l, if a { tr("再按一次：约用 270 MB 漫游流量") } else { tr("仍要完整版") });
                    self.ladder_kind(true, if a { BtnKind::Armed } else { BtnKind::Plain }, a as u8 + 2);
                }
            }
            View::Run => {
                self.set(title, if m.stopping { tr("正在停止…") } else { step_title(&m.kind, m.step) });
                let ms = m.now_ms.map_or(b"--".to_vec(), |v| cf!("%d ms", v));
                rows.push((tr("现在"), ms));
                rows.push((tr("下载 / 上传"), cf!("%.0f / %.0f Mbps", m.down_mbps, m.up_mbps)));
                rows.push((tr("已用"), cf!("%.0f MB", m.used_bytes as f64 / 1e6)));
                self.set(btn_l, if m.stopping { tr("正在停止…") } else { tr("停止") });
                self.ladder_kind(false, BtnKind::Danger, 4);
                two = false;
            }
            View::Result => {
                let r = m.result.clone().unwrap_or_default();
                let text = if m.result.is_some() { headline(&r) } else { tr("没拿到结果").into() };
                let wrapped = kit::wrap_cjk(&text, uf().cj22b, TW);
                self.set(title, &wrapped);
                let p50 = |i: usize| -> Vec<u8> {
                    r.steps.get(i).and_then(|s| s.p50_ms).map_or(b"--".to_vec(), |v| cf!("%.0f ms", v))
                };
                let back = match (r.recovered, r.recovery_ms) {
                    (Some(true), Some(ms)) => cf!(tr("%.1f 秒"), ms as f64 / 1000.0),
                    (Some(false), _) => tr("没恢复").into(),
                    _ => b"--".to_vec(),
                };
                for i in 0..r.steps.len().min(ROWS) {
                    let k = kind_of(&r, i as i32 + 1);
                    let v = if k == "recovery" { back.clone() } else { p50(i) };
                    rows.push((row_name(k), v));
                }
                note_text = facts(&r);
                charts = self.ladder_result_charts(r.chart.as_ref());
                cmp_text = comparison(&r);
                fixed_text = tr("只代表此刻。从设备发起，没测 Wi-Fi、通话丢包、CHILL 路径。");
                if m.viewing {
                    self.set(btn_l, tr("返回列表"));
                    self.ladder_kind(false, BtnKind::Plain, 5);
                    two = false;
                } else {
                    self.set(btn_l, tr("完成"));
                    self.ladder_kind(false, BtnKind::Primary, 0);
                    self.set(btn2_l, tr("再测一次"));
                    self.ladder_kind(true, BtnKind::Plain, 3);
                    two = true;
                }
            }
            View::History => {
                self.set(title, tr("保存的结果"));
                hist_n = m.history.len().min(HIST_ROWS);
                if hist_n == 0 {
                    cost_text = tr("还没有保存的结果").into();
                }
                let rows_ui = self.lad.hist.clone();
                for (i, (_, top, sub)) in rows_ui.iter().enumerate().take(hist_n) {
                    let h = &m.history[i];
                    let plan = if h.plan == "roaming" { tr("轻量") } else { tr("完整") };
                    let mut t = when(h.id);
                    t.extend_from_slice(" · ".as_bytes());
                    t.extend_from_slice(plan.as_bytes());
                    self.set(*top, &t);
                    // one line: a break after Latin text becomes a space, after CJK nothing
                    let raw = headline(h);
                    let mut head: Vec<u8> = Vec::with_capacity(raw.len());
                    for (j, b) in raw.iter().enumerate() {
                        match *b {
                            b'\n' if j > 0 && raw[j - 1].is_ascii() => head.push(b' '),
                            b'\n' => {}
                            c => head.push(c),
                        }
                    }
                    self.set(*sub, &head);
                }
                note_text = tr("设备上保存最近 30 次；管理网页「负载体检记录」能看全部、对比和导出。").into();
                self.set(btn_l, tr("返回"));
                self.ladder_kind(false, BtnKind::Plain, 6);
                two = false;
            }
        }
        let cmp = self.lad.cmp;
        let w_cmp = kit::wrap_cjk(&cmp_text, uf().cj13, TW);
        self.set(cmp, &w_cmp);
        for i in 0..ROWS {
            let (k, v) = rows.get(i).map_or(("", Vec::new()), |(k, v)| (*k, v.clone()));
            self.set(keys[i], k);
            self.set(vals[i], &v);
        }
        let wrapped_cost = cost_text;
        self.set(cost, &wrapped_cost);
        let w_note = kit::wrap_cjk(&note_text, uf().cj13, TW);
        self.set(note, &w_note);
        let w_fixed = kit::wrap_cjk(fixed_text.as_bytes(), uf().cj12, TW);
        self.set(fixed, &w_fixed);
        if view == View::Run {
            self.ladder_chart(&m.curve);
        }
        self.ladder_layout(view, rows.len(), two, !m.running && m.show_result && !m.viewing, charts, hist_n);
    }

    #[allow(clippy::too_many_arguments)]
    fn ladder_layout(&mut self, view: View, rows: usize, two: bool, side_by_side: bool, charts: (bool, bool), hist_n: usize) {
        let s = &self.lad;
        let shape = {
            let mut v = vec![view as u8, rows as u8, two as u8, charts.0 as u8, charts.1 as u8, hist_n as u8];
            for o in [s.title, s.cost, s.note, s.fixed, s.cmp] {
                v.extend_from_slice(&o.text());
                v.push(0);
            }
            v
        };
        if shape == s.laid {
            return;
        }
        let (title, cost, name, chart, note, fixed, card, sc) = (s.title, s.cost, s.chart_name, s.chart, s.note, s.fixed, s.card, s.scroll);
        let (btn, btn2) = (s.btn, s.btn2);
        let (keys, vals) = (s.key.clone(), s.val.clone());
        title.update_layout();
        let mut y = 12 + title.height() + 10;
        let has_cost = !cost.text().is_empty();
        kit::show(cost, has_cost);
        if has_cost {
            cost.set_y(y);
            cost.update_layout();
            y += cost.height() + 12;
        }
        let run = view == View::Run;
        kit::show(name, run);
        kit::show(chart, run);
        if run {
            name.set_y(y);
            y += 18;
            chart.set_y(y);
            y += CHART_H + 10;
        }
        for i in 0..ROWS {
            let on = i < rows;
            kit::show(keys[i], on);
            kit::show(vals[i], on);
            if on {
                keys[i].set_y(y + 2);
                vals[i].set_y(y);
                y += ROW_STEP;
            }
        }
        // the result's charts, under its rows
        let (res_name, res_chart, rate_name, rate_chart) = (s.res_name, s.res_chart, s.rate_name, s.rate_chart);
        let (rate_down_l, rate_up_l) = (s.rate_down_l, s.rate_up_l);
        let (ticks, steps) = (s.res_ticks.clone(), s.res_steps.clone());
        kit::show(res_name, charts.0);
        kit::show(res_chart, charts.0);
        if charts.0 {
            y += 6;
            res_name.set_y(y);
            y += 18;
            res_chart.set_y(y);
            for t in &ticks {
                t.set_y(y);
            }
            y += RES_H + 2;
            for l in &steps {
                l.set_y(y);
            }
            y += 18;
        } else {
            for o in ticks.iter().chain(&steps) {
                kit::show(*o, false);
            }
        }
        for o in [rate_name, rate_chart, rate_down_l, rate_up_l] {
            kit::show(o, charts.1);
        }
        if charts.1 {
            y += 4;
            for o in [rate_name, rate_up_l] {
                o.set_y(y);
            }
            rate_up_l.update_layout();
            rate_down_l.update_layout();
            rate_down_l.set_pos(rate_up_l.x() - 10 - rate_down_l.width(), y);
            y += 18;
            rate_chart.set_y(y);
            y += RATE_H + 10;
        }
        let (cmp, hist, btn3) = (s.cmp, s.hist.clone(), s.btn3);
        for (i, (row, _, _)) in hist.iter().enumerate() {
            let on = i < hist_n;
            kit::show(*row, on);
            if on {
                row.set_y(y);
                y += HIST_H + 6;
            }
        }
        for o in [cmp, note, fixed] {
            let on = !o.text().is_empty();
            kit::show(o, on);
            if on {
                o.set_y(y + 2);
                o.update_layout();
                y += o.height() + 8;
            }
        }
        y += 8;
        kit::show(btn2, two);
        if two && side_by_side {
            // 再测一次 on the left, 完成 on the right
            let w = (TW - MARGIN) / 2;
            btn2.set_pos(PAD, y);
            btn2.set_size(w, BTN_H);
            btn.set_pos(PAD + w + MARGIN, y);
            btn.set_size(w, BTN_H);
            y += BTN_H;
        } else {
            btn.set_pos(PAD, y);
            btn.set_size(TW, BTN_H);
            y += BTN_H;
            if two {
                btn2.set_pos(PAD, y + 8);
                btn2.set_size(TW, BTN_H);
                y += 8 + BTN_H;
            }
        }
        let start = view == View::Start;
        kit::show(btn3, start);
        if start {
            btn3.set_pos(PAD, y + 8);
            btn3.set_size(TW, BTN_H);
            y += 8 + BTN_H;
        }
        card.set_height(y + PAD);
        kit::scroll_extent(sc, 4 + y + PAD + MARGIN);
        self.lad.laid = shape;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui_model::ladder_step_t;

    fn run(up: f64) -> ladder_result_t {
        let plain = ladder_step_t::default();
        let both = ladder_step_t {
            target_down_mbps: Some(25.0),
            down_mbps: Some(25.0),
            target_up_mbps: Some(30.0),
            up_mbps: Some(up),
            ..plain.clone()
        };
        ladder_result_t { steps: vec![plain.clone(), plain.clone(), both, plain], ..Default::default() }
    }
    fn said(r: &ladder_result_t) -> String {
        String::from_utf8(headline(r)).unwrap()
    }

    #[test]
    fn each_finding_has_its_words() {
        let mut r = run(9.1);
        assert_eq!(said(&r), "此刻这两档负载下，反应没变慢");
        r.first_slow_step = 3;
        assert_eq!(said(&r), "此刻一加上传，反应就变慢");
        r.queued_steps = vec![3];
        assert_eq!(said(&r), "此刻上传跑到 9 Mbps\n就开始排队");
        let mut r = run(9.1);
        r.unloaded_steps = vec![3];
        assert_eq!(said(&r), "此刻上传只跑到 9 Mbps\n这一步不下结论");
        r.no_baseline = true;
        assert_eq!(said(&r), "空闲基准没量好，这次不下结论");
        r.aborted = Some("stopped".into());
        assert_eq!(said(&r), "已停止，这次不算数");
    }
    /// The plan with the upload-only step: idle, down, both, up, recovery.
    fn five(up_slow: bool) -> ladder_result_t {
        let k = |kind: &str| ladder_step_t { kind: kind.into(), p50_ms: Some(50.0), ..Default::default() };
        let mut r = ladder_result_t { steps: ["idle", "down", "both", "up", "recovery"].map(k).to_vec(), ..Default::default() };
        r.first_slow_step = 3;
        r.slow_steps = if up_slow { vec![3, 4] } else { vec![3] };
        r
    }
    #[test]
    fn the_upload_only_step_tells_which_queues() {
        let f = String::from_utf8(facts(&five(false))).unwrap();
        assert!(f.contains("只上传时没变慢：是上下一起才排队"), "{f}");
        let f = String::from_utf8(facts(&five(true))).unwrap();
        assert!(f.contains("只上传也变慢：是上传本身在排队"), "{f}");
        let mut r = five(true);
        r.first_slow_step = 4;
        assert_eq!(said(&r), "此刻只上传，反应就变慢");
        r.unresponsive_steps = vec![4];
        assert_eq!(said(&r), "此刻只上传，\n网络就没反应了");
        let mut r = five(false);
        (r.first_slow_step, r.slow_steps) = (0, vec![]);
        assert_eq!(said(&r), "此刻这三档负载下，反应都没变慢");
        // an old result: no kinds, four steps
        assert_eq!(said(&run(30.0)), "此刻这两档负载下，反应没变慢");
    }
    #[test]
    fn a_result_is_set_beside_the_last_run_like_it() {
        let mut r = five(false);
        r.band = "n78".into();
        r.pci = Some(837);
        r.recovery_ms = Some(500);
        let mut p = five(false);
        p.id = 1_793_964_864;
        p.steps[2].p50_ms = Some(120.0);
        (p.band, p.pci, p.recovery_ms) = ("n78".into(), Some(900), Some(700));
        r.steps[2].p50_ms = Some(194.0);
        r.prev = Some(Box::new(p));
        let c = String::from_utf8(comparison(&r)).unwrap();
        assert!(c.starts_with("和 11-06 11:34 那次比："), "{c}");
        assert!(c.contains("\n再加上传 120 → 194 ms"), "{c}");
        assert!(c.contains("\n停止后恢复 0.7 → 0.5 秒"), "{c}");
        assert!(c.ends_with("两次不在同一个小区，差别可能来自小区"), "{c}");
        r.prev.as_mut().unwrap().pci = Some(837);
        assert!(!String::from_utf8(comparison(&r)).unwrap().contains("小区"));
        r.prev = None;
        assert!(comparison(&r).is_empty());
    }
    #[test]
    fn a_result_never_mentions_a_call_working() {
        assert!(!said(&run(30.0)).contains("通话"));
    }
    #[test]
    fn the_short_rate_and_cost_are_shown() {
        let mut r = run(9.1);
        r.steps[2].short_of_target = true;
        r.counter_bytes = Some(152_000_000);
        let f = String::from_utf8(facts(&r)).unwrap();
        assert!(f.contains("上传只跑到 9.1 Mbps（目标 30）"), "{f}");
        assert!(f.contains("这次用了约 152 MB"), "{f}");
    }
}
