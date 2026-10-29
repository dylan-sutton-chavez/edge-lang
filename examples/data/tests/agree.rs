use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use edge::code::SSAChunk;
use edge::embed::{Limits, VM};
use edge::lang::Runtime;
use edge::deps::{partition_bindings, NativeBinding, Resolved, Resolver};
use edge::value::{HeapObj, HeapPool, SchedulerStatus, Val, VmErr};

// every run meters what it allocates as the artifacts do
edge::allocator!();

const MINI_JOB: &str = include_str!("../github-hour.mini");
const TINY_JOB: &str = include_str!("../github-hour.flow.yaml");

// how much of a body one read hands over as the native host reads it
const CHUNK: usize = 64 << 10;

// the reports both jobs write in the order mini prints them
const REPORTS: [&str; 5] = ["top_stars", "busy_branches", "activity.csv", "issues_opened", "time_to_close"];

/* a call the program made that waits for the transport to answer it */
enum Pending { Head, Read }

/* the hour's url served from the slice as the net plugin's contract words it */
struct Transport {
    body: Vec<u8>,
    at: usize,
    asked: bool,
    pending: Vec<Pending>,
}

impl Transport {
    fn request(&mut self, url: &str, inflate: bool) -> Result<Val, VmErr> {
        if url != data::HOUR || !inflate {
            return Err(VmErr::Raised(format!("ValueError: the test serves {} inflated, not {url}", data::HOUR)));
        }
        self.asked = true;
        Ok(Val::int(1))
    }

    /* a waiting call's answer as a value on `heap` */
    fn answer(&mut self, call: Pending, heap: &mut HeapPool) -> Result<Val, VmErr> {
        match call {
            Pending::Head => {
                let headers = heap.alloc(HeapObj::List(Rc::new(RefCell::new(Vec::new()))))?;
                heap.alloc(HeapObj::List(Rc::new(RefCell::new(vec![Val::int(200), headers]))))
            }
            Pending::Read if self.at >= self.body.len() => Ok(Val::none()),
            Pending::Read => {
                let end = (self.at + CHUNK).min(self.body.len());
                let chunk = self.body[self.at..end].to_vec();
                self.at = end;
                heap.alloc(HeapObj::Bytes(chunk))
            }
        }
    }
}

/* the resolver that gives a job the net plugin over `transport` */
struct Net(Arc<Mutex<Transport>>);

fn text_arg(heap: &HeapPool, args: &[Val], i: usize) -> Option<String> {
    match args.get(i).map(|&v| heap.try_get(v)) {
        Some(Some(HeapObj::Str(s))) => Some(s.to_string()),
        _ => None,
    }
}

impl Resolver for Net {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if spec != "net" {
            return Err(format!("module '{spec}' is not provided by this host"));
        }
        let (a, b, c) = (self.0.clone(), self.0.clone(), self.0.clone());
        let request = NativeBinding { name: "request".into(), func: Arc::new(move |heap: &mut HeapPool, args: &[Val], _| {
            let url = text_arg(heap, args, 1).unwrap_or_default();
            let inflate = text_arg(heap, args, 4).as_deref() == Some("gzip");
            a.lock().unwrap().request(&url, inflate)
        }) };
        let response = NativeBinding { name: "response".into(), func: Arc::new(move |_: &mut HeapPool, _: &[Val], _| {
            b.lock().unwrap().pending.push(Pending::Head);
            Err(VmErr::HostCallDeferred)
        }) };
        let read = NativeBinding { name: "read".into(), func: Arc::new(move |_: &mut HeapPool, _: &[Val], _| {
            c.lock().unwrap().pending.push(Pending::Read);
            Err(VmErr::HostCallDeferred)
        }) };
        let close = NativeBinding { name: "close".into(), func: Arc::new(|_: &mut HeapPool, _: &[Val], _| Ok(Val::none())) };
        let (bindings, classes, consts) = partition_bindings(vec![request, response, read, close]);
        Ok(Resolved::Native { bindings, classes, consts, canonical: "net".into() })
    }
}

thread_local! { static SENT: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) }; }

fn keep(channel: &str, line: &str) -> bool {
    SENT.with(|s| s.borrow_mut().push((channel.to_string(), line.to_string())));
    true
}

/* what one job printed and sent once it ran to its end over `body` */
fn run(runtime: &'static Runtime, front: &edge::lang::Front, names: &edge::lang::Names, src: &str, body: &[u8]) -> (String, Vec<(String, String)>) {
    let transport = Arc::new(Mutex::new(Transport { body: body.to_vec(), at: 0, asked: false, pending: Vec::new() }));
    let chunk = match front.program(src, Box::new(Net(transport.clone()))) {
        Ok(c) => c,
        Err(errs) => panic!("the job does not compile: {:?}", errs.iter().map(|d| d.msg.clone()).collect::<Vec<_>>()),
    };
    let chunk: &'static SSAChunk = edge::util::leak(Box::new(Rc::unwrap_or_clone(chunk)));
    let mut vm = VM::with_runtime(runtime, chunk, Limits { ops: 20_000_000_000, memory: 1 << 30 });
    vm.send_hook = Some(keep);
    SENT.with(|s| s.borrow_mut().clear());
    let mut id = 0u64;
    loop {
        match vm.run() {
            Ok(_) => break,
            Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)) => {
                let call = transport.lock().unwrap().pending.pop().expect("a wait with no call");
                let answer = transport.lock().unwrap().answer(call, vm.heap_mut());
                let delivered = match answer {
                    Ok(v) => vm.inject_host_result_by_id(id, v),
                    Err(e) => vm.inject_host_error_by_id(id, e),
                };
                assert!(delivered, "nothing waits on call {id}");
                id += 1;
            }
            Err(e) => panic!("the job failed: {}", e.render_at(names, src, vm.error_pos(), None)),
        }
    }
    assert!(transport.lock().unwrap().asked, "the job never asked net for the hour");
    (vm.output_text(), SENT.with(|s| s.borrow().clone()))
}

/* mini's printout cut into its reports by the line naming each */
fn printed(out: &str) -> BTreeMap<String, Vec<String>> {
    let mut reports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut at = String::new();
    for line in out.lines() {
        if REPORTS.contains(&line) {
            at = line.to_string();
            reports.entry(at.clone()).or_default();
        } else {
            assert!(!at.is_empty(), "mini printed {line:?} before naming a report");
            reports.get_mut(&at).unwrap().push(line.to_string());
        }
    }
    reports
}

/* tiny's rows written as mini prints the row of each report */
fn rows(sent: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut reports: BTreeMap<String, Vec<String>> = REPORTS.iter().map(|r| (r.to_string(), Vec::new())).collect();
    for (channel, line) in sent {
        let report = channel.strip_prefix("file:").unwrap_or(channel);
        let shown = if report == "activity.csv" {
            line.clone()
        } else {
            let row: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{report} wrote {line:?}, {e}"));
            let repo = row["repo"]["name"].as_str().unwrap_or_else(|| panic!("{report} wrote a row with no repo.name"));
            match report {
                "busy_branches" => format!("{:>6}  {repo} {}", count(&row), row["branch"].as_str().unwrap_or("")),
                "time_to_close" => format!("{:>8.2}  {repo}", row["average"].as_f64().unwrap_or(f64::NAN)),
                _ => format!("{:>6}  {repo}", count(&row)),
            }
        };
        reports.get_mut(report).unwrap_or_else(|| panic!("tiny wrote {channel}, which no report names")).push(shown);
    }
    reports
}

fn count(row: &serde_json::Value) -> i64 {
    row["count"].as_i64().unwrap_or_else(|| panic!("a row with no count {row}"))
}

/* the mini job and tiny's flow give every report the same rows on the slice */
#[test]
fn both_jobs_agree_report_by_report_on_the_slice() {
    let slice = data::slice(&data::cache(Path::new(env!("CARGO_TARGET_TMPDIR"))));
    assert_eq!(slice.iter().filter(|&&b| b == b'\n').count(), data::SLICE_RECORDS);
    let (out, _) = run(&mini::RUNTIME, &mini::FRONT, &mini::NAMES, MINI_JOB, &slice);
    let (_, sent) = run(&tiny::RUNTIME, &tiny::FRONT, &tiny::lang::NAMES, TINY_JOB, &slice);
    let (mini, tiny) = (printed(&out), rows(&sent));
    for report in REPORTS {
        let (m, t) = (&mini[report], &tiny[report]);
        assert!(!t.is_empty(), "tiny wrote no rows for {report}");
        assert_eq!(m, t, "mini and tiny disagree on {report}");
    }
    assert_eq!(mini.len(), REPORTS.len(), "mini printed reports tiny never wrote");
}
