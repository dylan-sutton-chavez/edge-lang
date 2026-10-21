/* An LLVM module read as text: the optimized program of `edge build --emit-llvm` and the VM's own module. bench/llvm,
   cli/tests/llvm_check.rs and native/safepoints.rs share it. */

/* Each function whose name starts with `prefix`, from its define line to its closing brace. */
pub fn bodies<'a>(ir: &'a str, prefix: &str) -> Vec<&'a str> {
    ir.split("\ndefine ").skip(1).map(|f| &f[..f.find("\n}").unwrap_or(f.len())])
        .filter(|f| f.lines().next().and_then(|l| l.find('@').map(|at| name(&l[at + 1..]))).is_some_and(|n| n.starts_with(prefix))).collect()
}

/* A symbol from the text after its `@`, quoted or plain, up to its argument list. */
fn name(s: &str) -> &str {
    match s.strip_prefix('"') { Some(q) => &q[..q.find('"').unwrap_or(q.len())], None => &s[..s.find(['(', ' ', ',']).unwrap_or(s.len())] }
}

/* The callee of each call in `body`, None for a call through a pointer; intrinsics reach nothing, so they are left out. */
fn callees(body: &str) -> Vec<Option<&str>> {
    body.lines().skip(1).filter_map(|l| {
        let at = l.find("call ").or_else(|| l.find("invoke "))?;
        let w = l[at..].split_whitespace().find(|w| (w.starts_with('@') || w.starts_with('%')) && w.contains('('))?;
        if w.starts_with("@llvm.") { None } else { Some(w.strip_prefix('@').map(name)) }
    }).collect()
}

/* The calls in `body` to the function named `callee`. */
pub fn calls(body: &str, callee: &str) -> usize {
    callees(body).iter().filter(|c| **c == Some(callee)).count()
}

/* The functions of `ir` that may reach `target` through calls, a call through a pointer reaching any function. */
pub fn reach<'a>(ir: &'a str, target: &str) -> Vec<&'a str> {
    let graph: Vec<(&str, Vec<Option<&str>>)> = bodies(ir, "").into_iter().filter_map(|f| Some((name(&f[f.find('@')? + 1..]), callees(f)))).collect();
    let mut may = std::collections::HashSet::new();
    // One pass per level of the call graph, every function found in it joining at once.
    loop {
        let more: Vec<&str> = graph.iter().filter(|(f, cs)| !may.contains(f) && cs.iter().any(|c| c.is_none_or(|c| c == target || may.contains(c)))).map(|(f, _)| *f).collect();
        if more.is_empty() { return may.into_iter().collect(); }
        may.extend(more);
    }
}

/* The calls in `body` that may reach `target`, a call through a pointer counting as one. */
pub fn reaching(ir: &str, body: &str, target: &str) -> usize {
    let may = reach(ir, target);
    callees(body).iter().filter(|c| c.is_none_or(|c| c == target || may.contains(&c))).count()
}

/* The checks left in `body` of the kind the VM's own tests named `prefix` make (`is_` for the tag tests, `fits_int` for
   the int range), read from `vm`, the VM module edge links, at the same O3. Their wide constants are the pattern, so
   nothing of Val's encoding is written here: an icmp counts when it takes one of them, or a value masked, shifted or
   offset by one. */
pub fn checks(vm: &str, body: &str, prefix: &str) -> usize {
    let wide = |l: &str| l.split([' ', ',']).filter_map(|w| w.parse::<i64>().ok()).filter(|c| c.unsigned_abs() >> 32 != 0).collect::<Vec<_>>();
    let pattern: Vec<i64> = bodies(vm, prefix).iter().flat_map(|f| f.lines().skip(1).flat_map(wide)).collect();
    // A module without the tests would find no check anywhere, so it is refused instead.
    assert!(!pattern.is_empty(), "the VM module has no {prefix} test to read its checks from");
    let takes = |l: &str| wide(l).iter().any(|c| pattern.contains(c));
    let masked: Vec<&str> = body.lines().filter(|l| !l.contains("icmp") && takes(l)).filter_map(|l| l.trim().split(" = ").next()).collect();
    body.lines().filter(|l| l.contains(" icmp ") && (takes(l) || l.split([' ', ',']).any(|w| masked.contains(&w)))).count()
}
