#[cfg(test)]
mod test {

    use edge::code::SSAChunk;
    use edge::embed::VM;
    use edge::embed as snapshot;
    use edge::deps::NoopResolver;
    use edge::value::{Limits, SchedulerStatus, VmErr};

    use crate::common::{lex, parser};

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        src: String,
        #[serde(default)]
        output: Vec<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        input: Vec<String>,
        #[serde(default)]
        events: Vec<String>,
        #[serde(default)]
        interactive_events: Vec<String>,
        // substrings the introspection JSON must hold at the first pause
        #[serde(default)]
        globals_contains: Vec<String>,
        #[serde(default)]
        stack_contains: Vec<String>,
        // preempts every n back-edges and 0 keeps the case cooperative
        #[serde(default)]
        preempt_interval: usize,
        // saves and restores at this preempt count and 0 never hops
        #[serde(default)]
        hop_at: usize,
        #[serde(default)]
        min_preempts: usize,
        #[serde(default)]
        max_preempts: Option<usize>,
        // a huge budget so preempts never ride the budget check
        #[serde(default)]
        unmetered: bool,
        // when present `send` reaches a scheduler that this corpus only needs to accept
        #[serde(default)]
        sends: Option<Vec<(String, String)>>,
        // the error line the vm suite checks and a replay leaves alone
        #[serde(default, rename = "error_line")]
        _error_line: Option<usize>,
        // a vm case running on the corpus budget
        #[serde(skip)]
        corpus: bool,
    }

    fn accept_send(_group: &str, _body: &str) -> bool {
        true
    }

    struct Run {
        output: Result<Vec<String>, VmErr>,
        preempts: usize,
        blob_len: usize,
    }

    fn parse_static(src: &str) -> &'static SSAChunk {
        let (tokens, lex_errs) = lex(src);
        assert!(lex_errs.is_empty(), "lex error in {:?}", src);
        let (mut chunk, errs) = parser(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
        assert!(errs.is_empty(), "parse error in {:?}: {:?}", src, errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
        edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
        edge::util::leak(Box::new(chunk))
    }

    fn limits_for(case: &Case) -> Limits {
        if case.unmetered { Limits { ops: u64::MAX, memory: 4 << 30 } } else if case.corpus { crate::common::corpus_limits() } else { Limits::sandbox() }
    }

    fn corpus(cases: Vec<serde_json::Value>) -> Vec<Case> {
        let mut cases: Vec<Case> = serde_json::from_value(cases.into()).expect("vm cases");
        for case in &mut cases { case.corpus = true; }
        cases
    }

    /* a bare VM to restore into since the state arrives from the blob */
    fn fresh(case: &Case, interval: usize) -> VM<'static> {
        let mut vm = crate::common::boot(parse_static(&case.src), limits_for(case));
        vm.set_preempt_interval(interval);
        if case.sends.is_some() { vm.send_hook = Some(accept_send); }
        vm
    }

    fn boot(case: &Case, interval: usize) -> VM<'static> {
        let mut vm = fresh(case, interval);
        vm.input_buffer = case.input.clone();
        for evt in &case.events { vm.push_event(evt).expect("push_event"); }
        vm
    }

    /* saves through `hops` restore cycles then restores the blob twice and keeps the second */
    fn hop(vm: &VM, case: &Case, interval: usize, hops: u32, blob_len: &mut usize) -> VM<'static> {
        let mut blob = snapshot::save(vm, &case.src, "").expect("save");
        for _ in 1..hops {
            let mut mid = fresh(case, interval);
            snapshot::restore(&mut mid, &blob).expect("restore hop");
            blob = snapshot::save(&mid, &case.src, "").expect("save hop");
        }
        assert_eq!(snapshot::source_of(&blob).expect("source_of"), case.src);
        *blob_len = blob.len();
        let mut scratch = fresh(case, interval);
        snapshot::restore(&mut scratch, &blob).expect("first restore");
        drop(scratch);
        let mut next = fresh(case, interval);
        snapshot::restore(&mut next, &blob).expect("second restore");
        next
    }

    /* one driver hopping `hop_events` times at each event pause and once at preempt `hop_at_preempt` */
    fn run_case(case: &Case, interval: usize, hop_events: u32, hop_at_preempt: usize) -> Run {
        let mut vm = boot(case, interval);
        let mut idx = 0;
        let mut preempts = 0;
        let mut blob_len = 0;
        loop {
            match vm.run() {
                Ok(_) => return Run { output: Ok(vm.output.clone()), preempts, blob_len },
                Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) => {
                    if idx >= case.interactive_events.len() {
                        return Run { output: Ok(vm.output.clone()), preempts, blob_len };
                    }
                    if hop_events > 0 {
                        if idx == 0 { check_introspection(&mut vm, case); }
                        vm = hop(&vm, case, interval, hop_events, &mut blob_len);
                    }
                    vm.push_event(&case.interactive_events[idx]).expect("push_event");
                    idx += 1;
                }
                Err(VmErr::HostYield(SchedulerStatus::Preempted)) => {
                    preempts += 1;
                    if hop_at_preempt != 0 && preempts == hop_at_preempt {
                        vm = hop(&vm, case, interval, 1, &mut blob_len);
                    }
                }
                Err(e) => return Run { output: Err(e), preempts, blob_len },
            }
        }
    }

    fn check_introspection(vm: &mut VM, case: &Case) {
        if !case.globals_contains.is_empty() {
            let json = snapshot::inspect_globals(vm);
            for needle in &case.globals_contains {
                assert!(json.contains(needle.as_str()), "globals JSON missing {:?} in {:?}\n  got: {}", needle, case.src, json);
            }
        }
        if !case.stack_contains.is_empty() {
            let json = snapshot::inspect_stack(vm);
            for needle in &case.stack_contains {
                assert!(json.contains(needle.as_str()), "stack JSON missing {:?} in {:?}\n  got: {}", needle, case.src, json);
            }
        }
    }

    fn check(case: &Case, label: &str, run: &Run, failures: &mut Vec<String>) {
        match &run.output {
            Ok(output) => {
                if case.error.is_some() {
                    failures.push(format!("[{label}] {:?}\n   expected error, got output {:?}", case.src, output));
                } else if *output != case.output {
                    failures.push(format!("[{label}] {:?}\n   got {:?}\n   want {:?}", case.src, output, case.output));
                }
            }
            Err(e) => match &case.error {
                Some(expected) if e.to_string().contains(expected.as_str()) => {}
                Some(expected) => failures.push(format!("[{label}] {:?}\n   got error '{e}'\n   want '{expected}'", case.src)),
                None => failures.push(format!("[{label}] {:?}\n   unexpected error: {e}", case.src)),
            },
        }
    }

    fn report(what: &str, failures: Vec<String>) {
        if !failures.is_empty() {
            let shown = failures.len().min(40);
            panic!("{} {what}(s):\n{}", failures.len(), failures[..shown].join("\n"));
        }
    }

    /* every case runs direct and then hopped at its event pauses or at a preempt */
    #[test]
    fn snapshot_cases() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("cases/snapshot.json")).expect("invalid JSON");
        let mut failures = Vec::new();
        for case in crate::common::slice(&cases) {
            assert!(!case.interactive_events.is_empty() || case.preempt_interval > 0,
                "snapshot case exercises no pause: {:?}", case.src);
            let direct = run_case(case, 0, 0, 0);
            assert_eq!(direct.preempts, 0, "interval 0 must not preempt: {:?}", case.src);
            check(case, "direct", &direct, &mut failures);
            if !case.interactive_events.is_empty() {
                check(case, "roundtrip", &run_case(case, case.preempt_interval, 1, 0), &mut failures);
                check(case, "double", &run_case(case, case.preempt_interval, 2, 0), &mut failures);
            }
            if case.preempt_interval > 0 {
                let pre = run_case(case, case.preempt_interval, 0, case.hop_at);
                check(case, "preempt", &pre, &mut failures);
                if pre.preempts < case.min_preempts {
                    failures.push(format!("[preempt] {:?}\n   {} preempts, expected >= {}", case.src, pre.preempts, case.min_preempts));
                }
                if case.max_preempts.is_some_and(|max| pre.preempts > max) {
                    failures.push(format!("[preempt] {:?}\n   {} preempts, expected <= {:?}", case.src, pre.preempts, case.max_preempts));
                }
                if case.hop_at > 0 && pre.blob_len <= 100 {
                    failures.push(format!("[preempt] {:?}\n   implausible blob length {}", case.src, pre.blob_len));
                }
            }
        }
        report("snapshot case failure", failures);
    }

    /* every interactive vm case survives a save and restore at each pause */
    #[test]
    fn vm_corpus_roundtrip() {
        let cases = corpus(crate::miri::corpus());
        let mut failures = Vec::new();
        for case in cases.iter().filter(|c| !c.interactive_events.is_empty() && c.error.is_none()) {
            let direct = run_case(case, 0, 0, 0).output;
            let cycled = run_case(case, 0, 1, 0).output;
            if crate::common::cut_short(&direct) || crate::common::cut_short(&cycled) { continue; }
            match (direct, cycled) {
                (Ok(a), Ok(b)) if a == b => {}
                (Err(a), Err(b)) if a.to_string() == b.to_string() => {}
                (a, b) => failures.push(format!("{:?}\n   direct {:?}\n   cycled {:?}", case.src, a, b)),
            }
        }
        report("corpus roundtrip mismatch", failures);
    }

    /* preempting at every back-edge changes no corpus result */
    #[test]
    fn vm_corpus_preempt_equivalence() {
        let cases = corpus(crate::miri::corpus_slice());
        let mut failures = Vec::new();
        for case in cases.iter().filter(|c| c.interactive_events.is_empty() && c.events.is_empty() && c.error.is_none()) {
            let direct = run_case(case, 0, 0, 0).output;
            let preempted = run_case(case, 1, 0, 0).output;
            if crate::common::cut_short(&direct) || crate::common::cut_short(&preempted) { continue; }
            match (direct, preempted) {
                (Ok(a), Ok(b)) if a == b => {}
                (Err(a), Err(b)) if a.to_string() == b.to_string() => {}
                (a, b) => failures.push(format!("{:?}\n   direct {:?}\n   preempted {:?}", case.src, a, b)),
            }
        }
        report("preempt divergence", failures);
    }

    /* a restore at a preempt and another in the restored run change no corpus result */
    #[test]
    fn vm_corpus_restore_at_a_preempt() {
        let cases = corpus(crate::miri::corpus_slice());
        let mut failures = Vec::new();
        for case in cases.iter().filter(|c| c.interactive_events.is_empty() && c.events.is_empty() && c.error.is_none()) {
            let direct = run_case(case, 0, 0, 0).output;
            let restored = match restored_at(case, 11, &[3, 6]) {
                Ok(out) => out,
                Err(e) => { failures.push(format!("{:?}\n   restore refused {e}", case.src)); continue; }
            };
            if crate::common::cut_short(&direct) || crate::common::cut_short(&restored) { continue; }
            match (direct, restored) {
                (Ok(a), Ok(b)) if a == b => {}
                (Err(a), Err(b)) if a.to_string() == b.to_string() => {}
                (a, b) => failures.push(format!("{:?}\n   direct {:?}\n   restored {:?}", case.src, a, b)),
            }
        }
        report("restore at a preempt divergence", failures);
    }

    /* the run of `case` restored at each preempt `at` names or why it was refused */
    fn restored_at(case: &Case, interval: usize, at: &[usize]) -> Result<Result<Vec<String>, VmErr>, String> {
        let mut vm = boot(case, interval);
        let mut preempts = 0;
        loop {
            match vm.run() {
                Err(VmErr::HostYield(SchedulerStatus::Preempted)) => {
                    preempts += 1;
                    if !at.contains(&preempts) { continue; }
                    let blob = snapshot::save(&vm, &case.src, "").map_err(|e| format!("save at preempt {preempts} {e}"))?;
                    let mut next = fresh(case, interval);
                    snapshot::restore(&mut next, &blob).map_err(|e| format!("at preempt {preempts} {e}"))?;
                    vm = next;
                }
                Ok(_) => return Ok(Ok(vm.output.clone())),
                Err(e) => return Ok(Err(e)),
            }
        }
    }

    /* corrupt or truncated blobs and blobs of another program fail without a panic */
    #[test]
    fn corrupt_blobs() {
        let src = "x = [1, 2, {'k': 3}]\nreceive()\nprint(x)";
        let chunk = parse_static(src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src, "").expect("save");
        drop(vm);

        let restore_fails = |blob: &[u8]| {
            let mut vm = crate::common::boot(chunk, Limits::sandbox());
            snapshot::restore(&mut vm, blob).is_err()
        };
        for cut in (0..blob.len()).step_by(97) {
            assert!(restore_fails(&blob[..cut]), "truncated blob at {cut} restored");
        }
        let mut bad = blob.clone();
        bad[0] ^= 0xFF;
        assert!(restore_fails(&bad), "bad magic restored");
        let mut bad = blob.clone();
        bad[4] = 0xEE;
        assert!(restore_fails(&bad), "bad format restored");
        let mut bad = blob.clone();
        bad.push(0);
        assert!(restore_fails(&bad), "trailing garbage restored");

        // the fingerprint rejects a VM booted from other source
        let mut other = crate::common::boot(parse_static("y = 99\nreceive()\nprint(y)"), Limits::sandbox());
        let err = snapshot::restore(&mut other, &blob).unwrap_err();
        assert!(err.contains("does not match"), "unexpected error: {err}");

        // the pristine blob still restores and finishes
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        snapshot::restore(&mut vm, &blob).expect("pristine blob");
        vm.push_event("go").unwrap();
        vm.run().expect("finish");
        assert_eq!(vm.output, vec!["[1, 2, {'k': 3}]"]);
    }

    /* an untrusted blob with any byte corrupted fails the restore and never runs or traps */
    #[test]
    fn flipped_bytes_never_panic() {
        let src = "r = range(3, 30, 4)\nx = [r, {'k': (1, 2.5)}, 'text']\nreceive()\nprint(x, len(r), r[1])";
        // one parse serves every boot since a chunk holds no state of a run
        let chunk = parse_static(src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src, "").expect("save");
        drop(vm);
        for at in crate::common::slice(0..blob.len()) {
            let mut bad = blob.clone();
            bad[at] ^= 0xFF;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut vm = crate::common::boot(chunk, Limits::sandbox());
                snapshot::restore(&mut vm, &bad).is_err()
            }));
            assert!(matches!(outcome, Ok(true)), "flipping byte {at} of {} restored or panicked", blob.len());
        }
    }

    /* a resealed blob naming a FOREIGN value it never saved fails to restore */
    #[test]
    fn forged_foreign_handles_fail() {
        let src = "x = 2 ** 100\nreceive()\nprint(x)";
        let mut vm = crate::common::boot(parse_static(src), Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src, "").expect("save");
        drop(vm);
        // a FOREIGN Val is tag 0x7FFD then 16 bits of generation and 32 of index
        let handle = |generation: u64, index: u64| (0x7FFD_0000_0000_0000 | generation << 32 | index).to_le_bytes();
        let real = handle(0, 0);
        let body = &blob[..blob.len() - 32];
        assert!(body.windows(8).any(|w| w == real), "the blob names no FOREIGN value");
        for forged in [handle(1, 0), handle(0, 7)] {
            let mut bad = body.to_vec();
            for at in 0..bad.len() - 7 { if bad[at..at + 8] == real { bad[at..at + 8].copy_from_slice(&forged); } }
            let seal = edge::util::sha256::sha256(&bad);
            bad.extend_from_slice(&seal);
            let mut vm = crate::common::boot(parse_static(src), Limits::sandbox());
            let err = snapshot::restore(&mut vm, &bad).unwrap_err();
            assert!(err.contains("missing object"), "unexpected error: {err}");
        }
    }

    /* a resealed forged blob is refused or runs within its limits */
    #[test]
    fn resealed_forgeries_never_panic() {
        // a recursion laid as sections parks inside them with links and levels to bend
        let src = "fn rec(n, acc):\n    if n == 0:\n        return acc\n    s = 0\n    for k in range(3):\n        s += k\n    return rec(n - 1, acc + n + s)\nt = 0\nfor i in range(40):\n    t += rec(30, 0)\nprint(t)";
        forgeries_never_panic(src, 97, 8, if cfg!(miri) { 20_000 } else { 2_000_000 });
        // a recursion through three functions parks holding levels whose count a forgery bends
        forgeries_never_panic(TRIO, 25, 6, if cfg!(miri) { 20_000 } else { 200_000 });
    }

    /* each body byte of the blob `src` saves at preempt `at` set three ways */
    fn forgeries_never_panic(src: &'static str, interval: usize, at: usize, ops: u64) {
        let chunk = parse_static(src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        vm.set_preempt_interval(interval);
        let mut blob = None;
        for _ in 0..at {
            if !matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted))) { break; }
            blob = Some(snapshot::save(&vm, src, "").expect("save"));
        }
        let blob = blob.expect("the run preempts");
        drop(vm);
        let body = blob.len() - 32;
        let start = snapshot::RUNTIME_AT + 32 + src.len();
        let stride = if cfg!(miri) { (body - start) / 16 + 1 } else { 1 };
        for at in crate::common::slice((start..body).step_by(stride)) {
            for v in [0u8, 1, 0xFF] {
                let mut bad = blob[..body].to_vec();
                if bad[at] == v { continue; }
                bad[at] = v;
                let seal = edge::util::sha256::sha256(&bad);
                bad.extend_from_slice(&seal);
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut vm = crate::common::boot(chunk, Limits { ops, memory: 64 << 20 });
                    vm.set_preempt_interval(interval);
                    if snapshot::restore(&mut vm, &bad).is_err() { return; }
                    // what a forgery lets run ends within its budget
                    for _ in 0..100_000 { if !matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted))) { break; } }
                }));
                assert!(outcome.is_ok(), "setting byte {at} of {} to {v} panicked", blob.len());
            }
        }
    }

    // a recursion through three functions which the loop lays as sections that keep levels
    const TRIO: &str = "fn expr(d):\n    v = term(d)\n    i = 0\n    while i < 100:\n        i += 1\n        v = v + term(d)\n    return v\nfn term(d):\n    return factor(d)\nfn factor(d):\n    if d > 0:\n        return expr(d - 1)\n    return 1\nt = 0\nfor i in range(300):\n    t += expr(1)\nprint(t)";

    /* a run restored at a preempt saves and restores again wherever it parks */
    #[test]
    fn a_restored_run_restores_again() {
        // restores at every few preempts of three intervals and the pair a single restore hid
        let patterns: [(usize, &[usize]); 2] = [(25, &[3, 6]), (11, &[5, 10, 15, 20])];
        // under miri the pairs alone over three narrower rounds fit one job
        let (src, out, sweeps): (String, &str, &[usize]) = if cfg!(miri) { (TRIO.replace("range(300)", "range(3)").replace("i < 100", "i < 10"), "363", &[]) } else { (TRIO.into(), "3060300", &[7, 11, 25]) };
        let case: Case = serde_json::from_value(serde_json::json!({ "src": src, "output": [out] })).expect("case");
        let every: Vec<usize> = (1..70_000).collect();
        for (interval, at) in patterns.into_iter().chain(sweeps.iter().map(|&i| (i, &every[..]))) {
            match restored_at(&case, interval, at) {
                Ok(Ok(out)) => assert_eq!(out, case.output, "restored at {} preempts of {interval}", at.len()),
                other => panic!("restored at {} preempts of {interval}: {other:?}", at.len()),
            }
        }
    }

    /* a recursion through three methods meets the depth cold code meets after a restore too */
    #[test]
    fn a_restored_recursion_stops_where_cold_code_stops() {
        // under miri a tenth of the warm loop and three preempts fit one job
        let (rounds, ats) = if cfg!(miri) { (20, 1..4) } else { (200, 1..30) };
        let src = format!("class P:\n    fn __init__(self, text):\n        self.text = text\n        self.at = 0\n    fn expr(self):\n        v = self.term()\n        while self.at < len(self.text) && self.text[self.at] == '+':\n            self.at += 1\n            v = v + self.term()\n        return v\n    fn term(self):\n        v = self.factor()\n        while self.at < len(self.text) && self.text[self.at] == '*':\n            self.at += 1\n            v = v * self.factor()\n        return v\n    fn factor(self):\n        c = self.text[self.at]\n        self.at += 1\n        if c == '(':\n            v = self.expr()\n            self.at += 1\n            return v\n        return int(c)\nfn parse(text):\n    try:\n        return P(text).expr()\n    except RecursionError:\n        return 'deep'\nt = 0\nfor i in range({rounds}):\n    t = t + parse('(1+2)*3+' + str(i % 10))\nprint(t)\nfor n in [100, 300, 400, 1200]:\n    print(n, parse('(' * n + '1' + ')' * n))");
        let case: Case = serde_json::from_value(serde_json::json!({ "src": src })).expect("case");
        let direct = run_case(&case, 0, 0, 0).output.expect("direct");
        assert_eq!(direct[3..], ["400 deep", "1200 deep"], "{direct:?}");
        for at in crate::common::slice(ats) {
            match restored_at(&case, 25, &[at]) {
                Ok(Ok(out)) => assert_eq!(out, direct, "restored at preempt {at}"),
                other => panic!("restored at preempt {at}: {other:?}"),
            }
        }
    }

    /* a blob whose heap passes its vm's memory is refused as it decodes */
    #[test]
    fn a_restore_refuses_a_heap_past_its_memory() {
        // under miri a fortieth of the heap preempted forty times as often fits one job
        let (n, every) = if cfg!(miri) { (1000, 25) } else { (40000, 1000) };
        let src = format!("xs = []\nfor i in range({n}):\n    xs.append('k' + str(i))\nt = 0\nfor i in range({}):\n    t += i\nprint(t, len(xs))", n / 2);
        let chunk = parse_static(&src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        vm.set_preempt_interval(every);
        for _ in 0..50 { assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted)))); }
        let blob = snapshot::save(&vm, &src, "").expect("save");
        let held = vm.memory();
        drop(vm);
        let mut small = crate::common::boot(chunk, Limits { memory: held / 4, ..Limits::sandbox() });
        let err = snapshot::restore(&mut small, &blob).expect_err("a heap four times its memory");
        assert!(err.contains("needs more memory"), "unexpected error: {err}");
        assert!(small.memory_peak() < held, "the refused restore held {} of the {held} the run held", small.memory_peak());
        // the vm the run saved under takes it back and finishes
        let mut same = crate::common::boot(chunk, Limits::sandbox());
        snapshot::restore(&mut same, &blob).expect("restore");
        while matches!(same.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted))) {}
        assert_eq!(same.output, vec![format!("{} {n}", n / 2 * (n / 2 - 1) / 2)]);
    }

    /* a forged blob whose class names itself as a base is refused at restore */
    #[test]
    fn a_class_its_own_base_is_refused() {
        let src = "class Base:\n    pass\nclass Cyc(Base):\n    pass\nt = 0\nfor i in range(100):\n    t += i\nprint(t, issubclass(Cyc, Base))";
        let chunk = parse_static(src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        vm.set_preempt_interval(5);
        for _ in 0..3 { assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted)))); }
        let blob = snapshot::save(&vm, src, "").expect("save");
        drop(vm);
        let body = &blob[..blob.len() - 32];
        let find = |pat: &[u8], from: usize| body[from..].windows(pat.len()).position(|w| w == pat).map(|i| i + from);
        let name = [&3u64.to_le_bytes()[..], b"Cyc"].concat();
        // a class head is its tag then its name then one base and its value
        let head = [&[18u8, 1][..], &name, &1u64.to_le_bytes()].concat();
        let at = find(&head, 0).expect("the blob holds the class head") + head.len();
        let base = body[at..at + 8].to_vec();
        // the class's own value follows its name where the module binds it
        let mut own = None;
        let mut from = 0;
        while let Some(i) = find(&name, from) {
            let v = &body[i + name.len()..i + name.len() + 8];
            if v != base.as_slice() && v[6..] == [0xFC, 0x7F] { own = Some(v.to_vec()); }
            from = i + 1;
        }
        let mut bad = body.to_vec();
        bad[at..at + 8].copy_from_slice(&own.expect("the blob binds the class"));
        let seal = edge::util::sha256::sha256(&bad);
        bad.extend_from_slice(&seal);
        let mut back = crate::common::boot(chunk, Limits::sandbox());
        let err = snapshot::restore(&mut back, &bad).expect_err("a class that is its own base");
        assert!(err.contains("class hierarchy is inconsistent"), "unexpected error: {err}");
        let mut back = crate::common::boot(chunk, Limits::sandbox());
        snapshot::restore(&mut back, &blob).expect("the pristine blob restores");
    }

    /* a restore keeps the order its heap reuses slots in so addresses stay alike */
    #[test]
    fn a_restore_keeps_addresses() {
        // under miri fewer rounds fit one job and still reuse freed slots
        let (n, hops) = if cfg!(miri) { (1000, [5, 10, 15]) } else { (6000, [7, 60, 100]) };
        let src = format!("fn kind_of(n):\n    yield n\nt = 0\nkeep = []\nfor i in range({n}):\n    t += i\n    xs = [i, [i]]\n    if i % 7 == 0:\n        keep.append(xs)\ng = kind_of(1)\nprint(g, t, len(keep))");
        let case: Case = serde_json::from_value(serde_json::json!({ "src": src })).expect("case");
        let direct = run_case(&case, 0, 0, 0).output.expect("direct");
        assert!(direct[0].contains(" at 0x"), "unexpected output {direct:?}");
        for hop in crate::common::slice(hops) {
            assert_eq!(run_case(&case, 50, 0, hop).output.expect("hopped"), direct, "restored at preempt {hop}");
        }
    }

    /* a blob keeps the entry its run started from for a host to name again */
    #[test]
    fn the_entry_rides_in_the_blob() {
        let src = "x = 1\nreceive()\nprint(x)";
        let mut vm = crate::common::boot(parse_static(src), Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        for entry in ["", "main.mini", "sub/dir/main.mini", "<eval>"] {
            let blob = snapshot::save(&vm, src, entry).expect("save");
            assert_eq!((snapshot::source_of(&blob), snapshot::entry_of(&blob)), (Ok(src), Ok(entry)));
            let mut back = crate::common::boot(parse_static(src), Limits::sandbox());
            snapshot::restore(&mut back, &blob).expect("restore");
        }
    }

    /* `limits_of` reports the profile recorded at save time with the remaining budget */
    #[test]
    fn limits_of_reports_recorded_budget() {
        let src = "x = 1\n";
        let saved = |limits: Limits| {
            let vm = crate::common::boot(parse_static(src), limits);
            snapshot::save(&vm, src, "").expect("save")
        };
        let l = snapshot::limits_of(&saved(Limits { ops: 1_234_567, memory: 42_000 })).unwrap();
        assert_eq!((l.ops, l.memory), (1_234_567, 42_000));
        let l = snapshot::limits_of(&saved(Limits::sandbox())).unwrap();
        assert_eq!((l.ops, l.memory), (100_000_000, 256 << 20));
        let l = snapshot::limits_of(&saved(Limits { ops: 5, memory: 9 })).unwrap();
        assert_eq!((l.ops, l.memory), (5, 9));
        let l = snapshot::limits_of(&saved(Limits { ops: 0, memory: 0 })).unwrap();
        assert_eq!((l.ops, l.memory), (0, 0));
        // a budget past four billion stays whole on a 32-bit engine too
        let l = snapshot::limits_of(&saved(Limits { ops: 6_000_000_000, memory: 9 })).unwrap();
        assert_eq!(l.ops, 6_000_000_000);

        // a partly spent budget records its remainder rather than a placeholder of one op
        let src = "x = 1 + 2\n";
        let mut vm = crate::common::boot(parse_static(src), Limits::sandbox());
        vm.run().expect("run");
        let l = snapshot::limits_of(&snapshot::save(&vm, src, "").expect("save")).unwrap();
        assert!(l.ops > 1 && l.ops < 100_000_000, "unexpected ops {}", l.ops);
    }

    // a restore leaves the run no work to pay for the tables it built again
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_restore_charges_nothing_for_the_tables_it_builds_again() {
        let src = "p = 2 ** 61 - 1\ns = set(range(0, 3000 * p, p))\nd = dict.fromkeys(s)\nw = {'x' * 100 + str(i) for i in range(300)}\nt = 0\nfor i in range(50):\n    t += i\nprint(len(s), len(d), len(w), t)\n";
        let chunk = parse_static(src);
        let mut vm = crate::common::boot(chunk, Limits::sandbox());
        vm.set_preempt_interval(100);
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::Preempted))));
        let blob = snapshot::save(&vm, src, "").expect("save");
        let mut next = crate::common::boot(chunk, Limits::sandbox());
        snapshot::restore(&mut next, &blob).expect("restore");
        assert!(!next.heap().has_work());
        while let Err(VmErr::HostYield(SchedulerStatus::Preempted)) = next.run() {}
        assert_eq!(next.output, ["3000 3000 300 1225"]);
    }
}
