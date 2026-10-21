use compiler::code::{effect, Binding, BodyKind, OpCode as O, ParamKind, Program, SSAChunk, Value};
use compiler::types::{Known, State, Tags, Typed};
use compiler::value::Val;
use std::fmt::Write;

// The tag tests the IR checks read after O3, kept whole through internalize.
const TAGS: [&str; 7] = ["is_int", "is_float", "is_bool", "is_none", "is_heap", "is_undef", "fits_int"];

/* One body: its code, its VM index (0 the module, fi + 1 a function), the tree's index of it, its LLVM name, its
   parameters' slots (MAX for one a direct call cannot bind) and where each name lives. */
struct Body<'c> { chunk: &'c SSAChunk, at: usize, ty: Option<usize>, name: String, params: Vec<usize>, binds: Vec<Binding> }

/* The program as LLVM text: one function per Python function calling the VM's handler of each opcode in order with
   its operands constant, names and stack in allocas mem2reg lifts, and the entry that boots the VM on the source. */
pub fn module(src: &str, chunk: &SSAChunk, typed: Option<&Typed>, target: &str) -> String {
    let program = Program::new(chunk);
    let (mut bodies, mut work) = (Vec::new(), vec![chunk]);
    while let Some(c) = work.pop() {
        work.extend(c.functions.iter().map(|f| &f.1).chain(c.classes.iter().flat_map(|k| k.functions.iter().map(|f| &f.1))));
        let Some((kind, binds)) = program.bindings_of(c) else { continue };
        // Generators and class bodies run on the VM, a class's methods are functions like any other.
        let (at, params) = match kind {
            BodyKind::Module => (0, Vec::new()),
            BodyKind::Function(fi) if !c.is_generator => (fi + 1, program.params_of(fi).iter().map(|&(k, s)| if k == ParamKind::Normal { s } else { usize::MAX }).collect()),
            _ => continue,
        };
        let mut name = if at == 0 { "edge_main".to_string() } else { format!("py_{}", c.qualname) };
        if bodies.iter().any(|b: &Body| b.name == name) { name += &format!(".{at}"); }
        let ty = typed.and_then(|t| t.prog.bodies.iter().position(|p| core::ptr::eq(p.chunk, c)));
        bodies.push(Body { chunk: c, at, ty, name, params, binds: binds.to_vec() });
    }
    let (mut ir, mut ops) = (String::new(), Vec::new());
    for b in &bodies { function(&mut ir, &mut ops, b, &bodies, typed); }
    ops.sort();
    ops.dedup();
    for o in ops { let _ = writeln!(ir, "declare i64 @op_{o}(ptr, i64, i64, i64, i64, ptr, ptr, i32)"); }
    for t in TAGS { let _ = writeln!(ir, "declare i1 @{t}(i64)"); }
    let used: Vec<String> = TAGS.iter().map(|t| format!("ptr @{t}")).collect();
    let text: String = src.bytes().map(|c| format!("\\{c:02X}")).collect();
    let _ = write!(ir, r#"declare i1 @is_err(i64)
declare i32 @err_id(i64)
declare void @llvm.assume(i1)
declare ptr @edge_boot(ptr, i64, i32, ptr)
declare i32 @edge_finish(ptr, i64)
declare i64 @edge_returned(ptr)
declare i64 @edge_enter(ptr, i32, i64, ptr, i32)
declare void @edge_leave(ptr, i64)
declare i64 @edge_raise(ptr, i64, ptr)
@llvm.used = appending global [{} x ptr] [{}], section "llvm.metadata"
@edge.src = private constant [{} x i8] c"{text}"
attributes #0 = {{ alwaysinline }}
define i32 @{}(i32 %argc, ptr %argv) {{
  %rt = call ptr @edge_boot(ptr @edge.src, i64 {}, i32 %argc, ptr %argv)
  %bad = icmp eq ptr %rt, null
  br i1 %bad, label %out, label %go
out:
  ret i32 1
go:
  %r = call i64 @edge_main(ptr %rt)
  %code = call i32 @edge_finish(ptr %rt, i64 %r)
  ret i32 %code
}}
"#, TAGS.len(), used.join(", "), src.len(), super::entry(target), src.len());
    ir
}

/* The stack's depth before each instruction, None where no path reaches. */
fn depths(c: &SSAChunk) -> Vec<Option<usize>> {
    let (mut at, mut work) = (vec![None; c.instructions.len() + 1], vec![(0usize, 0usize)]);
    while let Some((ip, d)) = work.pop() {
        if ip >= c.instructions.len() || at[ip].is_some() { continue; }
        at[ip] = Some(d);
        let ins = c.instructions[ip];
        let (pops, pushes) = effect(&ins, c).map_or((0, 0), |(p, q)| (p as usize, q as usize));
        let t = ins.operand as usize;
        match ins.opcode {
            O::Jump => work.push((t, d)),
            O::ReturnValue | O::Raise | O::RaiseFrom => {}
            O::JumpIfFalse => work.extend([(t, d - 1), (ip + 1, d - 1)]),
            O::JumpIfFalseOrPop | O::JumpIfTrueOrPop => work.extend([(t, d), (ip + 1, d - 1)]),
            O::ForIter => work.extend([(t, d), (ip + 1, d + 1)]),
            // An `except` starts with its exception on the stack, a `finally` with nothing more.
            O::SetupExcept => work.extend([(t, d + 1), (ip + 1, d)]),
            O::SetupFinally => work.extend([(t, d), (ip + 1, d)]),
            _ => work.push((ip + 1, d + pushes - pops.min(d))),
        }
    }
    at
}

/* The states the tree saw at `ip` of body `b`, of every context. */
fn states<'t>(typed: Option<&'t Typed>, b: &Body, ip: usize) -> Vec<&'t State> {
    let (Some(t), Some(ty)) = (typed, b.ty) else { return Vec::new() };
    t.tree.ctxs.iter().filter(|c| c.body as usize == ty).flat_map(|c| c.seen.get(ip).into_iter().flatten()).collect()
}

fn function(ir: &mut String, ops: &mut Vec<String>, b: &Body, bodies: &[Body], typed: Option<&Typed>) {
    let (c, at) = (b.chunk, depths(b.chunk));
    let rest = c.instructions.iter().filter_map(|i| effect(i, c)).map(|(p, q)| p.max(q)).max().unwrap_or(0) as usize + 1;
    let deep = at.iter().flatten().max().copied().unwrap_or(0) + 4;
    let local = |slot: usize| b.at != 0 && matches!(b.binds.get(slot), Some(Binding::Local));
    let (undef, none, yes) = (Val::undef().raw() as i64, Val::none().raw() as i64, Val::bool(true).raw() as i64);
    let params: String = (0..b.params.len()).map(|k| format!(", i64 %p{k}")).collect();
    let callee = if b.at == 0 { "" } else { ", i64 %callee" };
    let _ = writeln!(ir, "define internal i64 @\"{}\"(ptr %rt{callee}{params}) !edge.block !{} {{\nentry:", b.name, b.at);
    for k in 0..deep { let _ = writeln!(ir, "  %s{k} = alloca i64"); }
    for k in (0..c.names.len()).filter(|&k| local(k)) { let _ = writeln!(ir, "  %n{k} = alloca i64\n  store i64 {undef}, ptr %n{k}"); }
    let _ = writeln!(ir, "  %rest = alloca [{rest} x i64]\n  %live = alloca [{} x i64]\n  %hp = alloca i32\n  %exc = alloca i64", c.names.len() + deep);
    if b.at != 0 {
        let n = b.params.len();
        let _ = writeln!(ir, "  %args = alloca [{} x i64]", n.max(1));
        for k in 0..n { let _ = writeln!(ir, "  %a{k} = getelementptr i64, ptr %args, i64 {k}\n  store i64 %p{k}, ptr %a{k}"); }
        let _ = writeln!(ir, "  %in = call i64 @edge_enter(ptr %rt, i32 {}, i64 %callee, ptr %args, i32 {n})\n  %bad = call i1 @is_err(i64 %in)\n  br i1 %bad, label %fail, label %bound\nfail:\n  ret i64 %in\nbound:", b.at - 1);
        for (k, &slot) in b.params.iter().enumerate().filter(|p| local(*p.1)) { let _ = writeln!(ir, "  store i64 %p{k}, ptr %n{slot}"); }
    }
    let _ = writeln!(ir, "  br label %i0");
    let dead = typed.zip(b.ty).map(|(t, ty)| t.dead(ty)).unwrap_or_default();
    let handlers: Vec<(usize, bool)> = c.instructions.iter().filter(|i| matches!(i.opcode, O::SetupExcept | O::SetupFinally)).map(|i| (i.operand as usize, i.opcode == O::SetupExcept)).collect();
    for (ip, ins) in c.instructions.iter().enumerate() {
        let _ = writeln!(ir, "i{ip}:");
        let Some(d) = at[ip].filter(|_| !dead.get(ip).copied().unwrap_or(false)) else { ir.push_str("  unreachable\n"); continue };
        let site = (b.at as i64) << 32 | (ip as i64) << 16 | ins.operand as i64;
        // The module's last return meets an empty stack and takes nothing.
        let (pops, pushes) = effect(ins, c).map_or((0, 0), |(p, q)| ((p as usize).min(d), q as usize));
        let (base, t, next, k) = (d - pops, ins.operand as usize, format!("i{}", ip + 1), ip);
        let seen = states(typed, b, ip);
        // A stack move reads its inputs and writes them back in their new order.
        let order: Option<&[usize]> = match ins.opcode { O::Dup => Some(&[0, 0]), O::Dup2 => Some(&[0, 1, 0, 1]), O::Swap => Some(&[1, 0]), O::Rot3 => Some(&[2, 0, 1]), O::PopTop => Some(&[]), _ => None };
        if let Some(order) = order {
            for j in 0..pops { let _ = writeln!(ir, "  %m{k}.{j} = load i64, ptr %s{}", base + j); }
            for (j, &from) in order.iter().enumerate() { let _ = writeln!(ir, "  store i64 %m{k}.{from}, ptr %s{}", base + j); }
            let _ = writeln!(ir, "  br label %{next}");
            continue;
        }
        let konst = match (ins.opcode, c.constants.get(t)) {
            (O::LoadConst, Some(Value::Int(i))) => Val::int_checked(*i).map(|v| v.raw()),
            (O::LoadConst, Some(Value::Float(f))) => Some(Val::float(*f).raw()),
            (O::LoadConst, Some(Value::Bool(v))) => Some(Val::bool(*v).raw()),
            (O::LoadConst, Some(Value::None)) => Some(Val::none().raw()),
            _ => None,
        };
        let done = match ins.opcode {
            O::Jump => format!("  br label %i{t}"),
            _ if konst.is_some() => format!("  store i64 {}, ptr %s{d}\n  br label %{next}", konst.unwrap_or(0) as i64),
            O::StoreName if local(t) => format!("  %v{k} = load i64, ptr %s{base}\n  store i64 %v{k}, ptr %n{t}\n  br label %{next}"),
            O::ReturnValue if b.at != 0 => format!("  %v{k} = load i64, ptr %s{base}\n  call void @edge_leave(ptr %rt, i64 %v{k})\n  ret i64 %v{k}"),
            O::ReturnValue => format!("  ret i64 {none}"),
            // A phi of locals takes its first source that is bound, as the VM's does.
            O::Phi if local(t) => match c.phi_map.get(ip).and_then(|&p| c.phi_sources.get(p)).map(|&(x, y)| (x as usize, y as usize)).filter(|&(x, y)| local(x) && local(y)) {
                Some((x, y)) => format!("  %pa{k} = load i64, ptr %n{x}\n  %pb{k} = load i64, ptr %n{y}\n  %pu{k} = call i1 @is_undef(i64 %pa{k})\n  %pv{k} = select i1 %pu{k}, i64 %pb{k}, i64 %pa{k}\n  store i64 %pv{k}, ptr %n{t}\n  br label %{next}"),
                None => format!("  br label %{next}"),
            },
            _ => String::new(),
        };
        if !done.is_empty() { let _ = writeln!(ir, "{done}"); continue; }
        // A bound local is its own value, an unbound one raises through the VM.
        if ins.opcode == O::LoadName && local(t) {
            let _ = writeln!(ir, "  %b{k} = load i64, ptr %n{t}\n  %u{k} = call i1 @is_undef(i64 %b{k})\n  store i64 %b{k}, ptr %s{d}\n  br i1 %u{k}, label %h{k}, label %{next}\nh{k}:");
        }
        let input: Vec<String> = (0..pops).map(|j| format!("%a{k}.{j}")).collect();
        for (j, v) in input.iter().enumerate() {
            let _ = writeln!(ir, "  {v} = load i64, ptr %s{}", base + j);
            assume(ir, typed, seen.iter().fold(0, |a, s| a | s.stack.get(base + j).map_or(!0, |e| e.ty.tags)), v);
        }
        for (j, v) in input.iter().enumerate().skip(3) { let _ = writeln!(ir, "  %r{k}.{j} = getelementptr i64, ptr %rest, i64 {}\n  store i64 {v}, ptr %r{k}.{j}", j - 3); }
        let abc: Vec<String> = (0..3).map(|j| input.get(j).cloned().unwrap_or(undef.to_string())).collect();
        // What a collection must see: every local and the stack below the inputs, the handler storing them only on its slow path.
        let live: Vec<String> = (0..c.names.len()).filter(|&s| local(s)).map(|s| format!("%n{s}")).chain((0..base).map(|j| format!("%s{j}"))).collect();
        for (j, l) in live.iter().enumerate() { let _ = writeln!(ir, "  %l{k}.{j} = load i64, ptr {l}\n  %lp{k}.{j} = getelementptr i64, ptr %live, i64 {j}\n  store i64 %l{k}.{j}, ptr %lp{k}.{j}"); }
        match known(&seen, base, bodies).filter(|f| ins.opcode == O::Call && t >> 8 == 0 && f.params.len() + 1 == pops && f.params.iter().all(|&s| s != usize::MAX)) {
            // A recursive call stays a call, any other is inlined so the block is the caller with its callees laid in it.
            Some(f) => { let _ = writeln!(ir, "  %v{k} = call i64 @\"{}\"(ptr %rt, {}){}", f.name, input.iter().map(|v| format!("i64 {v}")).collect::<Vec<_>>().join(", "), if f.at == b.at { "" } else { " #0" }); }
            None => {
                ops.push(format!("{:?}", ins.opcode));
                let _ = writeln!(ir, "  %v{k} = call i64 @op_{:?}(ptr %rt, i64 {site}, i64 {}, i64 {}, i64 {}, ptr %rest, ptr %live, i32 {}) #0", ins.opcode, abc[0], abc[1], abc[2], live.len());
            }
        }
        for (j, l) in live.iter().enumerate() { let _ = writeln!(ir, "  %ll{k}.{j} = load i64, ptr %lp{k}.{j}\n  store i64 %ll{k}.{j}, ptr {l}"); }
        let _ = writeln!(ir, "  %e{k} = call i1 @is_err(i64 %v{k})\n  br i1 %e{k}, label %x{k}, label %ok{k}\nok{k}:");
        let (yes_to, no_to) = match ins.opcode { O::JumpIfFalse | O::JumpIfFalseOrPop => (next.clone(), format!("i{t}")), _ => (format!("i{t}"), next.clone()) };
        match ins.opcode {
            O::JumpIfFalse | O::JumpIfFalseOrPop | O::JumpIfTrueOrPop => { let _ = writeln!(ir, "  %t{k} = icmp eq i64 %v{k}, {yes}\n  br i1 %t{k}, label %{yes_to}, label %{no_to}"); }
            _ => {
                if pushes > 0 { let _ = writeln!(ir, "  store i64 %v{k}, ptr %s{base}"); }
                for j in 1..pushes { let _ = writeln!(ir, "  %q{k}.{j} = getelementptr i64, ptr %rest, i64 {}\n  %w{k}.{j} = load i64, ptr %q{k}.{j}\n  store i64 %w{k}.{j}, ptr %s{}", j - 1, base + j); }
                let _ = writeln!(ir, "  br label %{next}");
            }
        }
        // An error: a `for` that ended leaves its loop, any other goes to the `try` that catches it, or out of the function.
        let _ = writeln!(ir, "x{k}:\n  %id{k} = call i32 @err_id(i64 %v{k})");
        if ins.opcode == O::ForIter { let _ = writeln!(ir, "  %end{k} = icmp eq i32 %id{k}, {}\n  br i1 %end{k}, label %i{t}, label %y{k}\ny{k}:", stop()); }
        let _ = writeln!(ir, "  %c{k} = call i64 @edge_raise(ptr %rt, i64 {site}, ptr %hp)\n  %ce{k} = call i1 @is_err(i64 %c{k})\n  br i1 %ce{k}, label %z{k}, label %g{k}\nz{k}:");
        if b.at != 0 { let _ = writeln!(ir, "  call void @edge_leave(ptr %rt, i64 %c{k})"); }
        let arms: String = handlers.iter().map(|&(h, _)| format!(" i32 {h}, label %catch{h}")).collect();
        let _ = writeln!(ir, "  ret i64 %c{k}\ng{k}:\n  store i64 %c{k}, ptr %exc\n  %hv{k} = load i32, ptr %hp\n  switch i32 %hv{k}, label %nowhere [{arms} ]");
    }
    let _ = writeln!(ir, "i{}:\n  ret i64 {none}\nnowhere:\n  unreachable", c.instructions.len());
    let mut seen = Vec::new();
    for &(h, except) in &handlers {
        if seen.contains(&h) { continue; }
        seen.push(h);
        let _ = writeln!(ir, "catch{h}:");
        if let (true, Some(Some(d))) = (except, at.get(h)) { let _ = writeln!(ir, "  %cx{h} = load i64, ptr %exc\n  store i64 %cx{h}, ptr %s{}", d - 1); }
        let _ = writeln!(ir, "  br label %i{h}");
    }
    let pairs: Vec<String> = c.instructions.iter().map(|i| format!("!{{!\"{:?}\", i32 {}}}", i.opcode, i.operand)).collect();
    let _ = writeln!(ir, "}}\n!{} = !{{{}}}", b.at, pairs.join(", "));
}

/* An `llvm.assume` of the one tag every path gives `v`, so the handler's own test of it folds. */
fn assume(ir: &mut String, typed: Option<&Typed>, tags: Tags, v: &str) {
    let Some(t) = typed else { return };
    let plain = [("int", "is_int"), ("float", "is_float"), ("bool", "is_bool"), ("NoneType", "is_none")];
    let test = plain.iter().find(|(n, _)| t.prog.rules.tag(n) == Some(tags)).map(|x| x.1);
    let test = test.or_else(|| (tags.count_ones() == 1 && !plain.iter().any(|(n, _)| t.prog.rules.tag(n) == Some(tags))).then_some("is_heap"));
    if let Some(f) = test { let _ = writeln!(ir, "  {v}.is = call i1 @{f}(i64 {v})\n  call void @llvm.assume(i1 {v}.is)"); }
}

/* The function every path calls with the callee at `at` of the stack, when the tree knows it and it is native. */
fn known<'b>(seen: &[&State], at: usize, bodies: &'b [Body]) -> Option<&'b Body<'b>> {
    let mut got = None;
    for s in seen {
        let Known::Body(f) = s.stack.get(at)?.ty.known else { return None };
        if got.is_some_and(|g| g != f) { return None; }
        got = Some(f);
    }
    let f = got? as usize;
    bodies.iter().find(|b| b.ty == Some(f) && b.at != 0)
}

/* StopIteration's id, the error a `for` that ended gives. */
fn stop() -> usize { compiler::types::BUILTIN_TYPES.iter().position(|&t| t == "StopIteration").unwrap_or(0) }

