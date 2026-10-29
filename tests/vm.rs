#[cfg(test)]
mod test {

    use edge::embed::VM;
    use edge::deps::NoopResolver;
    use edge::value::{SchedulerStatus, VmErr};

    use crate::common::{lex, parser};

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        src: String,
        output: Vec<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        input: Vec<String>,
        #[serde(default)]
        events: Vec<String>,
        // events pushed one at a time after each PendingEvent yield
        #[serde(default)]
        interactive_events: Vec<String>,
        // when present a scheduler hook records the group and body pairs `send` hands over
        #[serde(default)]
        sends: Option<Vec<(String, String)>>,
        // the line the error points at counted from 1
        #[serde(default)]
        error_line: Option<usize>,
    }

    std::thread_local! {
        static SENT: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn record_send(group: &str, body: &str) -> bool {
        SENT.with(|s| s.borrow_mut().push((group.to_string(), body.to_string())));
        true
    }

    // clears what an earlier case left and installs the recording hook when sends are expected
    fn hook_sends(vm: &mut VM, case: &Case) {
        SENT.with(|s| s.borrow_mut().clear());
        if case.sends.is_some() { vm.send_hook = Some(record_send); }
    }

    fn sent() -> Vec<(String, String)> {
        SENT.with(|s| s.borrow().clone())
    }

    // resumes each PendingEvent with the next entry of `interactive_events`
    fn drive(vm: &mut VM, interactive: &[String]) -> Result<(), VmErr> {
        let mut idx = 0;
        loop {
            match vm.run() {
                Ok(_) => return Ok(()),
                Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) => {
                    if idx >= interactive.len() { return Ok(()); }
                    vm.push_event(&interactive[idx]).expect("push_event");
                    idx += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /* two cases equal in every field would test the same thing twice */
    #[test]
    fn no_duplicate_cases() {
        let cases = crate::miri::corpus();
        let mut seen = std::collections::HashMap::new();
        let repeated: Vec<String> = cases.iter().enumerate()
            .filter_map(|(i, case)| seen.insert(case.to_string(), i).map(|first| format!("case {i} repeats case {first}, {}", case["src"])))
            .collect();
        assert!(repeated.is_empty(), "{} duplicate case(s):\n{}", repeated.len(), repeated.join("\n"));
    }

    /* the vm cases under sandbox limits so runaway allocation and recursion raise instead of hanging */
    #[test]
    fn test_cases() {
        let cases: Vec<Case> = serde_json::from_value(crate::miri::corpus_slice().into()).expect("vm cases");

        let mut failures: Vec<String> = Vec::new();
        for case in cases {
            let (tokens, lex_errs) = lex(&case.src);
            // an expected error the lexer already reports needs no run
            if !lex_errs.is_empty()
                && let Some(expected) = &case.error
                && lex_errs.iter().any(|e| e.msg.contains(expected.as_str()))
            {
                continue;
            }
            let (mut chunk, errors) = parser(&case.src, tokens.into_iter(), Box::new(NoopResolver)).parse();
            // an expected error the parser already reports needs no run
            if let Some(expected) = &case.error && errors.iter().any(|e| e.msg.contains(expected.as_str())) { continue; }
            // the fold pass production runs so its bugs surface here
            edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
            let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
            vm.input_buffer = case.input.clone();
            hook_sends(&mut vm, &case);
            for evt in &case.events { vm.push_event(evt).expect("push_event"); }
            let result = drive(&mut vm, &case.interactive_events);
            if crate::common::cut_short(&result) { continue; }

            // every mismatch is collected so one run lists all regressions
            match result {
                Ok(_obj) => {
                    if let Some(expected) = &case.error {
                        failures.push(format!("NO ERROR {:?}\n   want '{}'", case.src, expected));
                    }
                    if vm.output != case.output {
                        failures.push(format!("OUTPUT {:?}\n   got {:?}\n   want {:?}", case.src, vm.output, case.output));
                    }
                    if let Some(want) = &case.sends && &sent() != want {
                        failures.push(format!("SENDS {:?}\n   got {:?}\n   want {:?}", case.src, sent(), want));
                    }
                }
                Err(e) => match &case.error {
                    Some(expected) => {
                        if !e.to_string().contains(expected.as_str()) {
                            failures.push(format!("ERR {:?}\n   got '{}'\n   want '{}'", case.src, e, expected));
                        }
                        let line = vm.error_pos().map(|at| case.src[..at.min(case.src.len())].matches('\n').count() + 1);
                        if let Some(want) = case.error_line && line != Some(want) {
                            failures.push(format!("LINE {:?}\n   got {:?}\n   want {}", case.src, line, want));
                        }
                    }
                    None => failures.push(format!("RAISED {:?}: {}", case.src, e)),
                }
            }
        }
        if !failures.is_empty() {
            let shown = failures.len().min(80);
            panic!("{} case(s) failed:\n{}", failures.len(), failures[..shown].join("\n"));
        }
    }

    /* every vm case again in strict input mode where reading past the supplied buffer fails */
    #[test]
    fn strict_cases() {
        let cases: Vec<Case> = serde_json::from_value(crate::miri::corpus_slice().into()).expect("vm cases");

        for case in cases {
            let (tokens, lex_errs) = lex(&case.src);
            // lex errors arrive as diagnostics matched against the expected error
            if !lex_errs.is_empty() {
                if let Some(expected) = &case.error {
                    assert!(
                        lex_errs.iter().any(|e| e.msg.contains(expected.as_str())),
                        "wrong lex error on {:?}: got {:?}, expected '{}'",
                        case.src,
                        lex_errs.iter().map(|e| e.msg).collect::<Vec<_>>(),
                        expected
                    );
                    continue;
                }
                panic!("lex error on {:?}: {:?}", case.src, lex_errs.iter().map(|e| e.msg).collect::<Vec<_>>());
            }
            let (mut chunk, errs) = parser(&case.src, tokens.into_iter(), Box::new(NoopResolver)).parse();
            if !errs.is_empty() {
                match &case.error {
                    Some(expected) => {
                        assert!(
                            errs.iter().any(|e| e.msg.contains(expected.as_str())),
                            "wrong parse error on {:?}: got {:?}, expected '{}'",
                            case.src,
                            errs.iter().map(|e| &e.msg).collect::<Vec<_>>(),
                            expected
                        );
                        continue;
                    }
                    None => panic!("parse error on {:?}: {:?}", case.src, errs.iter().map(|e| &e.msg).collect::<Vec<_>>()),
                }
            }
            // folded before running as production does
            edge::code::constant_fold(&mut chunk, mini::FRONT.policy);

            let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
            vm.input_buffer = case.input.clone();
            hook_sends(&mut vm, &case);
            for evt in &case.events { vm.push_event(evt).expect("push_event"); }
            let expects_input_error = case.input.is_empty() && (case.src.contains("input(") || case.src.contains("input ("));

            let run = drive(&mut vm, &case.interactive_events);
            if crate::common::cut_short(&run) { continue; }
            match run {
                Ok(_) => {
                    assert!(!expects_input_error, "expected input() to error under strict mode for: {:?}", case.src);
                    assert!(case.error.is_none(), "expected error {:?} on {:?}, the run ended cleanly", case.error, case.src);
                    assert_eq!(vm.output, case.output, "output mismatch on: {:?}", case.src);
                    if let Some(want) = &case.sends { assert_eq!(&sent(), want, "sends mismatch on: {:?}", case.src); }
                }
                Err(e) => match &case.error {
                    Some(expected) => assert!(e.to_string().contains(expected.as_str()), "wrong error on {:?}: got '{}', expected '{}'", case.src, e, expected),
                    None if expects_input_error => assert!(
                        e.to_string().contains("EOFError"),
                        "expected input EOFError under strict mode for: {:?}, got: {}",
                        case.src, e
                    ),
                    None => panic!("VM error on {:?}: {}", case.src, e),
                }
            }
        }
    }

    /* an error the host delivers into a parked call renders like one the call raised */
    mod host_errors {
        use edge::code::SSAChunk;
        use edge::embed::VM;
        use edge::embed as snapshot;
        use edge::value::{Limits, SchedulerStatus, VmErr};

        use crate::common::{fail, lex, parser, test_native, TestResolver};

        // compiles `src` against a module `m` whose `host_defer` always defers to the host
        fn compile(src: &str) -> SSAChunk {
            let resolver = TestResolver::new().with_native("m", vec![test_native("host_defer").unwrap()]).with_alias("m", "m");
            let (tokens, _) = lex(src);
            let (mut chunk, errs) = parser(src, tokens.into_iter(), Box::new(resolver)).parse();
            assert!(errs.is_empty(), "parse errors on {src:?}: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
            edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
            chunk
        }

        fn render(vm: &VM, e: &VmErr, src: &str) -> String {
            vm.render_error(e, src, Some("main.mini"))
        }

        // runs `src` answering every deferred call with `error` for the output and any traceback
        fn run(src: &str, error: &str) -> (Vec<String>, Option<String>) {
            let chunk = compile(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            let mut next_id = 0u64;
            loop {
                match vm.run() {
                    Ok(_) => return (vm.output.clone(), None),
                    Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)) => {
                        assert!(fail(&mut vm, next_id, error), "no call parked on id {next_id}");
                        next_id += 1;
                    }
                    Err(e) => return (vm.output.clone(), Some(render(&vm, &e, src))),
                }
            }
        }

        // the location lines of a traceback which must not depend on how the error arrived
        fn locations(tb: &str) -> Vec<&str> {
            tb.lines().filter(|l| l.contains("-->") || l.starts_with("note:") || l.starts_with("error:")).collect()
        }

        fn traceback(src: &str) -> String {
            let (_, tb) = run(src, "RuntimeError: boom");
            tb.unwrap_or_else(|| panic!("expected an uncaught error from {src:?}"))
        }

        #[test]
        fn a_recursion_through_sections_notes_its_levels_as_frames_do() {
            // the same lines both ways and a `try` keeps the recursion in frames
            let laid = |stop: &str, body: [&str; 2]| format!("fn f(n, stop):\n    if n == stop:\n        return 1 ~/ (stop - 70)\n    {}\n    {}\n    return f(x + 1, stop) + 1\nfn run():\n    for i in range(4):\n        f(0, 60)\n    f(0, {stop})\nrun()\n", body[0], body[1]);
            for stop in ["70", "-1"] {
                let (sections, frames) = (traceback(&laid(stop, ["x = n", "y = 0"])), traceback(&laid(stop, ["try: x = n", "except ValueError: y = 0"])));
                assert_eq!(locations(&sections), locations(&frames), "{sections}\n=====\n{frames}");
            }
            let mutual = |body: [&str; 2]| format!("fn even(n):\n    if n == 0:\n        return 1 ~/ n\n    {}\n    {}\n    return odd(x - 1)\nfn odd(n):\n    x = n\n    return even(x - 1)\nfor i in range(8):\n    try:\n        even(40 + i * 2)\n    except ZeroDivisionError:\n        pass\neven(30)\n", body[0], body[1]);
            let (sections, frames) = (traceback(&mutual(["x = n", "y = 0"])), traceback(&mutual(["try: x = n", "except ValueError: y = 0"])));
            assert_eq!(locations(&sections), locations(&frames), "{sections}");
        }

        #[test]
        fn uncaught_at_top_level_points_at_the_call() {
            let tb = traceback("from m import host_defer\nprint('before')\nhost_defer()\nprint('after')\n");
            assert!(tb.contains("RuntimeError: boom"), "{tb}");
            assert!(tb.contains("--> main.mini:3:1"), "{tb}");
            assert!(tb.contains("3 | host_defer()"), "{tb}");
        }

        #[test]
        fn uncaught_in_a_function_keeps_the_called_from_note() {
            let tb = traceback("from m import host_defer\nfn helper():\n    x = 1\n    host_defer()\nhelper()\n");
            assert!(tb.contains("--> main.mini:4:5"), "{tb}");
            assert!(tb.contains("note: called from helper()"), "{tb}");
            assert!(tb.contains(":5:7"), "{tb}");
        }

        #[test]
        fn a_key_that_fails_notes_its_call_as_a_called_function_does() {
            for call in ["sorted(xs, key=bad)", "min(xs, key=bad)", "max(xs, key=bad)", "list(map(bad, xs))"] {
                let tb = traceback(&format!("fn bad(x):\n    return 1 ~/ (x - 3)\nxs = [1, 2, 3, 4]\nprint({call})\n"));
                assert!(tb.contains("--> main.mini:2:5"), "{tb}");
                assert!(tb.contains("note: called from bad()"), "{tb}");
                assert!(tb.contains("--> main.mini:4:"), "{tb}");
            }
        }

        #[test]
        fn caught_error_leaves_no_stale_position() {
            let src = "from m import host_defer\ntry:\n    host_defer()\nexcept RuntimeError as e:\n    print('caught', e)\nx = 1 / 0\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(out, vec!["caught boom"]);
            let tb = tb.expect("the later ZeroDivisionError escapes");
            assert!(tb.contains("ZeroDivisionError"), "{tb}");
            assert!(tb.contains("--> main.mini:6:1"), "{tb}");
        }

        #[test]
        fn a_callers_handler_catches_an_error_raised_inside_a_resumed_helper() {
            let src = "from m import host_defer\nfn helper():\n    host_defer()\n    print('helper continued')\ntry:\n    helper()\nexcept RuntimeError:\n    print('caught in caller')\nprint('end')\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["caught in caller", "end"]);
        }

        #[test]
        fn a_host_call_cannot_wait_inside_a_coroutine() {
            let src = "from m import host_defer\nasync fn child():\n    host_defer()\ntry:\n    child().send(nil)\nexcept RuntimeError as e:\n    print(e)\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["receive() and host calls cannot wait inside a generator or a coroutine"]);
        }

        #[test]
        fn nested_helpers_render_like_an_error_raised_without_suspending() {
            let layout = |stmt: &str| format!("from m import host_defer\nfn inner():\n    {stmt}\nfn outer():\n    inner()\nouter()\n");
            let delivered = traceback(&layout("host_defer()"));
            let raised = traceback(&layout("raise RuntimeError('boom')"));
            assert_eq!(locations(&delivered), locations(&raised), "\n{delivered}\n{raised}");
            assert!(delivered.contains("note: called from inner()") && delivered.contains("note: called from outer()"), "{delivered}");
        }

        #[test]
        fn an_ordinary_error_after_a_helper_resumed_reaches_the_callers_handler() {
            let src = "from m import host_defer\nfn helper():\n    try:\n        host_defer()\n    except RuntimeError:\n        pass\n    raise ValueError('x')\ntry:\n    helper()\nexcept ValueError:\n    print('caught')\nprint('end')\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["caught", "end"]);
        }

        #[test]
        fn an_ordinary_error_after_a_helper_resumed_keeps_its_line_and_note() {
            let body = |stmt: &str| format!("from m import host_defer\nfn helper():\n    try:\n        {stmt}\n    except RuntimeError:\n        pass\n    x = 1 / 0\nhelper()\n");
            let resumed = traceback(&body("host_defer()"));
            let direct = traceback(&body("pass"));
            assert!(resumed.contains("--> main.mini:7:5"), "{resumed}");
            assert_eq!(locations(&resumed), locations(&direct), "\n{resumed}\n{direct}");
        }

        #[test]
        fn a_pending_raise_survives_a_snapshot() {
            let src = "from m import host_defer\nprint('before')\nhost_defer()\nprint('after')\n";
            let chunk = compile(src);
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingHostCall))));
            assert!(fail(&mut vm, 0, "RuntimeError: boom"));
            let blob = snapshot::save(&vm, src, "").expect("save");
            let mut restored = crate::common::boot(&chunk, Limits::sandbox());
            snapshot::restore(&mut restored, &blob).expect("restore");
            let e = restored.run().expect_err("the delivered error escapes after restore");
            let tb = render(&restored, &e, src);
            assert!(tb.contains("RuntimeError: boom") && tb.contains("--> main.mini:3:1"), "{tb}");
            assert!(!restored.output.iter().any(|l| l == "after"), "{:?}", restored.output);
        }

        // a builtin callback hands its result back so a wait inside it raises there
        #[test]
        fn a_wait_inside_a_builtins_callback_raises_there() {
            // `map` over a named list loops in a section of `run` and otherwise in frames
            for (call, it) in ["sorted(range(40), key=k)", "max(range(40), key=k)", "[v for v in map(k, range(40))]", "0"].into_iter().flat_map(|c| [(c, "range(40)"), (c, "xs")]) {
                let src = format!("from m import host_defer\nfn wait():\n    host_defer()\nfn k(x):\n    if x == 30:\n        wait()\n    return x\nxs = list(range(40))\nfn run():\n    t = 0\n    for v in map(k, {it}):\n        t += v\n    return t\ntry:\n    print({call})\nexcept RuntimeError as e:\n    print('caught', e)\ntry:\n    run()\nexcept RuntimeError as e:\n    print('caught', e)\nprint('end')\n");
                let (out, tb) = run(&src, "RuntimeError: boom");
                let caught = "caught receive() and host calls cannot wait inside a callback a builtin runs";
                let first = if call == "0" { "0" } else { caught };
                assert_eq!(out, vec![first, caught, "end"], "{call}: {tb:?}");
            }
        }

        // a lambda's error points into its body and its frame reads as the anonymous name
        #[test]
        fn a_lambda_is_named_and_points_into_its_body() {
            let tb = traceback("rows = [('a', '3'), ('b', 'x')]\nprint(sorted(rows, key=lambda r: int(r[1])))\n");
            assert!(tb.contains("--> main.mini:2:34") && tb.contains("note: called from <lambda>()"), "{tb}");
            let tb = traceback("f = lambda s: int(s)\nx = 1\nprint(f('y'))\n");
            assert!(tb.contains("--> main.mini:1:15") && tb.contains("note: called from <lambda>()") && tb.contains("--> main.mini:3:8"), "{tb}");
        }
    }
    // scanning or copying builtins pay for what they read so loops cannot outrun the budget
    #[test]
    fn native_work_pays_the_budget() {
        use edge::value::Limits;
        // tenfold smaller sizes keep the work over the budget and the bare loop under
        let (n, k, ops) = if cfg!(miri) { (1_000, 100, 20_000) } else { (10_000, 1_000, 200_000) };
        let h = n / 2;
        for src in [
            format!("s = 'ab' * {h}\nfor i in range({k}):\n    n = s.count('b')"),
            format!("l = list(range({n}))\nfor i in range({k}):\n    t = -1 in l"),
            format!("a = [list(range({n}))]\nb = [list(range({n}))]\nfor i in range({k}):\n    t = a == b"),
            format!("a = 'x' * {n}\nb = 'x' * {n}\nfor i in range({k}):\n    t = a == b"),
            format!("l = list(range({n}))\nfor i in range({k}):\n    t = sum(l)"),
            format!("d = dict.fromkeys(range({n}))\nfor i in range({k}):\n    e = d.copy()"),
            format!("k = 'x' * {n}\nd = {{}}\nfor i in range({k}):\n    d[k] = i"),
            format!("s = 'x'\nfor i in range({k}):\n    t = s.rjust({n})"),
            format!("l = list(range({n}))\nfor i in range({k}):\n    l.insert(0, i)\n    l.pop(0)"),
        ] {
            let (chunk, _) = crate::common::parse(&src);
            let mut vm = crate::common::boot(&chunk, Limits { ops, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{src}");
        }
        // the same work written without a loop pays line by line
        for (setup, line) in [(format!("s = 'ab' * {h}"), "n = s.count('b')"), (format!("a = 'x' * {n}\nb = 'x' * {n}"), "t = a == b"), (format!("l = list(range({n}))"), "t = -1 in l"), (format!("k = 'x' * {n}\nd = {{}}"), "d[k] = 1")] {
            let src = format!("{setup}\n{}", format!("{line}\n").repeat(k));
            let (chunk, _) = crate::common::parse(&src);
            let mut vm = crate::common::boot(&chunk, Limits { ops, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{line}");
        }
    }
    // a generator resumed in its window pays for its loop and handlers within the budget
    #[test]
    fn a_resumed_generator_pays_within_the_budget() {
        use edge::value::Limits;
        let src = "fn gi(n):\n    for i in range(n):\n        yield i\nfn outer(n):\n    for v in gi(n):\n        try:\n            yield v * 2\n        finally:\n            pass\nt = 0\nfor v in outer(30000):\n    t += v\nprint(t)\n";
        let (chunk, _) = crate::common::parse(src);
        for ops in 1..=80 {
            let mut vm = crate::common::boot(&chunk, Limits { ops, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{ops}");
        }
    }
    // a class asked for while native work waits past the budget raises dropping its floor
    #[test]
    fn a_class_whose_step_runs_out_of_budget_raises() {
        use edge::value::Limits;
        let src = "class O:\n    pass\nfn f():\n    class P:\n        pass\n    return P\nprint(f())\n";
        let (chunk, _) = crate::common::parse(src);
        for owed in [1_000, 70_000, 1_000_000] {
            let mut vm = crate::common::boot(&chunk, Limits { ops: 500, ..Limits::sandbox() });
            vm.heap_mut().spend(owed);
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{owed}");
        }
    }
    // a long pipe reads in linear time so each line costs what the first did
    #[test]
    #[cfg_attr(miri, ignore)]
    fn a_long_input_reads_in_linear_time() {
        let src = "t = 0\nfor _ in range(400000):\n    t += int(input())\nprint(t)\n";
        let (chunk, _) = crate::common::parse(src);
        let mut vm = crate::common::boot(&chunk, edge::value::Limits::sandbox());
        vm.input_buffer = (0..400_000).map(|i| (i % 10).to_string()).collect();
        let start = std::time::Instant::now();
        vm.run().unwrap();
        assert_eq!(vm.output, ["1800000"]);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    }
    // reads and cuts far into a text past ascii pay for the bytes they walk
    #[test]
    #[cfg_attr(miri, ignore)]
    fn far_reads_of_a_text_past_ascii_pay_for_the_bytes_they_walk() {
        use edge::value::Limits;
        for line in ["c = t[i * 104729 % n]", "c = t[-1 - i * 104729 % n]", "c = t[i * 104729 % n:i * 104729 % n + 2]", "c = t[i * 104729 % n::n]", "c = u[n + 2 + i]", "c = u[-n - 3 - i]"] {
            let src = format!("n = 300000\nt = 'é' * n\nu = 'é' + 'x' * n\nfor i in range(2000):\n    try:\n        {line}\n    except IndexError:\n        c = 0\n");
            let (chunk, _) = crate::common::parse(&src);
            let mut vm = crate::common::boot(&chunk, Limits { ops: 400_000, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{line}");
        }
        // each index in turn walks one character on from the last read
        let src = "n = 300000\nt = 'é' * n\nc = 0\nfor i in range(n):\n    if t[i] == 'é':\n        c += 1\nprint(c)\n";
        let (chunk, _) = crate::common::parse(src);
        let mut vm = crate::common::boot(&chunk, Limits { ops: 10_000_000, ..Limits::sandbox() });
        let start = std::time::Instant::now();
        vm.run().unwrap();
        assert_eq!(vm.output, ["300000"]);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    }
    // names sharing one hash pay for each probe of the tables that intern them
    #[test]
    #[cfg_attr(miri, ignore)]
    fn names_sharing_one_hash_pay_for_each_probe() {
        use edge::value::{Limits, hash_str};
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let inv = (0..6).fold(K, |x, _| x.wrapping_mul(2u64.wrapping_sub(K.wrapping_mul(x))));
        assert_eq!(K.wrapping_mul(inv), 1);
        let (mut names, mut seed) = (Vec::new(), 7u64);
        // a second block undoes whatever the first did so every name ends on one hash
        while names.len() < 1500 {
            seed = seed.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(1);
            let first = u64::from_le_bytes(core::array::from_fn(|k| b'a' + (seed >> (k * 4 + 20) & 15) as u8));
            let second = 12345u64.wrapping_mul(inv) ^ (K.rotate_left(5) ^ first).wrapping_mul(K).rotate_left(5);
            let text = [first.to_le_bytes(), second.to_le_bytes()].concat();
            if text[8..].iter().all(|b| (b' '..=b'~').contains(b) && *b != b'"' && *b != b'\\') && let Ok(s) = String::from_utf8(text) && !names.contains(&s) { names.push(s); }
        }
        assert!(names.iter().all(|s| hash_str(s) == hash_str(&names[0])));
        let plain: Vec<String> = (0..1500).map(|i| format!("plain name {i:05}")).collect();
        for (keys, shared) in [(&names, true), (&plain, false)] {
            let src = format!("class O:\n    pass\no = O()\nfor k in {keys:?}:\n    setattr(o, k, 1)\nprint(getattr(o, {:?}))\n", keys[1499]);
            let (chunk, _) = crate::common::parse(&src);
            let mut vm = crate::common::boot(&chunk, Limits { ops: 400_000, ..Limits::sandbox() });
            assert_eq!(matches!(vm.run(), Err(VmErr::Budget)), shared, "{}", keys[0]);
            // a type object made by such a name pays for the probes of its table
            let mut vm = crate::common::boot(&chunk, Limits::sandbox());
            for k in keys.iter() { vm.heap_mut().type_object(k).unwrap(); }
            let work = vm.heap().take_work();
            assert_eq!(work > 500_000, shared, "{work}");
        }
    }
    // emptying a set by pop and a map by popitem reads each removed entry once
    #[test]
    #[cfg_attr(miri, ignore)]
    fn emptying_a_set_or_a_map_from_one_end_takes_linear_time() {
        let src = "s = set(range(1000000))\nd = dict.fromkeys(range(1000000))\nk = 0\nwhile len(s) > 0:\n    s.pop()\n    d.popitem()\n    k += 1\nprint(k, len(d))\n";
        let (chunk, _) = crate::common::parse(src);
        let mut vm = crate::common::boot(&chunk, edge::value::Limits::sandbox());
        let start = std::time::Instant::now();
        vm.run().unwrap();
        assert_eq!(vm.output, ["1000000 0"]);
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
    }
}
