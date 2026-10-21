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

/* The module that holds `@edge.count` and prints it as the last line on stderr when the program exits, its `write` taking the target's `size_t`. */
pub fn runtime(target: &str) -> String {
    let size = if target.starts_with("wasm32") { "i32" } else { "i64" };
    format!(r#"@edge.count = global i64 0
@edge.count.label = private constant [14 x i8] c"instructions: "
@llvm.global_dtors = appending global [1 x {{ i32, ptr, ptr }}] [{{ i32, ptr, ptr }} {{ i32 0, ptr @edge.count.print, ptr null }}]
declare {size} @write(i32, ptr, {size})
define internal void @edge.count.print() {{
entry:
  %buf = alloca [24 x i8]
  %n0 = load i64, ptr @edge.count
  %end = getelementptr i8, ptr %buf, i64 23
  store i8 10, ptr %end
  br label %digit
digit:
  %n = phi i64 [ %n0, %entry ], [ %q, %digit ]
  %p = phi ptr [ %end, %entry ], [ %p1, %digit ]
  %q = udiv i64 %n, 10
  %r = urem i64 %n, 10
  %r8 = trunc i64 %r to i8
  %c = add i8 %r8, 48
  %p1 = getelementptr i8, ptr %p, i64 -1
  store i8 %c, ptr %p1
  %more = icmp ne i64 %q, 0
  br i1 %more, label %digit, label %done
done:
  %from = ptrtoint ptr %p1 to {size}
  %to = ptrtoint ptr %end to {size}
  %len = sub {size} %to, %from
  %size = add {size} %len, 1
  call {size} @write(i32 2, ptr @edge.count.label, {size} 14)
  call {size} @write(i32 2, ptr %p1, {size} %size)
  ret void
}}
"#)
}

