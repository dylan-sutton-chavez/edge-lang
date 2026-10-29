use std::path::Path;

use crate::common::slice;

/* the vm cases a corpus test walks and under Miri the sample tests/cases/miri.tsv names */
pub fn corpus() -> Vec<serde_json::Value> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if cfg!(miri) { crate::cases::miri_sample(root) } else { crate::cases::vm_cases(root) }
}

/* the slice EDGE_SLICE picks of `corpus` where Miri reads no other sampled case */
pub fn corpus_slice() -> Vec<serde_json::Value> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if cfg!(miri) { crate::cases::miri_cases(root, &slice(crate::cases::miri_lines(root)).collect::<Vec<_>>()) } else { slice(crate::cases::vm_cases(root)).collect() }
}

#[cfg(test)]
mod test {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use crate::cases::{key, miri_lines, miri_sample, vm_files};

    fn root() -> &'static Path { Path::new(env!("CARGO_MANIFEST_DIR")) }

    /* each line names a corpus case by key where its topic file holds it */
    #[test]
    fn the_sample_names_cases_of_the_corpus() {
        let files = vm_files(root());
        let at: HashMap<String, (&str, usize)> = files.iter().flat_map(|(f, cases)| cases.iter().enumerate().map(move |(i, c)| (key(c), (f.as_str(), i)))).collect();
        let lines = miri_lines(root());
        let (mut fixed, mut stale) = (String::from("key\tfile\tindex\n"), Vec::new());
        for (k, file, i) in &lines {
            let line = match at.get(k) {
                Some((f, j)) => format!("{k}\t{f}\t{j}"),
                // an edited case keeps its place under its new key
                None => match files.iter().find(|(f, _)| f == file).and_then(|(_, cases)| cases.get(*i)) {
                    Some(case) => format!("{}\t{file}\t{i}", key(case)),
                    None => { stale.push(format!("{k} names no case and {file} holds no case {i}")); continue }
                },
            };
            if line != format!("{k}\t{file}\t{i}") { stale.push(format!("{k}\t{file}\t{i} is now {line}")); }
            fixed.push_str(&line);
            fixed.push('\n');
        }
        if let Some(out) = std::env::var_os("MIRI_TSV_OUT") { std::fs::write(out, &fixed).expect("writing MIRI_TSV_OUT"); }
        assert!(stale.is_empty(), "tests/cases/miri.tsv no longer matches the corpus and MIRI_TSV_OUT=tests/cases/miri.tsv rewrites it\n{}", stale.join("\n"));
        let mut seen = HashSet::new();
        assert!(lines.iter().all(|l| seen.insert(&l.0)), "tests/cases/miri.tsv names a case twice");
        // the loader Miri runs reads each case as the whole parse does
        for ((k, file, i), case) in lines.iter().zip(miri_sample(root())) {
            let whole = files.iter().find(|(f, _)| f == file).map(|(_, cases)| &cases[*i]);
            assert_eq!(Some(&case), whole, "{k} read alone differs from {file} case {i}");
        }
    }

    /* the compiler opcodes `case` runs as test_cases runs it on the Miri budget */
    #[cfg(feature = "coverage")]
    fn ran(case: &serde_json::Value) -> Vec<usize> {
        use edge::code::SSAChunk;
        use edge::value::{Limits, SchedulerStatus, VmErr};
        let texts = |f: &str| case[f].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
        let (src, error) = (case["src"].as_str().unwrap_or(""), case["error"].as_str());
        let (tokens, lex_errs) = crate::common::lex(src);
        if let Some(e) = error && lex_errs.iter().any(|l| l.msg.contains(e)) { return Vec::new(); }
        let (mut chunk, errors) = crate::common::parser(src, tokens.into_iter(), Box::new(edge::deps::NoopResolver)).parse();
        if let Some(e) = error && errors.iter().any(|d| d.msg.contains(e)) { return Vec::new(); }
        edge::code::constant_fold(&mut chunk, mini::FRONT.policy);
        let mut vm = crate::common::boot(&chunk, Limits { ops: crate::common::MIRI_OPS, ..Limits::sandbox() });
        vm.input_buffer = texts("input");
        vm.send_hook = Some(|_, _| true);
        for e in texts("events") { vm.push_event(&e).expect("push_event"); }
        let mut waiting = texts("interactive_events").into_iter();
        while let Err(VmErr::HostYield(SchedulerStatus::PendingEvent)) = vm.run() {
            let Some(e) = waiting.next() else { break };
            vm.push_event(&e).expect("push_event");
        }
        fn each(c: &SSAChunk, f: &mut impl FnMut(&SSAChunk)) {
            f(c);
            for (_, body, _, _) in &c.functions { each(body, f); }
            for body in &c.classes { each(body, f); }
        }
        let mut seen = [false; 256];
        each(&chunk, &mut |c| for (ip, ins) in c.instructions.iter().enumerate() { if vm.ran(c, ip) { seen[ins.opcode as usize] = true; } });
        (0..256).filter(|&op| seen[op]).collect()
    }

    /* every opcode a corpus case runs some case of the sample runs too */
    #[cfg(feature = "coverage")]
    #[test]
    fn coverage_of_the_sample_holds_every_opcode_the_corpus_runs() {
        let sampled: HashSet<String> = miri_lines(root()).into_iter().map(|l| l.0).collect();
        let (mut first, mut held) = (HashMap::<usize, (std::time::Duration, String)>::new(), HashSet::new());
        for (file, cases) in vm_files(root()) {
            for (i, case) in cases.iter().enumerate() {
                let k = key(case);
                let start = std::time::Instant::now();
                // a run that panics covers nothing and a sampled one must never panic
                let Ok(ops) = std::panic::catch_unwind(|| ran(case)) else { assert!(!sampled.contains(&k), "sampled case {k} panics"); continue };
                let spent = start.elapsed();
                for op in ops {
                    if sampled.contains(&k) { held.insert(op); }
                    if first.get(&op).is_none_or(|(t, _)| spent < *t) { first.insert(op, (spent, format!("{k}\t{file}\t{i}"))); }
                }
            }
        }
        let mut missing: Vec<(&usize, &(std::time::Duration, String))> = first.iter().filter(|(op, _)| !held.contains(op)).collect();
        missing.sort();
        let lines: Vec<String> = missing.iter().map(|(op, (_, line))| format!("{line} for {:?}", edge::code::OpCode::ALL[**op])).collect();
        assert!(lines.is_empty(), "no case of tests/cases/miri.tsv runs these opcodes and the cheapest case to add for each follows\n{}", lines.join("\n"));
    }
}
