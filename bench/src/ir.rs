/* The optimized LLVM module of `edge build --emit-llvm`, read as text. bench/llvm and cli/tests/llvm_check.rs share it. */

/* Each function whose name starts with `prefix`, from its define line to its closing brace. */
pub fn bodies<'a>(ir: &'a str, prefix: &str) -> Vec<&'a str> {
    let at = format!("@{prefix}");
    ir.split("\ndefine ").skip(1).map(|f| &f[..f.find("\n}").unwrap_or(f.len())])
        .filter(|f| f.lines().next().is_some_and(|l| l.split_whitespace().any(|w| w.starts_with(&at)))).collect()
}

/* The calls in `body` to the function named `callee`. */
pub fn calls(body: &str, callee: &str) -> usize {
    let at = format!("@{callee}(");
    body.lines().skip(1).filter(|l| (l.contains("call ") || l.contains("invoke ")) && l.contains(&at)).count()
}

/* The checks left in `body` of the kind the module's own tests named `prefix` make (`is_` for the tag tests, `fits_int`
   for the int range). Their wide constants, as the same O3 left them, are the pattern, so nothing of Val's encoding is
   written here: an icmp counts when it takes one of them, or a value masked, shifted or offset by one. */
pub fn checks(ir: &str, body: &str, prefix: &str) -> usize {
    let wide = |l: &str| l.split([' ', ',']).filter_map(|w| w.parse::<i64>().ok()).filter(|c| c.unsigned_abs() >> 32 != 0).collect::<Vec<_>>();
    let pattern: Vec<i64> = bodies(ir, prefix).iter().flat_map(|f| f.lines().skip(1).flat_map(wide)).collect();
    let takes = |l: &str| wide(l).iter().any(|c| pattern.contains(c));
    let masked: Vec<&str> = body.lines().filter(|l| !l.contains("icmp") && takes(l)).filter_map(|l| l.trim().split(" = ").next()).collect();
    body.lines().filter(|l| l.contains(" icmp ") && (takes(l) || l.split([' ', ',']).any(|w| masked.contains(&w)))).count()
}
