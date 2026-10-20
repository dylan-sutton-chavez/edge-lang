#[cfg(test)]
mod test {

    use compiler::lexer::lex;
    use compiler::parser::Parser;
    use compiler::vm::VM;
    use compiler::vm::types::{SchedulerStatus, VmErr};

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
        // Events pushed one-at-a-time after each PendingEvent yield (host-resume path).
        #[serde(default)]
        interactive_events: Vec<String>,
        // Present installs a scheduler hook, the (group, body) pairs send() handed over.
        #[serde(default)]
        sends: Option<Vec<(String, String)>>,
        // The line the error points at, counted from 1.
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

    // Installs the recording hook when the case expects sends, clearing what an earlier case left.
    fn hook_sends(vm: &mut VM, case: &Case) {
        SENT.with(|s| s.borrow_mut().clear());
        if case.sends.is_some() { vm.send_hook = Some(record_send); }
    }

    fn sent() -> Vec<(String, String)> {
        SENT.with(|s| s.borrow().clone())
    }

    // Resume on each PendingEvent by pushing the next interactive_events entry.
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

    /* Two cases equal in every field test the same thing twice, so the suite keeps only the first. */
    #[test]
    fn no_duplicate_cases() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!("cases/vm.json")).expect("invalid JSON");
        let mut seen = std::collections::HashMap::new();
        let repeated: Vec<String> = cases.iter().enumerate()
            .filter_map(|(i, case)| seen.insert(case.to_string(), i).map(|first| format!("case {i} repeats case {first}, {}", case["src"])))
            .collect();
        assert!(repeated.is_empty(), "{} duplicate case(s):\n{}", repeated.len(), repeated.join("\n"));
    }

    /* Runs vm.json cases under sandbox limits testing budget heap depth guards verifying runaway allocation recursion materialization produce MemoryError RecursionError */
    #[test]
    fn test_cases() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("cases/vm.json")).expect("invalid JSON");

        let mut failures: Vec<String> = Vec::new();
        for case in crate::common::slice(cases) {
            let (tokens, lex_errs) = lex(&case.src);
            // Skip cases whose expected error is already raised by the lexer.
            if !lex_errs.is_empty()
                && let Some(expected) = &case.error
                && lex_errs.iter().any(|e| e.msg.contains(expected.as_str()))
            {
                continue;
            }
            let (mut chunk, _errors) = Parser::new(&case.src, tokens.into_iter()).parse();
            // Run the same fold pass production does (exports.rs), so fold-path bugs surface here.
            compiler::vm::optimizer::constant_fold(&mut chunk);
            let mut vm = VM::with_limits(&chunk, crate::common::corpus_limits());
            vm.input_buffer = case.input.clone();
            hook_sends(&mut vm, &case);
            for evt in &case.events { vm.push_event(evt).expect("push_event"); }
            let result = drive(&mut vm, &case.interactive_events);
            if crate::common::cut_short(&result) { continue; }

            // Collect every mismatch (not panic-on-first) so one run lists all regressions.
            match result {
                Ok(_obj) => {
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

    /* Reruns every vm.json case in strict_input mode (host-supplied buffer, reading past = RuntimeError) under `Limits::sandbox()` (see `test_cases` for why the bounded profile). Lex/parse errors are also asserted here. */
    #[test]
    fn strict_cases() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("cases/vm.json")).expect("invalid JSON");

        for case in crate::common::slice(cases) {
            let (tokens, lex_errs) = lex(&case.src);
            // Lex errors are surfaced as diagnostics, match against the expected error and move on.
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
            let (mut chunk, errs) = Parser::new(&case.src, tokens.into_iter()).parse();
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
            // Match production and fold before running.
            compiler::vm::optimizer::constant_fold(&mut chunk);

            let mut vm = VM::with_limits(&chunk, crate::common::corpus_limits());
            vm.input_buffer = case.input.clone();
            hook_sends(&mut vm, &case);
            for evt in &case.events { vm.push_event(evt).expect("push_event"); }
            let expects_input_error = case.input.is_empty() && (case.src.contains("input(") || case.src.contains("input ("));

            let run = drive(&mut vm, &case.interactive_events);
            if crate::common::cut_short(&run) { continue; }
            match run {
                Ok(_) => {
                    assert!(!expects_input_error, "expected input() to error under strict mode for: {:?}", case.src);
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

    /* An error the host delivers into a parked call renders like one the call raised itself. */
    mod host_errors {
        use compiler::lexer::lex;
        use compiler::parser::{Parser, SSAChunk};
        use compiler::vm::VM;
        use compiler::vm::snapshot;
        use compiler::vm::types::{Limits, SchedulerStatus, VmErr};

        use crate::common::{fail, test_native, TestResolver};

        // Compiles `src` against a module `m` whose `host_defer` always defers to the host.
        fn compile(src: &str) -> SSAChunk {
            let resolver = TestResolver::new().with_native("m", vec![test_native("host_defer").unwrap()]).with_alias("m", "m");
            let (tokens, _) = lex(src);
            let (mut chunk, errs) = Parser::with_resolver(src, tokens.into_iter(), Box::new(resolver)).parse();
            assert!(errs.is_empty(), "parse errors on {src:?}: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
            compiler::vm::optimizer::constant_fold(&mut chunk);
            chunk
        }

        fn render(vm: &VM, e: &VmErr, src: &str) -> String {
            vm.render_error(e, src, Some("main.py"))
        }

        // Runs `src`, answering every deferred call with `error`, returns the output and any rendered traceback.
        fn run(src: &str, error: &str) -> (Vec<String>, Option<String>) {
            let chunk = compile(src);
            let mut vm = VM::with_limits(&chunk, Limits::sandbox());
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

        // The location lines of a traceback, the part that must not depend on how the error arrived.
        fn locations(tb: &str) -> Vec<&str> {
            tb.lines().filter(|l| l.contains("-->") || l.starts_with("note:") || l.starts_with("error:")).collect()
        }

        fn traceback(src: &str) -> String {
            let (_, tb) = run(src, "RuntimeError: boom");
            tb.unwrap_or_else(|| panic!("expected an uncaught error from {src:?}"))
        }

        #[test]
        fn a_recursion_through_sections_notes_its_levels_as_frames_do() {
            // The same lines each way, a `try` keeping the recursion in frames, run warm first, one too deep ending at the same level, nothing pending.
            let laid = |stop: &str, body: [&str; 2]| format!("def f(n, stop):\n    if n == stop:\n        return 1 // (stop - 70)\n    {}\n    {}\n    return f(x + 1, stop) + 1\ndef run():\n    for i in range(4):\n        f(0, 60)\n    f(0, {stop})\nrun()\n", body[0], body[1]);
            for stop in ["70", "-1"] {
                let (sections, frames) = (traceback(&laid(stop, ["x = n", "y = 0"])), traceback(&laid(stop, ["try: x = n", "except ValueError: y = 0"])));
                assert_eq!(locations(&sections), locations(&frames), "{sections}\n=====\n{frames}");
            }
            let mutual = |body: [&str; 2]| format!("def even(n):\n    if n == 0:\n        return 1 // n\n    {}\n    {}\n    return odd(x - 1)\ndef odd(n):\n    x = n\n    return even(x - 1)\nfor i in range(8):\n    try:\n        even(40 + i * 2)\n    except ZeroDivisionError:\n        pass\neven(30)\n", body[0], body[1]);
            let (sections, frames) = (traceback(&mutual(["x = n", "y = 0"])), traceback(&mutual(["try: x = n", "except ValueError: y = 0"])));
            assert_eq!(locations(&sections), locations(&frames), "{sections}");
        }

        #[test]
        fn uncaught_at_top_level_points_at_the_call() {
            let tb = traceback("from m import host_defer\nprint('before')\nhost_defer()\nprint('after')\n");
            assert!(tb.contains("RuntimeError: boom"), "{tb}");
            assert!(tb.contains("--> main.py:3:1"), "{tb}");
            assert!(tb.contains("3 | host_defer()"), "{tb}");
        }

        #[test]
        fn uncaught_in_a_function_keeps_the_called_from_note() {
            let tb = traceback("from m import host_defer\ndef helper():\n    x = 1\n    host_defer()\nhelper()\n");
            assert!(tb.contains("--> main.py:4:5"), "{tb}");
            assert!(tb.contains("note: called from helper()"), "{tb}");
            assert!(tb.contains(":5:7"), "{tb}");
        }

        #[test]
        fn an_error_in_exec_text_points_inside_the_text_then_at_the_call() {
            let src = "def g():\n    exec('def h():\\n    return 1 / 0\\nh()')\ng()\n";
            let (_, tb) = run(src, "");
            let tb = tb.expect("the ZeroDivisionError escapes");
            assert!(tb.contains("--> <string>:2:5"), "{tb}");
            assert!(tb.contains("--> <string>:3:2"), "{tb}");
            assert!(tb.contains("note: called from exec()\n  --> main.py:2:9"), "{tb}");
            assert!(tb.contains("note: called from g()\n  --> main.py:3:2"), "{tb}");
        }

        #[test]
        fn caught_error_leaves_no_stale_position() {
            let src = "from m import host_defer\ntry:\n    host_defer()\nexcept RuntimeError as e:\n    print('caught', e)\nx = 1 / 0\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(out, vec!["caught boom"]);
            let tb = tb.expect("the later ZeroDivisionError escapes");
            assert!(tb.contains("ZeroDivisionError"), "{tb}");
            assert!(tb.contains("--> main.py:6:1"), "{tb}");
        }

        #[test]
        fn a_callers_handler_catches_an_error_raised_inside_a_resumed_helper() {
            let src = "from m import host_defer\ndef helper():\n    host_defer()\n    print('helper continued')\ntry:\n    helper()\nexcept RuntimeError:\n    print('caught in caller')\nprint('end')\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["caught in caller", "end"]);
        }

        #[test]
        fn a_host_call_cannot_wait_inside_a_coroutine() {
            let src = "from m import host_defer\nasync def child():\n    host_defer()\ntry:\n    child().send(None)\nexcept RuntimeError as e:\n    print(e)\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["receive() and host calls cannot wait inside a generator or a coroutine"]);
        }

        #[test]
        fn nested_helpers_render_like_an_error_raised_without_suspending() {
            let layout = |stmt: &str| format!("from m import host_defer\ndef inner():\n    {stmt}\ndef outer():\n    inner()\nouter()\n");
            let delivered = traceback(&layout("host_defer()"));
            let raised = traceback(&layout("raise RuntimeError('boom')"));
            assert_eq!(locations(&delivered), locations(&raised), "\n{delivered}\n{raised}");
            assert!(delivered.contains("note: called from inner()") && delivered.contains("note: called from outer()"), "{delivered}");
        }

        #[test]
        fn an_ordinary_error_after_a_helper_resumed_reaches_the_callers_handler() {
            let src = "from m import host_defer\ndef helper():\n    try:\n        host_defer()\n    except RuntimeError:\n        pass\n    raise ValueError('x')\ntry:\n    helper()\nexcept ValueError:\n    print('caught')\nprint('end')\n";
            let (out, tb) = run(src, "RuntimeError: boom");
            assert_eq!(tb, None);
            assert_eq!(out, vec!["caught", "end"]);
        }

        #[test]
        fn an_ordinary_error_after_a_helper_resumed_keeps_its_line_and_note() {
            let body = |stmt: &str| format!("from m import host_defer\ndef helper():\n    try:\n        {stmt}\n    except RuntimeError:\n        pass\n    x = 1 / 0\nhelper()\n");
            let resumed = traceback(&body("host_defer()"));
            let direct = traceback(&body("pass"));
            assert!(resumed.contains("--> main.py:7:5"), "{resumed}");
            assert_eq!(locations(&resumed), locations(&direct), "\n{resumed}\n{direct}");
        }

        #[test]
        fn a_pending_raise_survives_a_snapshot() {
            let src = "from m import host_defer\nprint('before')\nhost_defer()\nprint('after')\n";
            let chunk = compile(src);
            let mut vm = VM::with_limits(&chunk, Limits::sandbox());
            assert!(matches!(vm.run(), Err(VmErr::HostYield(SchedulerStatus::PendingHostCall))));
            assert!(fail(&mut vm, 0, "RuntimeError: boom"));
            let blob = snapshot::save(&vm, src).expect("save");
            let mut restored = VM::with_limits(&chunk, Limits::sandbox());
            snapshot::restore(&mut restored, &blob).expect("restore");
            let e = restored.run().expect_err("the delivered error escapes after restore");
            let tb = render(&restored, &e, src);
            assert!(tb.contains("RuntimeError: boom") && tb.contains("--> main.py:3:1"), "{tb}");
            assert!(!restored.output.iter().any(|l| l == "after"), "{:?}", restored.output);
        }

        #[test]
        fn a_raise_from_renders_its_cause_first() {
            let tb = traceback("try:\n    1 / 0\nexcept ZeroDivisionError as e:\n    raise ValueError('bad') from e\n");
            assert_eq!(locations(&tb), vec!["error: ZeroDivisionError: division by zero", "  --> main.py:2:5", "error: ValueError: bad", "  --> main.py:4:5"], "{tb}");
            assert!(tb.contains("\nThe above exception was the direct cause of the following exception:\n\n"), "{tb}");
        }

        #[test]
        fn an_error_inside_a_handler_renders_the_handled_one_first() {
            let tb = traceback("d = {}\ntry:\n    d['k']\nexcept KeyError:\n    1 / 0\n");
            assert_eq!(locations(&tb), vec!["error: KeyError: 'k'", "  --> main.py:3:5", "error: ZeroDivisionError: division by zero", "  --> main.py:5:5"], "{tb}");
            assert!(tb.contains("\nDuring handling of the above exception, another exception occurred:\n\n"), "{tb}");
        }

        #[test]
        fn a_raise_from_none_hides_the_handled_error() {
            let tb = traceback("try:\n    1 / 0\nexcept ZeroDivisionError:\n    raise ValueError('clean') from None\n");
            assert_eq!(locations(&tb), vec!["error: ValueError: clean", "  --> main.py:4:5"], "{tb}");
        }

        #[test]
        fn a_handled_error_is_no_context_once_its_handler_ends() {
            let tb = traceback("try:\n    1 / 0\nexcept ZeroDivisionError:\n    pass\nraise KeyError('later')\n");
            assert_eq!(locations(&tb), vec!["error: KeyError: 'later'", "  --> main.py:5:1"], "{tb}");
        }
        // A builtin's callback hands its result to the builtin, so a wait inside it raises where it waits, as one in a generator does.
        #[test]
        fn a_wait_inside_a_builtins_callback_raises_there() {
            // The loop over `map` runs as a section of `run` when its iterable is a list a name holds, as a frame of the loop otherwise.
            for (call, it) in ["sorted(range(40), key=k)", "max(range(40), key=k)", "[v for v in map(k, range(40))]", "0"].into_iter().flat_map(|c| [(c, "range(40)"), (c, "xs")]) {
                let src = format!("from m import host_defer\ndef wait():\n    host_defer()\ndef k(x):\n    if x == 30:\n        wait()\n    return x\nxs = list(range(40))\ndef run():\n    t = 0\n    for v in map(k, {it}):\n        t += v\n    return t\ntry:\n    print({call})\nexcept RuntimeError as e:\n    print('caught', e)\ntry:\n    run()\nexcept RuntimeError as e:\n    print('caught', e)\nprint('end')\n");
                let (out, tb) = run(&src, "RuntimeError: boom");
                let caught = "caught receive() and host calls cannot wait inside a callback a builtin runs";
                let first = if call == "0" { "0" } else { caught };
                assert_eq!(out, vec![first, caught, "end"], "{call}: {tb:?}");
            }
        }
    }
    // Builtins that scan or copy pay the op budget for what they read, so looping one cannot outrun it.
    #[test]
    fn native_work_pays_the_budget() {
        use compiler::vm::types::Limits;
        for src in [
            "s = 'ab' * 5000\nfor i in range(1000):\n    n = s.count('b')",
            "l = list(range(10000))\nfor i in range(1000):\n    t = -1 in l",
            "a = [list(range(10000))]\nb = [list(range(10000))]\nfor i in range(1000):\n    t = a == b",
            "a = 'x' * 10000\nb = 'x' * 10000\nfor i in range(1000):\n    t = a == b",
            "l = list(range(10000))\nfor i in range(1000):\n    t = sum(l)",
            "d = dict.fromkeys(range(10000))\nfor i in range(1000):\n    e = d.copy()",
            "k = 'x' * 10000\nd = {}\nfor i in range(1000):\n    d[k] = i",
            "s = 'x'\nfor i in range(1000):\n    t = s.rjust(10000)",
            "l = list(range(10000))\nfor i in range(1000):\n    l.insert(0, i)\n    l.pop(0)",
        ] {
            let (tokens, _) = lex(src);
            let (chunk, _) = Parser::new(src, tokens.into_iter()).parse();
            let mut vm = VM::with_limits(&chunk, Limits { ops: 200_000, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{src}");
        }
        // The same work written without a loop pays line by line.
        for (setup, line) in [("s = 'ab' * 5000", "n = s.count('b')"), ("a = 'x' * 10000\nb = 'x' * 10000", "t = a == b"), ("l = list(range(10000))", "t = -1 in l"), ("k = 'x' * 10000\nd = {}", "d[k] = 1")] {
            let src = format!("{setup}\n{}", format!("{line}\n").repeat(1000));
            let (tokens, _) = lex(&src);
            let (chunk, _) = Parser::new(&src, tokens.into_iter()).parse();
            let mut vm = VM::with_limits(&chunk, Limits { ops: 200_000, ..Limits::sandbox() });
            assert!(matches!(vm.run(), Err(VmErr::Budget)), "{line}");
        }
    }
}
