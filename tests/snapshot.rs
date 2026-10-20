#[cfg(test)]
mod test {

    use compiler::lexer::lex;
    use compiler::code::{Parser, SSAChunk};
    use compiler::embed::VM;
    use compiler::embed as snapshot;
    use compiler::value::{Limits, SchedulerStatus, VmErr};

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
        // Substrings required in introspection JSON at the first pause.
        #[serde(default)]
        globals_contains: Vec<String>,
        #[serde(default)]
        stack_contains: Vec<String>,
        // Preempt every n back-edges, 0 keeps the case cooperative-only.
        #[serde(default)]
        preempt_interval: usize,
        // Save/restore at this preempt count, 0 never hops.
        #[serde(default)]
        hop_at: usize,
        #[serde(default)]
        min_preempts: usize,
        #[serde(default)]
        max_preempts: Option<usize>,
        // Run on a huge budget so preempts don't ride the budget check.
        #[serde(default)]
        unmetered: bool,
        // Present means send() reaches a scheduler, this corpus only needs it accepted.
        #[serde(default)]
        sends: Option<Vec<(String, String)>>,
        // The line the vm suite checks an error at, which a replay leaves alone.
        #[serde(default, rename = "error_line")]
        _error_line: Option<usize>,
        // A vm.json case, which runs on the corpus budget.
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
        let (mut chunk, errs) = Parser::new(src, tokens.into_iter()).parse();
        assert!(errs.is_empty(), "parse error in {:?}: {:?}", src, errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
        compiler::code::constant_fold(&mut chunk);
        compiler::util::leak(Box::new(chunk))
    }

    fn limits_for(case: &Case) -> Limits {
        if case.unmetered { Limits { ops: u64::MAX, memory: 4 << 30 } } else if case.corpus { crate::common::corpus_limits() } else { Limits::sandbox() }
    }

    fn corpus() -> Vec<Case> {
        let mut cases: Vec<Case> = serde_json::from_str(include_str!("cases/vm.json")).expect("invalid JSON");
        for case in &mut cases { case.corpus = true; }
        cases
    }

    /* Restore target is a bare VM, state arrives from the blob. */
    fn fresh(case: &Case, interval: usize) -> VM<'static> {
        let mut vm = VM::with_limits(parse_static(&case.src), limits_for(case));
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

    /* Save, chain `hops` cycles, restore twice, return the last VM. */
    fn hop(vm: &VM, case: &Case, interval: usize, hops: u32, blob_len: &mut usize) -> VM<'static> {
        let mut blob = snapshot::save(vm, &case.src).expect("save");
        for _ in 1..hops {
            let mut mid = fresh(case, interval);
            snapshot::restore(&mut mid, &blob).expect("restore hop");
            blob = snapshot::save(&mid, &case.src).expect("save hop");
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

    /* One driver, `hop_events` hops at each event pause, `hop_at_preempt` at that preempt. */
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
                        if idx == 0 { check_introspection(&vm, case); }
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

    fn check_introspection(vm: &VM, case: &Case) {
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

    /* Cooperative cases run direct, roundtrip and double, preempt cases add a hopped preempt run. */
    #[test]
    fn snapshot_cases() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("cases/snapshot.json")).expect("invalid JSON");
        let mut failures = Vec::new();
        for case in &cases {
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

    /* Property, every interactive vm.json case survives a save/restore at each pause. */
    #[test]
    fn vm_corpus_roundtrip() {
        let cases = corpus();
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

    /* Property, preempting at every back-edge changes no corpus result. */
    #[test]
    fn vm_corpus_preempt_equivalence() {
        let cases = corpus();
        let mut failures = Vec::new();
        for case in crate::common::slice(cases.iter().filter(|c| c.interactive_events.is_empty() && c.events.is_empty() && c.error.is_none())) {
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

    /* Corrupt, truncated and cross-program blobs fail cleanly, never panic. */
    #[test]
    fn corrupt_blobs() {
        let src = "x = [1, 2, {'k': 3}]\nreceive()\nprint(x)";
        let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src).expect("save");
        drop(vm);

        let restore_fails = |blob: &[u8]| {
            let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
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

        // Fingerprint rejects a VM booted from different source.
        let mut other = VM::with_limits(parse_static("y = 99\nreceive()\nprint(y)"), Limits::sandbox());
        let err = snapshot::restore(&mut other, &blob).unwrap_err();
        assert!(err.contains("does not match"), "unexpected error: {err}");

        // The pristine blob still restores and finishes.
        let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
        snapshot::restore(&mut vm, &blob).expect("pristine blob");
        vm.push_event("go").unwrap();
        vm.run().expect("finish");
        assert_eq!(vm.output, vec!["[1, 2, {'k': 3}]"]);
    }

    /* A blob is untrusted, any single corrupted byte must fail the restore, never run or trap. */
    #[test]
    fn flipped_bytes_never_panic() {
        let src = "r = range(3, 30, 4)\nx = [r, {'k': (1, 2.5)}, 'text']\nreceive()\nprint(x, len(r), r[1])";
        let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src).expect("save");
        drop(vm);
        for at in 0..blob.len() {
            let mut bad = blob.clone();
            bad[at] ^= 0xFF;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
                snapshot::restore(&mut vm, &bad).is_err()
            }));
            assert!(matches!(outcome, Ok(true)), "flipping byte {at} of {} restored or panicked", blob.len());
        }
    }

    /* A resealed blob naming a FOREIGN value it never saved, by a stale generation or a missing index, fails to restore. */
    #[test]
    fn forged_foreign_handles_fail() {
        let src = "x = 2 ** 100\nreceive()\nprint(x)";
        let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
        assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingEvent))));
        let blob = snapshot::save(&vm, src).expect("save");
        drop(vm);
        // A FOREIGN Val is the tag 0x7FFD, then 16 bits of generation and 32 of index.
        let handle = |generation: u64, index: u64| (0x7FFD_0000_0000_0000 | generation << 32 | index).to_le_bytes();
        let real = handle(0, 0);
        let body = &blob[..blob.len() - 32];
        assert!(body.windows(8).any(|w| w == real), "the blob names no FOREIGN value");
        for forged in [handle(1, 0), handle(0, 7)] {
            let mut bad = body.to_vec();
            for at in 0..bad.len() - 7 { if bad[at..at + 8] == real { bad[at..at + 8].copy_from_slice(&forged); } }
            let seal = compiler::util::sha256::sha256(&bad);
            bad.extend_from_slice(&seal);
            let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
            let err = snapshot::restore(&mut vm, &bad).unwrap_err();
            assert!(err.contains("missing object"), "unexpected error: {err}");
        }
    }

    /* limits_of reports the profile recorded at save time, remaining budget included. */
    #[test]
    fn limits_of_reports_recorded_budget() {
        let src = "x = 1\n";
        let saved = |limits: Limits| {
            let vm = VM::with_limits(parse_static(src), limits);
            snapshot::save(&vm, src).expect("save")
        };
        let l = snapshot::limits_of(&saved(Limits { ops: 1_234_567, memory: 42_000 })).unwrap();
        assert_eq!((l.ops, l.memory), (1_234_567, 42_000));
        let l = snapshot::limits_of(&saved(Limits::sandbox())).unwrap();
        assert_eq!((l.ops, l.memory), (100_000_000, 256 << 20));
        let l = snapshot::limits_of(&saved(Limits { ops: 5, memory: 9 })).unwrap();
        assert_eq!((l.ops, l.memory), (5, 9));
        let l = snapshot::limits_of(&saved(Limits { ops: 0, memory: 0 })).unwrap();
        assert_eq!((l.ops, l.memory), (0, 0));
        // A budget past four billion stays whole on a 32-bit engine too.
        let l = snapshot::limits_of(&saved(Limits { ops: 6_000_000_000, memory: 9 })).unwrap();
        assert_eq!(l.ops, 6_000_000_000);

        // A partially spent budget records the remainder, never the old ops 1 placeholder.
        let src = "x = 1 + 2\n";
        let mut vm = VM::with_limits(parse_static(src), Limits::sandbox());
        vm.run().expect("run");
        let l = snapshot::limits_of(&snapshot::save(&vm, src).expect("save")).unwrap();
        assert!(l.ops > 1 && l.ops < 100_000_000, "unexpected ops {}", l.ops);
    }
}
