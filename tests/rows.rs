#[cfg(test)]
mod test {
    use edge::deps::NoopResolver;
    use edge::value::{SchedulerStatus, VmErr};

    use crate::common::{lex, parser};

    // what one run of a case gives back and whether a wait parked it
    struct Ran { output: Vec<String>, error: Option<String>, pos: Option<usize>, shown: Option<String>, parked: bool, stacked: u32 }

    fn accept_send(_group: &str, _body: &str) -> bool { true }

    // cases run over rows as a reference ROWS_REF kept and ROWS_OUT lists them
    #[test]
    fn rows_match_reference() {
        let cases = crate::cases::vm_cases(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        let (mut listed, mut stacked) = (String::new(), 0u32);
        let mut seen = serde_json::Map::new();
        for case in crate::common::slice(&cases) {
            let src = case["src"].as_str().unwrap_or("");
            let texts = |f: &str| case[f].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
            // events arrive one at a time as the bench hands them so each wait parks
            let (input, events) = (texts("input"), [texts("events"), texts("interactive_events")].concat());
            let (tokens, _) = lex(src);
            let (mut chunk, errors) = parser(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
            if !errors.is_empty() { continue; }
            edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
            let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
            vm.input_buffer = input.clone();
            vm.send_hook = Some(accept_send);
            let (mut next, mut parked) = (events.iter(), false);
            let error = loop {
                match vm.run() {
                    Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) => {
                        parked = true;
                        match next.next() { Some(e) => vm.push_event(e).expect("push_event"), None => break None }
                    }
                    Err(VmErr::HostYield(_)) => { parked = true; break None }
                    Err(e) => break Some(e),
                    Ok(_) => break None,
                }
            };
            let shown = error.as_ref().map(|e| vm.render_error(e, src, Some("main.mini")));
            let ran = Ran { output: vm.output.clone(), error: error.map(|e| e.to_string()), pos: vm.error_pos(), shown, parked, stacked: vm.stack_work().0 };
            stacked += ran.stacked;
            listed.push_str(&format!("{}\t{}\t{}\t{:?}\n", crate::cases::key(case), u8::from(ran.parked), chunk.instructions.len(), src.chars().take(60).collect::<String>()));
            seen.insert(crate::cases::key(case), serde_json::json!([ran.output, ran.error, ran.pos, ran.shown, ran.parked]));
        }
        if let Some(out) = std::env::var_os("ROWS_OUT") { std::fs::write(out, listed).expect("writing ROWS_OUT"); }
        // `ROWS_REF_OUT` keeps what each case gave and `ROWS_REF` checks against one kept before
        if let Some(out) = std::env::var_os("ROWS_REF_OUT") { std::fs::write(out, serde_json::to_string(&serde_json::Value::Object(seen.clone())).expect("a reference")).expect("writing ROWS_REF_OUT"); }
        if let Some(path) = std::env::var_os("ROWS_REF") {
            let kept: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(path).expect("reading ROWS_REF")).expect("a reference");
            let moved: Vec<String> = kept.iter().filter(|(k, v)| seen.get(*k).is_some_and(|now| now != *v)).map(|(k, v)| format!("{k}\n   kept {v}\n   now  {}", seen[k])).collect();
            assert!(moved.is_empty(), "{} case(s) differ from the reference:\n{}", moved.len(), moved.join("\n"));
        }
        // no lowering was refused for an op over rows
        assert_eq!(stacked, 0, "code lowered with an op over rows");
    }

    // every chunk a case compiles to with the functions and classes inside it
    fn each_chunk(c: &edge::code::SSAChunk, f: &mut impl FnMut(&edge::code::SSAChunk)) {
        f(c);
        for (_, body, _, _) in &c.functions { each_chunk(body, f); }
        for body in &c.classes { each_chunk(body, f); }
    }

    // the finish line holds once no lowering is refused for an op over rows
    #[test]
    fn finish_line() {
        let cases = crate::cases::vm_cases(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        let (mut folded, mut lowered, mut ops) = (0usize, 0u32, std::collections::BTreeMap::<String, u32>::new());
        for case in crate::common::slice(&cases) {
            let src = case["src"].as_str().unwrap_or("");
            let (tokens, _) = lex(src);
            let (mut chunk, errors) = parser(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
            if !errors.is_empty() { continue; }
            edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
            // the and and or the rows table leaves out never reach compiled code
            each_chunk(&chunk, &mut |c| folded += c.instructions.iter().filter(|i| matches!(i.opcode, edge::code::OpCode::And | edge::code::OpCode::Or)).count());
            let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
            vm.send_hook = Some(accept_send);
            let mut events = case["events"].as_array().into_iter().flatten().filter_map(|v| v.as_str());
            while let Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) = vm.run() {
                let Some(e) = events.next() else { break };
                let _ = vm.push_event(e);
            }
            let (stack, work) = vm.stack_work();
            lowered += stack;
            for (op, n) in work { *ops.entry(format!("{op:?}")).or_default() += n; }
        }
        assert_eq!(folded, 0, "compiled code holds an and or an or");
        assert_eq!(lowered, 0, "code lowered with an op over rows");
        assert!(ops.is_empty(), "lowerings refused for these ops over rows {ops:?}");
    }
}
