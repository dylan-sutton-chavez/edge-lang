/* The optimized LLVM module of `edge build --emit-llvm`, read as text. bench/llvm and cli/tests/llvm_check.rs share it. */

/* Each function whose name starts with `prefix`, from its define line to its closing brace. */
pub fn bodies<'a>(ir: &'a str, prefix: &str) -> Vec<&'a str> {
    let at = format!("@{prefix}");
    ir.split("\ndefine ").skip(1).map(|f| &f[..f.find("\n}").unwrap_or(f.len())])
        .filter(|f| f.lines().next().is_some_and(|l| l.split_whitespace().any(|w| w.starts_with(&at)))).collect()
}

/* The callee of each call in `body`, `@name` or, for a call through a pointer, `%value`. */
fn callees(body: &str) -> Vec<&str> {
    body.lines().skip(1).filter_map(|l| l.split_once("call ").or_else(|| l.split_once("invoke ")))
        .filter_map(|(_, after)| after.split('(').find_map(|part| part.rsplit(' ').next().filter(|t| t.starts_with(['@', '%'])))).collect()
}

/* The calls in `body` to the function named `callee`. */
pub fn calls(body: &str, callee: &str) -> usize {
    callees(body).iter().filter(|c| c.strip_prefix('@') == Some(callee)).count()
}

/* The calls in `body` that may reach `target` through the module's call graph, a call through a pointer counting as one. */
pub fn reaching(ir: &str, body: &str, target: &str) -> usize {
    let graph: Vec<(&str, Vec<&str>)> = bodies(ir, "").into_iter().filter_map(|f| Some((f.split('@').nth(1)?.split('(').next()?, callees(f)))).collect();
    let mut reach = std::collections::HashSet::from([target]);
    let joins = |reach: &std::collections::HashSet<&str>, c: &&str| c.starts_with('%') || reach.contains(&c[1..]);
    // One pass per level of the call graph, every function found in it joining at once.
    loop {
        let more: Vec<&str> = graph.iter().filter(|(f, c)| !reach.contains(f) && c.iter().any(|c| joins(&reach, c))).map(|(f, _)| *f).collect();
        if more.is_empty() { break; }
        reach.extend(more);
    }
    callees(body).iter().filter(|c| joins(&reach, c)).count()
}

/* The checks left in `body` of the kind the module's own tests named `prefix` make (`is_` for the tag tests, `fits_int`
   for the int range). Their wide constants, as the same O3 left them, are the pattern, so nothing of Val's encoding is
   written here: an icmp counts when it takes one of them, or a value masked, shifted or offset by one. */
pub fn checks(ir: &str, body: &str, prefix: &str) -> usize {
    let wide = |l: &str| l.split([' ', ',']).filter_map(|w| w.parse::<i64>().ok()).filter(|c| c.unsigned_abs() >> 32 != 0).collect::<Vec<_>>();
    let pattern: Vec<i64> = bodies(ir, prefix).iter().flat_map(|f| f.lines().skip(1).flat_map(wide)).collect();
    // A module without the tests would find no check anywhere, so it is refused instead.
    assert!(!pattern.is_empty(), "the module keeps no {prefix} test to read its checks from, --emit-llvm must keep them in @llvm.used");
    let takes = |l: &str| wide(l).iter().any(|c| pattern.contains(c));
    let masked: Vec<&str> = body.lines().filter(|l| !l.contains("icmp") && takes(l)).filter_map(|l| l.trim().split(" = ").next()).collect();
    body.lines().filter(|l| l.contains(" icmp ") && (takes(l) || l.split([' ', ',']).any(|w| masked.contains(&w)))).count()
}
