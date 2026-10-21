/* The optimized module with each block adding its own instruction count to `@edge.count` on entry, after its phis and pads, so the total is the IR instructions a run executed. */
pub fn instrument(ir: &str) -> String {
    let lines: Vec<&str> = ir.lines().collect();
    let mut out = String::with_capacity(ir.len() * 2);
    let (mut body, mut k) = (false, 0usize);
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at];
        let define = line.starts_with("define ") && line.ends_with('{');
        // A named entry block starts at its label, an unnamed one right after the `define`.
        let starts = define && !lines.get(at + 1).is_some_and(|l| is_label(l)) || body && is_label(line);
        body = (body || define) && line != "}";
        out.push_str(line);
        out.push('\n');
        at += 1;
        if !starts {
            continue;
        }
        // The block runs to its terminator, the next label or the function's end, `switch` cases being part of one instruction.
        let end = (at..lines.len()).find(|&i| is_label(lines[i]) || lines[i] == "}").unwrap_or(lines.len());
        let count = lines[at..end].iter().filter(|l| is_instruction(l)).count();
        let pads = lines[at..end].iter().take_while(|l| !is_instruction(l) || l.contains(" = phi ") || l.contains("landingpad") || l.contains("catchpad") || l.contains("cleanuppad")).count();
        for l in &lines[at..at + pads] {
            out.push_str(l);
            out.push('\n');
        }
        at += pads;
        out.push_str(&format!("  %edge.count.{k} = load i64, ptr @edge.count\n  %edge.count.{k}.n = add i64 %edge.count.{k}, {count}\n  store i64 %edge.count.{k}.n, ptr @edge.count\n"));
        k += 1;
    }
    out + "@edge.count = external global i64\n"
}

fn is_label(line: &str) -> bool {
    let head = line.split(';').next().unwrap_or("").trim_end();
    !line.starts_with(' ') && head.ends_with(':') && !head.contains(' ') || head.starts_with('"') && head.ends_with("\":")
}

fn is_instruction(line: &str) -> bool {
    line.starts_with("  ") && !line.starts_with("   ") && !line.starts_with("  ]") && !line[2..].starts_with(';')
}

/* The module that holds `@edge.count` and prints it as the last line on stderr when the program exits. */
pub fn runtime() -> &'static str {
    r#"@edge.count = global i64 0
@edge.count.text = private constant [20 x i8] c"instructions: %lld\0A\00"
@llvm.global_dtors = appending global [1 x { i32, ptr, ptr }] [{ i32, ptr, ptr } { i32 0, ptr @edge.count.print, ptr null }]
declare i32 @dprintf(i32, ptr, ...)
define internal void @edge.count.print() {
  %n = load i64, ptr @edge.count
  call i32 (i32, ptr, ...) @dprintf(i32 2, ptr @edge.count.text, i64 %n)
  ret void
}
"#
}
