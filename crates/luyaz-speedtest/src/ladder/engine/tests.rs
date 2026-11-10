//! Whole runs against a loopback server that can be made to misbehave.
use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

#[derive(Default)]
struct Behave {
    /// added to each latency answer while an upload is running
    slow_under_upload_ms: u64,
    refuse_upload: bool,
    /// the download stops sending for this long, once
    stall_download: Duration,
    stall_forever: bool,
    /// answer every latency request with a 307 to this base
    redirect_to: Mutex<Option<String>>,
    uploads: AtomicUsize,
    last_upload: Mutex<Option<Instant>>,
    stalled: AtomicBool,
}
fn serve(b: Arc<Behave>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}/s/", l.local_addr().unwrap());
    thread::spawn(move || {
        for s in l.incoming().flatten() {
            let b = b.clone();
            thread::spawn(move || conn(s, &b));
        }
    });
    addr
}
fn conn(mut s: TcpStream, b: &Behave) {
    let mut r = BufReader::new(s.try_clone().unwrap());
    loop {
        let mut first = String::new();
        if r.read_line(&mut first).unwrap_or(0) == 0 {
            return;
        }
        let mut len = 0usize;
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            if h == "\r\n" {
                break;
            }
        }
        let path = first
            .split(' ')
            .nth(1)
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        if first.starts_with("POST") {
            let mut body = vec![0u8; len];
            if b.refuse_upload {
                return; // the empty reply a real node gave
            }
            if r.read_exact(&mut body).is_err() {
                return;
            }
            b.uploads.fetch_add(1, Ordering::SeqCst);
            *b.last_upload.lock().unwrap() = Some(Instant::now());
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        } else if path.ends_with("latency.txt") || path.ends_with("generate_204") {
            if let Some(to) = b
                .redirect_to
                .lock()
                .unwrap()
                .clone()
                .filter(|_| !path.starts_with("/moved"))
            {
                let m = format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {to}latency.txt\r\nContent-Length: 0\r\n\r\n");
                let _ = s.write_all(m.as_bytes());
                continue;
            }
            let busy = b
                .last_upload
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
            if busy && b.slow_under_upload_ms > 0 {
                thread::sleep(Duration::from_millis(b.slow_under_upload_ms));
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ntest");
        } else {
            const SIZE: usize = 8 * 1024 * 1024;
            let _ = s
                .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\n\r\n").as_bytes());
            let chunk = vec![0u8; 64 * 1024];
            for i in 0..SIZE / chunk.len() {
                if i == 4
                    && (b.stall_forever || !b.stall_download.is_zero())
                    && !b.stalled.swap(true, Ordering::SeqCst)
                {
                    thread::sleep(if b.stall_forever {
                        Duration::from_secs(60)
                    } else {
                        b.stall_download
                    });
                }
                if s.write_all(&chunk).is_err() {
                    return;
                }
            }
        }
    }
}
fn node(base: &str) -> Node {
    Node {
        name: "loop".into(),
        ping_url: format!("{base}latency.txt"),
        download_url: format!("{base}random.jpg"),
        upload_url: format!("{base}upload.php"),
    }
}
const K4: [StepKind; 4] = [StepKind::Idle, StepKind::Down, StepKind::Both, StepKind::Recovery];
/// The four steps from before the upload-only step, each this long.
fn four(d: [Duration; 4]) -> &'static [(StepKind, Duration)] {
    Box::leak(K4.into_iter().zip(d).collect::<Vec<_>>().into_boxed_slice())
}
fn plan() -> Plan {
    let s = Duration::from_millis(3200);
    Plan {
        name: "test",
        steps: four([s, s, Duration::from_millis(4000), s]),
        down_mbps: 8.0,
        down_conns: 2,
        up_mbps: 8.0,
        up_conns: 4,
        budget: 50_000_000,
        trial_bytes: 1024 * 1024,
        gap_after_idle: Duration::ZERO,
    }
}
#[derive(Default)]
struct Script {
    events: Vec<(u8, Event)>,
    ticks: Arc<AtomicUsize>,
    /// step 1 reports other traffic on this many ticks, one after another
    busy_idle: u8,
    redos: u8,
}
impl Watch for Script {
    fn redo_idle(&mut self) {
        self.redos += 1;
    }
    fn tick(&mut self, step: u8, _app: u64) -> Vec<Event> {
        self.ticks.fetch_add(1, Ordering::SeqCst);
        if step == 1 && self.busy_idle > 0 {
            self.busy_idle -= 1;
            return vec![Event {
                kind: EventKind::Void,
                code: "other_traffic",
            }];
        }
        let (now, later): (Vec<_>, Vec<_>) = self.events.drain(..).partition(|(s, _)| *s <= step);
        self.events = later;
        now.into_iter().map(|(_, e)| e).collect()
    }
}
fn run_on(plan: &Plan, n: Node, watch: &mut Script) -> Report {
    run(
        plan,
        &[n],
        &Stopper::new(),
        watch,
        &Mutex::new(Progress::default()),
    )
}
#[test]
fn the_four_steps_in_order_hold_their_targets_and_find_nothing_wrong() {
    let b = Arc::new(Behave::default());
    let ticks = Arc::new(AtomicUsize::new(0));
    let mut w = Script {
        ticks: ticks.clone(),
        ..Script::default()
    };
    let r = run_on(&plan(), node(&serve(b.clone())), &mut w);
    assert_eq!(r.aborted, None, "{r:?}");
    assert!(ticks.load(Ordering::SeqCst) >= 10);
    assert_eq!(r.steps[0].down_mbps, None);
    let (video, backup) = (&r.steps[1], &r.steps[2]);
    let d = video.down_mbps.unwrap();
    assert!((7.0..=9.5).contains(&d), "down {d}");
    let (d, u) = (backup.down_mbps.unwrap(), backup.up_mbps.unwrap());
    assert!((7.0..=9.5).contains(&d), "down {d}");
    assert!((6.5..=9.5).contains(&u), "up {u}");
    assert!(!video.short_of_target && !backup.short_of_target);
    assert_eq!(r.findings.first_slow_step, None, "{:?}", r.findings);
    assert_eq!(r.findings.recovered, Some(true), "{:?}", r.findings);
    // after step 3 the uploads end
    assert!(b.last_upload.lock().unwrap().unwrap().elapsed() > Duration::from_secs(2));
    assert!(
        r.app_bytes > 5_000_000 && r.app_bytes < 50_000_000,
        "{}",
        r.app_bytes
    );
}
#[test]
fn the_calibration_gap_is_not_measured() {
    let mut p = plan();
    p.gap_after_idle = Duration::from_secs(2);
    let r = run_on(
        &p,
        node(&serve(Arc::new(Behave::default()))),
        &mut Script::default(),
    );
    // step 1 is 3.2 s: 12 windows, not 20
    assert!(r.steps[0].stats.windows <= 13, "{:?}", r.steps[0].stats);
    assert_eq!(r.aborted, None);
}
#[test]
fn a_load_that_never_happened_is_not_a_pass() {
    let step = |down: f64, up: Option<f64>| StepReport {
        target_down_mbps: Some(3.0),
        down_mbps: Some(down),
        target_up_mbps: up.map(|_| 3.0),
        up_mbps: up,
        ..StepReport::default()
    };
    let idle = StepReport::default();
    let judged = || Findings {
        recovered: Some(true),
        recovery_ms: Some(0),
        ..Findings::default()
    };
    // what the device measured on 11-05
    let mut f = judged();
    apply_unloaded(
        &mut f,
        &[
            idle.clone(),
            step(0.2, None),
            step(0.25, Some(0.0)),
            idle.clone(),
        ],
        None,
    );
    assert_eq!(f.unloaded_steps, vec![2, 3]);
    assert_eq!(f.recovered, None);
    // the video load held and only the backup fell short
    let mut f = judged();
    apply_unloaded(
        &mut f,
        &[
            idle.clone(),
            step(3.0, None),
            step(3.0, Some(1.0)),
            idle.clone(),
        ],
        None,
    );
    assert_eq!(f.unloaded_steps, vec![3]);
    assert_eq!(f.recovered, Some(true));
    // close to the target is a load
    let mut f = judged();
    apply_unloaded(
        &mut f,
        &[idle.clone(), step(2.0, None), step(2.0, Some(2.0)), idle],
        None,
    );
    assert!(f.unloaded_steps.is_empty());
}
/// What the global device measured on 11-06: the cell changed, and step 3 went unanswered.
#[test]
fn a_step_without_answers_is_found_without_a_baseline() {
    let st = |p50: f64, unresponsive: bool| StepStats {
        p50_ms: Some(p50),
        p90_ms: Some(p50),
        windows: 60,
        unresponsive,
        ..StepStats::default()
    };
    let steps = [
        st(31.0, false),
        st(113.0, false),
        st(2000.0, true),
        st(2000.0, true),
    ];
    let f = judge(&steps, &K4, &[], Some(2));
    assert_eq!(f.unresponsive_steps, vec![3]);
    assert_eq!(f.first_slow_step, None);
    let f = judge(&steps, &K4, &[], Some(1));
    assert!(f.no_baseline);
    assert_eq!(f.unresponsive_steps, vec![3]);
    let report = |stats: StepStats| StepReport {
        target_up_mbps: Some(30.0),
        up_mbps: Some(0.9),
        stats,
        ..StepReport::default()
    };
    let mut f = judge(&steps, &K4, &[], Some(2));
    let reps = [
        StepReport::default(),
        StepReport::default(),
        report(steps[2].clone()),
        StepReport::default(),
    ];
    apply_unloaded(&mut f, &reps, Some(2));
    assert!(f.unloaded_steps.is_empty(), "{f:?}");
}
/// What the device measured on 11-05: 9 of 30 Mbps up, delay from 46 to 147 ms.
#[test]
fn a_short_load_with_the_delay_up_found_the_limit() {
    let stats = |p50: f64, p90: f64| StepStats {
        p50_ms: Some(p50),
        p90_ms: Some(p90),
        windows: 40,
        unresponsive: false,
        ..StepStats::default()
    };
    let idle = StepReport {
        stats: stats(46.0, 51.0),
        ..StepReport::default()
    };
    let down = StepReport {
        target_down_mbps: Some(25.0),
        down_mbps: Some(25.0),
        stats: stats(49.0, 63.0),
        ..StepReport::default()
    };
    let both = StepReport {
        target_up_mbps: Some(30.0),
        up_mbps: Some(9.1),
        stats: stats(147.0, 404.0),
        ..down.clone()
    };
    let steps = [idle.clone(), down, both, idle];
    let slow = || Findings {
        first_slow_step: Some(3),
        recovered: Some(true),
        ..Findings::default()
    };
    let mut f = slow();
    apply_unloaded(&mut f, &steps, None);
    assert_eq!(f.queued_steps, vec![3]);
    assert!(f.unloaded_steps.is_empty());
    // the cell changed at step 3: its delay is not comparable
    let mut f = Findings::default();
    apply_unloaded(&mut f, &steps, Some(3));
    assert!(f.queued_steps.is_empty());
    assert_eq!(f.unloaded_steps, vec![3]);
}
#[test]
fn delay_under_the_backup_load_is_found_at_step_3() {
    let b = Arc::new(Behave {
        slow_under_upload_ms: 300,
        ..Behave::default()
    });
    let r = run_on(&plan(), node(&serve(b)), &mut Script::default());
    assert_eq!(
        r.findings.first_slow_step,
        Some(3),
        "{:?} {:?}",
        r.findings,
        r.steps
    );
    assert_eq!(r.findings.recovered, Some(true));
}
#[test]
fn a_short_stall_is_part_of_the_result_not_a_failure() {
    let mut p = plan();
    let s = Duration::from_millis(3200);
    p.steps = four([s, Duration::from_secs(7), Duration::from_millis(4000), s]);
    let b = Arc::new(Behave {
        stall_download: Duration::from_secs(6),
        ..Behave::default()
    });
    // one connection stalls; with the other still moving bytes, one connection is enough
    p.down_conns = 1;
    let r = run_on(&p, node(&serve(b)), &mut Script::default());
    assert_eq!(r.aborted, None);
    assert!(!r.steps[1].failed && !r.steps[2].failed);
    assert!(
        r.stalls.iter().any(|s| !s.upload && s.secs >= 5),
        "{:?}",
        r.stalls
    );
}
#[test]
fn a_stall_lasting_to_the_end_of_the_load_is_reported() {
    let mut p = plan();
    p.down_conns = 1;
    let s = Duration::from_millis(3200);
    p.steps = four([s, Duration::from_secs(5), Duration::from_secs(3), s]);
    let b = Arc::new(Behave {
        stall_forever: true,
        ..Behave::default()
    });
    let r = run_on(&p, node(&serve(b)), &mut Script::default());
    assert!(
        r.stalls.iter().any(|s| !s.upload && s.secs >= 5),
        "{:?}",
        r.stalls
    );
}
#[test]
fn a_stop_on_a_stuck_server_ends_the_run_within_two_seconds() {
    let b = Arc::new(Behave {
        stall_forever: true,
        ..Behave::default()
    });
    let n = node(&serve(b));
    let cancel = Stopper::new();
    let c2 = cancel.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let r = run(
            &plan(),
            &[n],
            &c2,
            &mut Script::default(),
            &Mutex::new(Progress::default()),
        );
        let _ = tx.send(r);
    });
    thread::sleep(Duration::from_millis(4500)); // into step 2
    let t = Instant::now();
    cancel.stop();
    let r = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("still running after a stop");
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(r.aborted, Some("stopped"));
    assert_eq!(r.findings, Findings::default());
}
#[test]
fn an_upload_that_fails_at_step_3_says_so_and_concludes_nothing_from_there() {
    let b = Arc::new(Behave {
        refuse_upload: true,
        ..Behave::default()
    });
    let r = run_on(&plan(), node(&serve(b)), &mut Script::default());
    assert!(r.steps[2].failed, "{:?}", r.steps[2]);
    assert_eq!(r.findings.first_slow_step, None);
    assert_eq!(r.findings.recovered, None);
}
#[test]
fn the_budget_ends_the_step_and_is_not_exceeded() {
    let mut p = plan();
    p.budget = 3_000_000;
    let r = run_on(
        &p,
        node(&serve(Arc::new(Behave::default()))),
        &mut Script::default(),
    );
    assert!(r.steps.iter().any(|s| s.budget_ended), "{:?}", r.steps);
    assert!(r.app_bytes <= p.budget + 100_000, "{}", r.app_bytes);
    // a step that lost its load is not a step that held up
    assert!(r.findings.short_steps.contains(&3), "{:?}", r.findings);
    assert_eq!(r.findings.first_slow_step, None);
}
#[test]
fn events_from_the_device() {
    let n = node(&serve(Arc::new(Behave::default())));
    let abort = Event {
        kind: EventKind::Abort,
        code: "overheated",
    };
    let mut w = Script {
        events: vec![(2, abort)],
        ..Script::default()
    };
    let r = run_on(&plan(), n.clone(), &mut w);
    assert_eq!(r.aborted, Some("overheated"));
    assert_eq!(r.findings, Findings::default());

    let cell = Event {
        kind: EventKind::Void,
        code: "cell_changed",
    };
    let mut w = Script {
        events: vec![(3, cell)],
        ..Script::default()
    };
    let r = run_on(&plan(), n, &mut w);
    assert_eq!(r.aborted, None);
    assert_eq!(r.findings.recovered, None, "{:?}", r.findings);
    assert_eq!(r.notes[0].code, "cell_changed");
}
#[test]
fn a_disturbed_idle_step_is_measured_again() {
    let n = node(&serve(Arc::new(Behave::default())));
    let mut p = plan();
    let short = Duration::from_millis(1500);
    p.steps = four([Duration::from_millis(3200), short, short, short]);
    let codes = |r: &Report| r.notes.iter().map(|e| e.code).collect::<Vec<_>>();
    // busy once: measured again, and the run keeps its baseline
    let mut w = Script {
        busy_idle: 1,
        ..Script::default()
    };
    let r = run_on(&p, n.clone(), &mut w);
    assert_eq!(w.redos, 1);
    assert!(!r.findings.no_baseline, "{:?}", r.findings);
    assert_eq!(codes(&r), vec!["idle_redone"]);
    // the steps' spans follow one another; the redo moved step 1's start past the first tick
    let span: Vec<_> = r.steps.iter().map(|s| (s.from_ms, s.to_ms)).collect();
    assert!(span[0].0 > 0 && span.windows(2).all(|w| w[0].1 <= w[1].0), "{span:?}");
    assert!(r.steps[0].stats.probes > 0, "{:?}", r.steps[0].stats);
    // about a rate a second, the download showing in step 2
    let (from, to) = span[1];
    let down = r.rates.iter().filter(|(t, _, _)| (from..=to).contains(t));
    assert!(down.clone().count() >= 1 && down.clone().any(|(_, d, _)| *d > 0), "{:?}", r.rates);

    // busy throughout: two redos, then the baseline is given up and the reason named
    let mut w = Script {
        busy_idle: 3,
        ..Script::default()
    };
    let r = run_on(&p, n, &mut w);
    assert_eq!(w.redos, 2);
    assert!(r.findings.no_baseline, "{:?}", r.findings);
    assert_eq!(codes(&r), vec!["idle_redone", "idle_redone", "idle_busy"]);
}
#[test]
fn the_upload_only_step_stops_the_download_and_keeps_the_upload() {
    let n = node(&serve(Arc::new(Behave::default())));
    let mut p = plan();
    let s = Duration::from_millis(3200);
    p.steps = Box::leak(Box::new([
        (StepKind::Idle, s),
        (StepKind::Down, s),
        (StepKind::Both, s),
        (StepKind::Up, s),
        (StepKind::Recovery, s),
    ]));
    let r = run_on(&p, n, &mut Script::default());
    assert_eq!(r.aborted, None);
    let kinds: Vec<_> = r.steps.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, p.steps.iter().map(|s| s.0).collect::<Vec<_>>());
    let up_only = &r.steps[3];
    assert_eq!((up_only.target_down_mbps, up_only.down_mbps), (None, None));
    assert!(up_only.up_mbps.is_some_and(|m| m > 1.0), "{up_only:?}");
    // the download stopped: about nothing arrives in the upload-only step's span
    let (from, to) = (up_only.from_ms + 1000, up_only.to_ms);
    let down: u32 = r.rates.iter().filter(|x| x.0 > from && x.0 <= to).map(|x| x.1).sum();
    assert!(down < 500, "{:?}", r.rates);
    assert_eq!(r.findings.recovered, Some(true), "{:?}", r.findings);
}
#[test]
fn a_dead_node_gives_way_to_the_next() {
    let dead = node("http://127.0.0.1:1/s/");
    let good = node(&serve(Arc::new(Behave::default())));
    let mut p = plan();
    p.steps = four([Duration::from_millis(600); 4]);
    let r = run(
        &p,
        &[dead, good.clone()],
        &Stopper::new(),
        &mut Script::default(),
        &Mutex::new(Progress::default()),
    );
    assert_eq!(r.node.unwrap().ping_url, good.ping_url);
    assert_eq!(r.aborted, None);
}
#[test]
fn picking_skips_a_node_refusing_uploads_and_follows_a_redirect() {
    let refuses = node(&serve(Arc::new(Behave {
        refuse_upload: true,
        ..Behave::default()
    })));
    let target = serve(Arc::new(Behave::default()));
    let moved = Arc::new(Behave::default());
    *moved.redirect_to.lock().unwrap() = Some(target.clone());
    let source = node(&serve(moved));
    let picked = pick_nodes(&plan(), vec![refuses, source], &Stopper::new());
    assert_eq!(picked.len(), 1, "{picked:?}");
    assert_eq!(picked[0].upload_url, format!("{target}upload.php"));
    assert_eq!(picked[0].download_url, format!("{target}random.jpg"));
}
#[test]
fn a_source_split_across_hosts_keeps_its_urls() {
    let base = serve(Arc::new(Behave::default()));
    let mut split = node(&base);
    split.ping_url = format!("{base}generate_204");
    let picked = pick_nodes(&plan(), vec![split.clone()], &Stopper::new());
    assert_eq!(picked.len(), 1, "{picked:?}");
    assert_eq!(picked[0].ping_url, split.ping_url);
    assert_eq!(picked[0].download_url, split.download_url);
}
/// Against real nodes (network and about 10 MB): `LADDER_NODES=base,base cargo test -- --ignored live`
#[test]
#[ignore]
fn live() {
    let list = std::env::var("LADDER_NODES").unwrap();
    let nodes: Vec<Node> = list
        .split(',')
        .map(|b| Node {
            name: b.into(),
            ping_url: format!("{b}latency.txt"),
            download_url: format!("{b}random4000x4000.jpg"),
            upload_url: format!("{b}upload.php"),
        })
        .collect();
    let cancel = Stopper::new();
    let p = Plan::roaming();
    let picked = pick_nodes(&p, nodes, &cancel);
    println!("picked {picked:?}");
    let mut r = run(
        &p,
        &picked,
        &cancel,
        &mut Script::default(),
        &Mutex::new(Progress::default()),
    );
    let n = r.samples.len();
    r.samples.clear();
    println!("{n} samples\n{}", serde_json::to_string_pretty(&r).unwrap());
}
