use super::*;
use crate::value::{BinOp, UnOp, unboxed};

use cache::OpcodeCache;

/* IC, same for comparison opcodes, reflected pairs collapse to the forward name. */
fn compare_dunder_name(op: OpCode) -> Option<&'static str> {
    super::dunder::compare_dunder_names(op).map(|(l, _)| l)
}

impl<'a> VM<'a> {

    /* Add/Sub/Mul/Div with IC, Mod/Pow/FloorDiv exact for ints of any size, Minus is unary. */
    pub(crate) fn handle_arith(&mut self, op: OpCode, operand: u16, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk, rerun: crate::vm::redo::Rerun) -> Result<(), VmErr> {
        if op == OpCode::Minus {
            return self.exec_unary(rip, cache, chunk, "__neg__", |v| Val::float(-v.as_float()), UnOp::Neg, "-", rerun);
        }
        if op == OpCode::Pos {
            // +float returns the value unchanged, bool drops its tag to int.
            return self.exec_unary(rip, cache, chunk, "__pos__", |v| v, UnOp::Pos, "+", rerun);
        }

        let (a, b) = self.pop2()?;
        let inplace = matches!(op, OpCode::InPlaceAdd | OpCode::InPlaceSub) || operand == crate::parser::INPLACE;

        // `name += rhs` extends a left list in place with any iterable (Python __iadd__). Other types behave as Add.
        let op = if op == OpCode::InPlaceAdd {
            // Guard is_heap since `i += 1` operands are inline ints, and heap.get on a non-heap Val indexes garbage.
            let a_is_list = a.is_heap() && matches!(self.heap.get(a), HeapObj::List(_));
            if a_is_list {
                // A `map` or `filter` whose steps run Python, met where a frame may open, appends its items as a job of the loop.
                if rerun == crate::vm::redo::Rerun::Stack && b.is_heap() && matches!(self.heap.get(b), HeapObj::Iter(..)) {
                    self.stack.extend_from_slice(&[a, b]);
                    if self.extend_job(chunk)? { return Ok(()); }
                    self.stack.truncate(self.stack.len() - 2);
                }
                // Snapshot rhs first so `xs += xs` doubles correctly, TypeError if rhs isn't iterable.
                let rhs = self.extract_iter(b)?;
                if let HeapObj::List(la) = self.heap.get(a) { la.borrow_mut().extend_from_slice(&rhs); }
                self.push(a);
                return Ok(());
            }
            // A bytearray grows in place by any bytes-like value.
            if a.is_heap() && matches!(self.heap.get(a), HeapObj::ByteArray(_)) && let Some(more) = self.heap.bytes_of(b).map(<[u8]>::to_vec) {
                self.heap.reserve(more.len())?;
                if let Some(HeapObj::ByteArray(buf)) = self.heap.try_get_mut(a) { buf.extend_from_slice(&more); }
                self.push(a);
                return Ok(());
            }
            OpCode::Add
        } else { op };

        // `name -= rhs` removes from a left set in place (alias-visible), every other type behaves as Sub.
        let op = if op == OpCode::InPlaceSub {
            if self.is_set_like(a) && self.is_set_like(b) { return self.set_iop_and_push(a, b, OpCode::Sub); }
            OpCode::Sub
        } else { op };

        // A dunder the op runs is a frame of the loop, the op run again after it, a format's of the instances a tuple holds too.
        let user = self.heap.users && (a.is_heap() || b.is_heap()) && (self.instance_class(a).is_some() || self.instance_class(b).is_some());
        // A text formats an instance before the instance is asked, its `__rmod__` never, the conversions it calls the op's.
        let text = user && op == OpCode::Mod && self.text_mod(a, b);
        let fmt = text || !user && op == OpCode::Mod && b.is_heap() && rerun != crate::vm::redo::Rerun::No && self.redo_fmt(b);
        // A stack op formats them as a job, the conversions run as frames of the loop before the text is made once.
        if fmt && rerun == crate::vm::redo::Rerun::Stack && self.percent_job(a, b, chunk)? { return Ok(()); }
        let user = user && !text;
        let armed = (user || fmt) && self.redo_late(rip, rerun, self.stack.len() + 2, &[a, b]);
        let r = self.arith_of(op, a, b, inplace, user, rip, cache, chunk);
        self.redo_done(armed);
        r
    }

    /* `a % b` of a plain text whose values hold instances, each spec's dunder a frame of the loop and the text made once, Ok(false) if none converts. */
    #[inline(never)]
    fn percent_job(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let Some(HeapObj::Str(fmt)) = self.heap.plain(a) else { return Ok(false) };
        let fmt = fmt.clone();
        let args: alloc::vec::Vec<Val> = match self.heap.try_get(b) { Some(HeapObj::Tuple(t)) => t.clone(), _ => alloc::vec![b] };
        // Its conversions noted in one word, a format of more values formats them nested.
        if args.len() > 64 { return Ok(false); }
        let mut specs = alloc::vec::Vec::new();
        // A spec that fails fails again as the text is made, past the conversions before it.
        let _ = self.percent_with(&fmt, b, Some(&args), 0, Some(&mut specs), false, chunk);
        let plan: alloc::vec::Vec<(usize, crate::vm::jobs::Check)> = specs.into_iter().filter_map(|(at, c)| self.percent_check(args[at], c).map(|k| (at, k))).collect();
        if plan.is_empty() { return Ok(false); }
        if let crate::vm::jobs::Converted::Give(s) = self.convert_run(alloc::boxed::Box::new(Percent { fmt, arg: b, args, done: 0, plan, k: 0 }), chunk)? { self.push(s); }
        Ok(true)
    }

    /* The dunder spec `conv` converts instance `v` by, as CPython's `%` picks it, None when its class lacks it. */
    fn percent_check(&self, v: Val, conv: char) -> Option<crate::vm::jobs::Check> {
        use crate::vm::jobs::Check;
        let class = self.instance_class(v)?;
        let has = |n: &str| self.lookup_class_member(class, n).is_some();
        match conv {
            's' if has("__str__") => Some(Check::Str(false)),
            's' | 'r' | 'a' if has("__repr__") => Some(Check::Str(true)),
            'd' | 'i' | 'u' if has("__int__") => Some(Check::Int(v)),
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' if has("__float__") => Some(Check::Float(v)),
            'd' | 'i' | 'u' | 'x' | 'X' | 'o' | 'c' | 'f' | 'F' | 'e' | 'E' | 'g' | 'G' if has("__index__") => Some(Check::Index),
            _ => None,
        }
    }

    /* Whether `a % b` is a plain text's format of an instance, which CPython's `str.__mod__` answers before `b.__rmod__`. */
    #[inline(never)]
    fn text_mod(&self, a: Val, b: Val) -> bool {
        matches!(self.heap.plain(a), Some(HeapObj::Str(_))) && matches!(self.heap.try_get(b), Some(HeapObj::Instance(..)))
    }

    /* `handle_arith` of the operands it took, `user` whether one is an instance. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn arith_of(&mut self, op: OpCode, a: Val, b: Val, inplace: bool, user: bool, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        // The dunder's own answer is the op's, its frame's value taking the operands' place, though a format's is not.
        if user { self.redo_then(super::super::redo::Then::Done); }
        let dunder = if user { self.try_binary_dunder(op, a, b, inplace, chunk) } else { Ok(None) };
        if user { self.redo_then(super::super::redo::Then::Again); }

        // instance dunder protocol, try user-defined operator before any builtin coercion.
        if let Some(r) = dunder? {
            // record the resolved class+method so the IC can fire on subsequent iterations of a hot loop. Reflected ops deopt through `NotImplemented`.
            if let Some(name) = self.site_dunder_name(op, a, inplace) {
                self.record_dunder_hit(rip, cache, a, name, 2);
            }
            // A number on the left answered by the instance on the right learns its reflected dunder, the operands swapped.
            if (a.is_int() || a.is_float() || a.is_bool()) && let Some((_, rname)) = super::dunder::binary_dunder_names(op) {
                self.record_dunder_hit(rip, cache, b, rname, crate::vm::sites::REFLECTED);
            }
            self.push(r);
            return Ok(());
        }

        if op == OpCode::Sub && let Some((x, y)) = self.view_set_operands(a, b, true, chunk)? {
            return self.set_binop_and_push(x, y, op);
        }
        // A number of a class deriving from int or float computes as the number.
        let (a, b) = (unboxed(a, &self.heap), unboxed(b, &self.heap));
        let result = match op {
            OpCode::Add => self.add_vals(a, b),
            OpCode::Sub => self.sub_vals(a, b),
            OpCode::Mul => self.mul_vals(a, b),
            OpCode::Div => self.div_vals(a, b),
            OpCode::Mod => self.exec_mod(a, b, chunk),
            OpCode::Pow => self.exec_pow(a, b),
            OpCode::FloorDiv => self.exec_floordiv(a, b),
            _ => return Err(cold_runtime("non-arith opcode in handle_arith")),
        };
        let result = if inplace { result.map_err(inplace_err)? } else { result? };
        self.push(result);
        Ok(())
    }

    /* Unary `-`/`+`, instance dunder takes precedence over numeric coercion. `ffl` maps the float case. */
    #[allow(clippy::too_many_arguments)]
    fn exec_unary(&mut self, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk,
                  name: &'static str, ffl: fn(Val) -> Val, op: UnOp, sym: &'static str, rerun: crate::vm::redo::Rerun) -> Result<(), VmErr> {
        let v = self.pop()?;
        let armed = self.heap.users && v.is_heap() && self.instance_class(v).is_some() && self.redo_late(rip, rerun, self.stack.len() + 1, &[v]);
        let dunder = self.try_call_dunder(v, name, &[], chunk);
        self.redo_done(armed);
        if let Some(r) = dunder? {
            // monomorphic unary-instance sites promote like binary ops.
            self.record_dunder_hit(rip, cache, v, name, 1);
            self.push(r);
            return Ok(());
        }
        let v = unboxed(v, &self.heap);
        let result = if v.is_float() {
            ffl(v)
        } else if v.is_int() || v.is_bool() {
            let i = self.as_i128(v).unwrap_or(0);
            self.heap.int(if op == UnOp::Neg { -i } else { i })?
        } else {
            match self.heap.foreign_unop(op, v)? { Some(r) => r, None => return Err(self.bad_operand(&crate::s!("unary ", str sym), v)) }
        };
        self.push(result);
        Ok(())
    }

    fn exec_mod(&mut self, a: Val, b: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        // `str % args` is printf-style formatting, not modulo, and so is `bytes % args`.
        if a.is_heap() && matches!(self.heap.get(a), HeapObj::Str(_)) {
            return self.str_percent_format(a, b, chunk);
        }
        if a.is_heap() && let HeapObj::Bytes(f) | HeapObj::ByteArray(f) = self.heap.get(a) {
            let (fmt, array) = (f.iter().map(|&c| c as char).collect::<String>(), matches!(self.heap.get(a), HeapObj::ByteArray(_)));
            // Every character the bytes form writes is below 0x100, so each goes back as one byte.
            let text = self.percent_format(fmt, b, true, chunk)?;
            let out: alloc::vec::Vec<u8> = text.chars().map(|c| c as u8).collect();
            return self.heap.alloc(if array { HeapObj::ByteArray(out) } else { HeapObj::Bytes(out) });
        }
        self.floor_op(BinOp::Mod, a, b, "%")
    }

    /* `a // b` or `a % b`, floats when either side is one, signed like the divisor. */
    pub(crate) fn floor_op(&mut self, op: BinOp, a: Val, b: Val, sym: &str) -> Result<Val, VmErr> {
        let modulo = op == BinOp::Mod;
        if a.is_float() || b.is_float() {
            let (Some(af), Some(bf)) = (crate::vm::num_f64(a, &self.heap), crate::vm::num_f64(b, &self.heap)) else { return Err(self.unsupported(sym, a, b)); };
            let (af, bf) = (af?, bf?);
            if bf == 0.0 { return Err(VmErr::ZeroDiv); }
            let (q, r) = float_divmod(af, bf);
            return Ok(Val::float(if modulo { r } else { q }));
        }
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) && y != 0 && let Some((q, r)) = crate::vm::int_divmod(x, y) {
            return self.heap.int(if modulo { r } else { q });
        }
        self.heap.foreign_binop(op, a, b)?.ok_or_else(|| self.unsupported(sym, a, b))
    }

    /* printf-style `str % args` translates each `%[flags][width][.prec]conv` into the `{:spec}` mini-language and reuses `format_value`. A tuple spreads, else one value. */
    fn str_percent_format(&mut self, fmt_val: Val, arg: Val, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let fmt = match self.heap.get(fmt_val) { HeapObj::Str(s) => s.clone(), _ => return Err(cold_type("% requires a string")) };
        let out = self.percent_format(fmt, arg, false, chunk)?;
        self.heap.alloc(HeapObj::Str(out))
    }

    /* The text `fmt % arg` makes, `bytes` reading `%s` and `%b` as bytes and `%r` as `%a`, the bytes form of PEP 461. */
    fn percent_format(&mut self, fmt: String, arg: Val, bytes: bool, chunk: &SSAChunk) -> Result<String, VmErr> {
        self.percent_with(&fmt, arg, None, 0, None, bytes, chunk)
    }

    /* `percent_format` over `given` with the values whose `done` bits are set already converted, or with `plan` noting instances and specs in order. */
    #[allow(clippy::too_many_arguments)]
    fn percent_with(&mut self, fmt: &str, arg: Val, given: Option<&[Val]>, done: u64, mut plan: Option<&mut alloc::vec::Vec<(usize, char)>>, bytes: bool, chunk: &SSAChunk) -> Result<String, VmErr> {
        let (args, spread): (alloc::vec::Vec<Val>, bool) = match self.heap.try_get(arg) {
            Some(HeapObj::Tuple(t)) => (given.map_or_else(|| t.clone(), <[Val]>::to_vec), true),
            _ => (given.map_or_else(|| alloc::vec![arg], <[Val]>::to_vec), false),
        };
        let chars: alloc::vec::Vec<char> = fmt.chars().collect();
        let mut out = String::new();
        let mut ai = 0usize;
        let mut i = 0usize;
        let mut keyed = false;
        while i < chars.len() {
            let c = chars[i];
            if c != '%' { out.push(c); i += 1; continue; }
            let start = i;
            let stray = || cold_value_s(crate::s!("stray % at position ", int start as i64));
            i += 1;
            if i < chars.len() && chars[i] == '%' { out.push('%'); i += 1; continue; }
            // `%(name)s` reads `name` off a mapping, its parentheses nesting.
            let mut key = None;
            if i < chars.len() && chars[i] == '(' {
                let (mut depth, mut j) = (1, i + 1);
                while j < chars.len() && depth > 0 { match chars[j] { '(' => depth += 1, ')' => depth -= 1, _ => {} } j += 1; }
                if depth > 0 { return Err(cold_value_s(crate::s!("stray % or incomplete format key at position ", int start as i64))); }
                key = Some(chars[i + 1..j - 1].iter().collect::<String>());
                i = j;
            }
            let no_star = |at: usize, key: &Option<String>| if key.is_some() && chars.get(at) == Some(&'*') { Err(cold_value_s(crate::s!("* cannot be used with a parenthesised mapping key at position ", int start as i64))) } else { Ok(()) };
            // flags
            let (mut left, mut zero, mut plus, mut space, mut alt) = (false, false, false, false, false);
            while i < chars.len() {
                match chars[i] {
                    '-' => left = true, '0' => zero = true, '+' => plus = true, ' ' => space = true, '#' => alt = true,
                    _ => break,
                }
                i += 1;
            }
            // width is digits, or `*` reads it (with sign) from the next argument.
            let mut width = String::new();
            no_star(i, &key)?;
            if i < chars.len() && chars[i] == '*' {
                i += 1;
                let w = Self::star_arg_int(&args, &mut ai)?;
                if w < 0 { left = true; } // negative `*` width left-aligns, like Python
                width = crate::s!(int w.unsigned_abs());
            } else {
                while i < chars.len() && chars[i].is_ascii_digit() { width.push(chars[i]); i += 1; }
            }
            let mut prec = String::new();
            let mut has_prec = false;
            if i < chars.len() && chars[i] == '.' {
                i += 1;
                no_star(i, &key)?;
                if i < chars.len() && chars[i] == '*' {
                    i += 1;
                    let p = Self::star_arg_int(&args, &mut ai)?;
                    // Negative `.*` precision is ignored, matching Python.
                    if p >= 0 { has_prec = true; prec = crate::s!(int p); }
                } else {
                    has_prec = true;
                    while i < chars.len() && chars[i].is_ascii_digit() { prec.push(chars[i]); i += 1; }
                }
            }
            // The C length modifiers `h`, `l` and `L` change nothing.
            while i < chars.len() && matches!(chars[i], 'h' | 'l' | 'L') { i += 1; }
            if i >= chars.len() { return Err(stray()); }
            let (conv, at_conv) = (chars[i], i); i += 1;
            let (val, at) = match key {
                Some(k) => { keyed = true; (self.format_key(arg, k, bytes, chunk)?, usize::MAX) }
                None if keyed => return Err(cold_value_s(crate::s!("format requires a parenthesised mapping key at position ", int start as i64))),
                None => { let v = *args.get(ai).ok_or_else(|| short_of(args.len()))?; ai += 1; (v, ai - 1) }
            };
            if let Some(p) = plan.as_deref_mut() { if at != usize::MAX && self.is_instance(val) { p.push((at, conv)); } continue; }
            let mut converted = at < 64 && done >> at & 1 != 0;
            // An instance converts here by the dunder its spec runs, as the job converts it.
            let val = match if converted || bytes || !self.is_instance(val) { None } else { self.percent_check(val, conv) } {
                Some(c) => { converted = true; self.with_roots([val], |vm| vm.convert_now(val, c, chunk))? }
                None => val,
            };
            // A bad verb right after `%` is unsupported, one after flags or a width may end a stray `%`.
            let unsupported = || if at_conv == start + 1 { cold_value_s(crate::s!("unsupported format %", char conv, " at position ", int start as i64)) }
                else { cold_value_s(crate::s!("stray % at position ", int start as i64, " or unexpected format character '", char conv, "' at position ", int at_conv as i64)) };
            // How an error names the argument, by its place when a tuple spread them.
            let wrong = |vm: &Self, needs: &str| {
                let at = if spread { crate::s!("format argument ", int ai as i64) } else { String::from("format argument") };
                VmErr::TypeMsg(crate::s!(str &at, ": %", char conv, " requires ", str needs, ", not ", str vm.type_name(val)))
            };
            let int_like = val.is_int() || val.is_bool() || crate::frontend::int::is_big(&self.heap, val);
            // Map printf conversion -> (format value, spec type char, is-numeric).
            let (fval, ty, numeric): (Val, Option<char>, bool) = match conv {
                // The bytes form takes bytes, or what `__bytes__` makes, as their Latin-1 text.
                's' | 'b' if bytes => {
                    let raw = self.heap.octets(val).map(|b| b.to_vec());
                    let raw = match raw { Some(r) => r, None => match self.try_call_dunder(val, "__bytes__", &[], chunk)? {
                        Some(r) if self.heap.bytes_of(r).is_some() => self.heap.bytes_of(r).unwrap_or_default().to_vec(),
                        _ => { let w = wrong(self, "a bytes-like object, or an object that implements __bytes__"); return Err(match w { VmErr::TypeMsg(m) => VmErr::TypeMsg(m.replacen("%s", "%b", 1)), e => e }); }
                    }};
                    (self.heap.alloc(HeapObj::Str(raw.iter().map(|&c| c as char).collect()))?, None, false)
                }
                'r' | 'a' if bytes => { let s = crate::vm::format_spec::ascii_escape(&self.repr_op(val, chunk)?); (self.heap.alloc(HeapObj::Str(s))?, None, false) }
                'c' if bytes => {
                    let byte = match self.heap.bytes_of(val) {
                        Some([b]) => *b,
                        _ if int_like => match self.as_i128(val).and_then(|n| u8::try_from(n).ok()) {
                            Some(b) => b,
                            None => { let at = if spread { crate::s!("format argument ", int ai as i64) } else { String::from("format argument") }; return Err(VmErr::Raised(crate::s!("OverflowError: ", str &at, ": %c argument not in range(256)"))); }
                        },
                        _ => return Err(wrong(self, "an integer in range(256) or a single byte")),
                    };
                    (self.heap.alloc(HeapObj::Str(String::from(byte as char)))?, None, false)
                }
                // A converted value is the text its dunder gave.
                's' | 'r' if converted => (val, None, false),
                'a' if converted => { let s = crate::vm::format_spec::ascii_escape(&self.display(val)); (self.heap.alloc(HeapObj::Str(s))?, None, false) }
                's' => { let s = self.display_op(val, chunk)?; (self.heap.alloc(HeapObj::Str(s))?, None, false) }
                'r' => { let s = self.repr_op(val, chunk)?; (self.heap.alloc(HeapObj::Str(s))?, None, false) }
                'a' => { let s = crate::vm::format_spec::ascii_escape(&self.repr_op(val, chunk)?); (self.heap.alloc(HeapObj::Str(s))?, None, false) }
                // A float truncates the way `int()` does, an integer verb refuses it.
                'd' | 'i' | 'u' => {
                    let v = if int_like || val.is_float() { val } else { self.format_number(val, "__int__", chunk)?.ok_or_else(|| wrong(self, "a real number"))? };
                    let v = if v.is_float() { crate::vm::globals::numeric::float_to_int(&mut self.heap, v.as_float(), libm::trunc)? } else { v };
                    (v, Some('d'), true)
                }
                // The integer verbs take an int or what `__index__` makes one.
                'x' | 'X' | 'o' => {
                    let v = if int_like { val } else { self.format_number(val, "__index__", chunk)?.ok_or_else(|| wrong(self, "an integer"))? };
                    (v, Some(conv), true)
                }
                'c' if !(int_like || matches!(self.heap.try_get(val), Some(HeapObj::Str(_)))) => return Err(wrong(self, "an int or a unicode character")),
                'f' | 'F' | 'e' | 'E' | 'g' | 'G' if crate::vm::num_f64(val, &self.heap).is_none() => return Err(wrong(self, "a real number")),
                'c' => (val, Some('c'), false),
                'f' | 'F' => (val, Some('f'), true),
                'e' => (val, Some('e'), true),
                'E' => (val, Some('E'), true),
                'g' => (val, Some('g'), true),
                'G' => (val, Some('G'), true),
                _ => return Err(unsupported()),
            };
            // Build the equivalent `{:spec}` string. printf right-aligns by default (incl. strings).
            let mut spec = String::new();
            if left { spec.push('<'); }
            else if !(zero && numeric) { spec.push('>'); }
            if plus { spec.push('+'); } else if space { spec.push(' '); }
            if alt { spec.push('#'); }
            if zero && numeric && !left { spec.push('0'); }
            spec.push_str(&width);
            if has_prec { spec.push('.'); spec.push_str(if prec.is_empty() { "0" } else { &prec }); }
            if let Some(t) = ty { spec.push(t); }
            let rendered = crate::vm::format_spec::format_value(fval, &spec, &self.heap).map_err(|m| match m {
                crate::vm::format_spec::C_RANGE_ERR => VmErr::Raised(String::from("OverflowError: format argument: %c argument not in range(0x110000)")),
                m => crate::vm::format_spec::fmt_err(m),
            })?;
            out.push_str(&rendered);
        }
        // Every supplied arg must be consumed, like Python, unless a mapping gave them by name.
        if !keyed && ai != args.len() {
            return Err(VmErr::TypeMsg(crate::s!("not all arguments converted during string formatting (required ", int ai as i64, ", got ", int args.len() as i64, ")")));
        }
        Ok(out)
    }

    /* The value `%(key)s` reads off `arg`, which any subscriptable value but a tuple or a string may be. */
    fn format_key(&mut self, arg: Val, key: String, bytes: bool, chunk: &SSAChunk) -> Result<Val, VmErr> {
        let subscriptable = arg.is_heap() && !matches!(self.heap.get(arg), HeapObj::Tuple(_) | HeapObj::Str(_))
            && (self.heap.dict_cell(arg).is_some() || matches!(self.heap.get(arg), HeapObj::List(_) | HeapObj::Instance(..)));
        if !subscriptable { return Err(VmErr::TypeMsg(crate::s!("format requires a mapping, not ", str self.type_name(arg)))); }
        let k = self.heap.alloc(if bytes { HeapObj::Bytes(key.chars().map(|c| c as u8).collect()) } else { HeapObj::Str(key) })?;
        self.with_roots([k], |vm| match vm.try_call_dunder(arg, "__getitem__", &[k], chunk)? {
            Some(v) => Ok(v),
            None => vm.getitem_val(arg, k),
        })
    }

    /* Reads a `*` width/precision argument as an i64, non-integers raise TypeError like Python. */
    fn star_arg_int(args: &[Val], ai: &mut usize) -> Result<i64, VmErr> {
        let v = *args.get(*ai).ok_or_else(|| short_of(args.len()))?;
        *ai += 1;
        if v.is_bool() { return Ok(v.as_bool() as i64); }
        if v.is_int() { return Ok(v.as_int()); }
        Err(cold_type("* wants int"))
    }

    /* `%d`/`%x` on a user instance defers to its `__int__`, matching Python. */
    /* The int a `%` verb takes from an instance through `dunder`, None when the instance has none. */
    fn format_number(&mut self, v: Val, dunder: &str, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        if !matches!(self.heap.try_get(v), Some(HeapObj::Instance(..))) { return Ok(None); }
        match self.try_call_dunder(v, dunder, &[], chunk)? {
            Some(r) if r.is_int() || crate::frontend::int::is_big(&self.heap, r) => Ok(Some(r)),
            Some(r) => Err(self.returned_wrong(v, dunder, "an int", r)),
            None => Ok(None),
        }
    }

    fn exec_floordiv(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        self.floor_op(BinOp::FloorDiv, a, b, "//")
    }

    fn exec_pow(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        self.pow_vals(a, b, "** requires numeric operands")
    }

    /* Int bitwise and shifts of any size, BitNot unary. Set/Set on |/&/^ means union/intersection/symmetric-diff, other types use the bitwise path. */
    pub(crate) fn handle_bitwise(&mut self, op: OpCode, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        // Augmented set bitwise reuses the plain path but mutates the left set in place.
        let inplace = matches!(op, OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd | OpCode::InPlaceBitXor);
        let op = match op {
            OpCode::InPlaceBitOr => OpCode::BitOr,
            OpCode::InPlaceBitAnd => OpCode::BitAnd,
            OpCode::InPlaceBitXor => OpCode::BitXor,
            other => other,
        };
        if op == OpCode::BitNot {
            let v = self.pop()?;
            if let Some(r) = self.try_call_dunder(v, "__invert__", &[], chunk)? {
                self.push(r);
                return Ok(());
            }
            let v = unboxed(v, &self.heap);
            let out = if v.is_int() || v.is_bool() { self.heap.int(!self.as_i128(v).unwrap_or(0))? }
                else { match self.heap.foreign_unop(UnOp::Invert, v)? { Some(r) => r, None => return Err(self.bad_operand("unary ~", v)) } };
            self.push(out);
            return Ok(());
        }

        let (a, b) = self.pop2()?;

        // User instance operands dispatch __or__/__and__/__xor__ (and reflected) first.
        let dunder = self.try_binary_dunder(op, a, b, inplace || operand == crate::parser::INPLACE, chunk);
        if let Some(r) = dunder? { self.push(r); return Ok(()); }
        let (a, b) = (unboxed(a, &self.heap), unboxed(b, &self.heap));

        if matches!(op, OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor) && let Some((x, y)) = self.view_set_operands(a, b, true, chunk)? {
            return self.set_binop_and_push(x, y, op);
        }
        if self.is_set_like(a) && self.is_set_like(b)
            && matches!(op, OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor) {
            return if inplace { self.set_iop_and_push(a, b, op) } else { self.set_binop_and_push(a, b, op) };
        }
        // `dict | dict` merges, right operand winning, into the type of the left, and `|=` on a dict updates it in place.
        if op == OpCode::BitOr && let (Some(x), Some(y)) = (self.heap.dict_cell(a), self.heap.dict_cell(b)) {
            let entries: alloc::vec::Vec<(Val, Val)> = y.borrow().iter().collect();
            let frozen = matches!(self.heap.get(a), HeapObj::FrozenDict(_));
            if inplace && !frozen {
                let x = x.clone();
                for (k, v) in entries { x.borrow_mut().insert(k, v, &self.heap); }
                self.push(a);
                return Ok(());
            }
            let mut merged = x.borrow().clone();
            for (k, v) in entries { merged.insert(k, v, &self.heap); }
            let out = self.heap.alloc(if frozen { HeapObj::FrozenDict(Rc::new(RefCell::new(merged))) } else { HeapObj::Dict(Rc::new(RefCell::new(merged))) })?;
            self.push(out);
            return Ok(());
        }
        // Two bools keep a bool for `&`, `|` and `^`.
        if a.is_bool() && b.is_bool() && let Some(v) = match op {
            OpCode::BitAnd => Some(a.as_bool() & b.as_bool()), OpCode::BitOr => Some(a.as_bool() | b.as_bool()), OpCode::BitXor => Some(a.as_bool() ^ b.as_bool()), _ => None,
        } {
            self.push(Val::bool(v));
            return Ok(());
        }
        let result = match op {
            OpCode::BitAnd => self.int_bitop(BinOp::And, "&", a, b)?,
            // `int | str` between types builds a union instead.
            OpCode::BitOr => match self.type_union(a, b)? { Some(u) => u, None => self.int_bitop(BinOp::Or, "|", a, b)? },
            OpCode::BitXor => self.int_bitop(BinOp::Xor, "^", a, b)?,
            OpCode::Shl => self.int_bitop(BinOp::Shl, "<<", a, b)?,
            OpCode::Shr => self.int_bitop(BinOp::Shr, ">>", a, b)?,
            _ => return Err(cold_runtime("non-bitwise opcode in handle_bitwise")),
        };
        self.push(result);
        Ok(())
    }

    /* `int | str` flattened and deduplicated, a lone member stands alone, None when an operand is not a type. */
    fn type_union(&mut self, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if a.is_none() && b.is_none() { return Ok(None); }
        let mut members: Vec<Val> = Vec::new();
        for v in [a, b] {
            if v.is_none() {
                self.register_builtin("NoneType");
                members.push(self.global("NoneType").ok_or_else(|| cold_runtime("NoneType is not registered"))?);
                continue;
            }
            // A sentinel stands in a union as itself, the way `int | MISSING` marks an optional argument.
            if crate::frontend::sentinel::get(&self.heap, v).is_some() { members.push(v); continue; }
            if !v.is_heap() { return Ok(None); }
            match self.heap.get(v) {
                HeapObj::Union(args) => if let HeapObj::Tuple(t) = self.heap.get(*args) { members.extend_from_slice(t) },
                HeapObj::Type(_) | HeapObj::Class(..) | HeapObj::GenericAlias(..) | HeapObj::TypeAlias(..) | HeapObj::TypeVar(_) => members.push(v),
                _ => return Ok(None),
            }
        }
        let mut unique: Vec<Val> = Vec::new();
        for m in members {
            if !unique.iter().any(|&u| eq_member(u, m, &self.heap)) { unique.push(m); }
        }
        if unique.len() == 1 { return Ok(Some(unique[0])); }
        let args = self.heap.alloc(HeapObj::Tuple(unique))?;
        Ok(Some(self.heap.alloc(HeapObj::Union(args))?))
    }

    /* `a @ b` has no builtin meaning, only `__matmul__` or `__rmatmul__` answer it. */
    pub(crate) fn handle_matmul(&mut self, operand: u16, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;
        let dunder = self.try_binary_dunder(OpCode::MatMul, a, b, operand == crate::parser::INPLACE, chunk);
        let r = dunder?.ok_or_else(|| self.unsupported("@", a, b))?;
        self.push(r);
        Ok(())
    }

    /* `&`, `|`, `^`, `<<` and `>>` on ints, exact in i128 and through the int kind past it. */
    fn int_bitop(&mut self, op: BinOp, sym: &'static str, a: Val, b: Val) -> Result<Val, VmErr> {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            let r = match op {
                BinOp::And => Some(x & y),
                BinOp::Or => Some(x | y),
                BinOp::Xor => Some(x ^ y),
                _ if y < 0 => return Err(cold_value("negative shift count")),
                // Bits shifted past the top leave i128, which `checked_shl` alone lets through.
                BinOp::Shl => if y < 128 { x.checked_shl(y as u32).filter(|r| r >> y == x) } else { (x == 0).then_some(0) },
                _ => Some(x >> y.min(127)),
            };
            if let Some(r) = r { return self.heap.int(r); }
        }
        self.heap.foreign_binop(op, a, b)?.ok_or_else(|| self.unsupported(sym, a, b))
    }

    /* Learns `a`'s dunder `name` for a comparison, a `!=`'s on a class with no `__ne__` its `__eq__`, inverted. */
    fn record_compare_hit(&self, op: OpCode, rip: usize, cache: &OpcodeCache, a: Val, name: &str) {
        let inverts = op == OpCode::NotEq && self.instance_class(a).is_some_and(|c| self.lookup_class_member(c, "__ne__").is_none());
        if inverts { self.record_dunder_hit(rip, cache, a, "__eq__", crate::vm::sites::INVERTS) } else { self.record_dunder_hit(rip, cache, a, name, 2) }
    }

    pub(crate) fn handle_compare(&mut self, op: OpCode, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk, rerun: crate::vm::redo::Rerun) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;

        // A dunder the op runs is a frame of the loop, the op run again after it.
        let user = self.heap.users && (a.is_heap() || b.is_heap()) && (self.instance_class(a).is_some() || self.instance_class(b).is_some());
        let armed = user && self.redo_late(rip, rerun, self.stack.len() + 2, &[a, b]);
        // The dunder's own answer is the op's, its frame's value taking the operands' place.
        if armed { self.redo_then(super::super::redo::Then::Done); }
        let dunder = if user { self.compare_dunder(op, a, b, chunk) } else { Ok(None) };
        self.redo_done(armed);

        // try the user-defined comparison dunder before falling back to numeric/string compare.
        if let Some(r) = dunder? {
            // monomorphic comparison sites cache the resolved method like arithmetic ones.
            if let Some(name) = compare_dunder_name(op) {
                self.record_compare_hit(op, rip, cache, a, name);
            }
            if (a.is_int() || a.is_float() || a.is_bool()) && let Some((_, rname)) = super::dunder::compare_dunder_names(op) {
                self.record_dunder_hit(rip, cache, b, rname, crate::vm::sites::REFLECTED);
            }
            self.push(r);
            return Ok(());
        }

        // A comparison whose instance's dunder declined still learns it, so the relowered op calls its section, declining to the reflected one laid too.
        if matches!(self.heap.try_get(a), Some(HeapObj::Instance(..))) && let Some(name) = compare_dunder_name(op) { self.record_compare_hit(op, rip, cache, a, name); }

        // Set/Set uses subset/superset, NOT total order, the numeric `LtEq = !lt_vals(b, a)` identity is wrong here ({1,2} <= {2,3} would come back True), so we bypass `lt_vals`.
        if self.is_set_like(a) && self.is_set_like(b) { return self.set_compare_and_push(a, b, op); }
        if let Some((x, y)) = self.view_set_operands(a, b, false, chunk)? { return self.set_compare_and_push(x, y, op); }

        // An instance whose `__eq__` and the other's both declined, or neither has, is equal only to itself, its dunders run once.
        let inst = |v: Val| matches!(self.heap.try_get(v), Some(HeapObj::Instance(..)));
        let result = match op {
            OpCode::Eq | OpCode::NotEq if inst(a) || inst(b) => (a.0 == b.0) == (op == OpCode::Eq),
            OpCode::Eq => self.values_eq_op(a, b, chunk, rip, rerun)?,
            OpCode::NotEq => !self.values_eq_op(a, b, chunk, rip, rerun)?,
            OpCode::Lt => self.values_lt_op(a, b, chunk, rip, rerun, [a, b]).map_err(|e| self.ordering_err(e, "<", a, b))?,
            OpCode::Gt => self.values_lt_op(b, a, chunk, rip, rerun, [a, b]).map_err(|e| self.ordering_err(e, ">", a, b))?,
            OpCode::LtEq => !self.values_lt_op(b, a, chunk, rip, rerun, [a, b]).map_err(|e| self.ordering_err(e, "<=", a, b))?,
            OpCode::GtEq => !self.values_lt_op(a, b, chunk, rip, rerun, [a, b]).map_err(|e| self.ordering_err(e, ">=", a, b))?,
            _ => return Err(cold_runtime("non-compare opcode in handle_compare")),
        };
        self.push(Val::bool(result));
        Ok(())
    }

    /* An ordering `a op b` neither operand supports, named with the operator written and the operands in order. */
    #[cold]
    fn ordering_err(&self, e: VmErr, op: &str, a: Val, b: Val) -> VmErr {
        match e {
            VmErr::TypeMsg(m) if m.starts_with("'<' not supported between instances") => VmErr::TypeMsg(crate::s!("'", str op, "' not supported between instances of '", str self.type_name(a), "' and '", str self.type_name(b), "'")),
            e => e,
        }
    }

    // Only plain `not`, And/Or are short-circuited by the parser via Jump-If-Or-Pop.
    pub(crate) fn handle_logic(&mut self, op: OpCode, chunk: &SSAChunk) -> Result<(), VmErr> {
        match op {
            OpCode::Not => {
                let v = self.pop()?;
                let t = self.truthy_op(v, chunk)?;
                self.push(Val::bool(!t));
            }
            _ => return Err(cold_runtime("non-logic opcode in handle_logic")),
        }
        Ok(())
    }

    /* `is` / `is not` compare tag bits inline, `in` / `not in` delegate to contains(). */
    pub(crate) fn handle_identity(&mut self, op: OpCode, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;
        // A generator a test steps runs as a frame of the loop to the first item equal to the value, or to its end.
        if matches!(op, OpCode::In | OpCode::NotIn) && b.is_heap() && matches!(self.heap.get(b), HeapObj::Coroutine(..) | HeapObj::Instance(..))
            && let Some(g) = if matches!(self.heap.get(b), HeapObj::Coroutine(..)) { Some(b) } else if matches!(self.heap.get(b), &HeapObj::Instance(c, _) if self.lookup_class_member(c, "__contains__").is_some()) { None } else { self.with_roots([a], |vm| vm.iter_of_user(b, chunk))? } {
            if self.with_roots([a, g], |vm| vm.gather_job(g, crate::vm::jobs::Takes::Contains(a, op == OpCode::NotIn), Vec::new()))? { return Ok(()); }
            // the test steps the iterator the instance gave
            if g.0 != b.0 && matches!(self.heap.try_get(g), Some(HeapObj::Iter(..))) && self.with_roots([a, g], |vm| vm.contains_job(g, crate::vm::jobs::Takes::Contains(a, op == OpCode::NotIn), chunk))? { return Ok(()); }
            if g.0 != b.0 || matches!(self.heap.try_get(g), Some(HeapObj::Instance(..))) {
                let user = matches!(self.heap.try_get(g), Some(HeapObj::Instance(..)));
                let hit = self.with_roots([a, g], |vm| if user { vm.iter_contains(b, g, a, chunk) } else { vm.contains_op(g, a, chunk) })?;
                self.push(Val::bool(hit == (op == OpCode::In)));
                return Ok(());
            }
        }
        // A `map` or `filter` whose steps run Python steps as a job too, the `__eq__` of its items frames of the loop.
        if matches!(op, OpCode::In | OpCode::NotIn) && matches!(self.heap.try_get(b), Some(HeapObj::Iter(..)))
            && self.with_roots([a], |vm| vm.contains_job(b, crate::vm::jobs::Takes::Contains(a, op == OpCode::NotIn), chunk))? { return Ok(()); }
        let result = match op {
            OpCode::In => self.contains_op(b, a, chunk)?,
            OpCode::NotIn => !self.contains_op(b, a, chunk)?,
            OpCode::Is => a.0 == b.0,
            OpCode::IsNot => a.0 != b.0,
            _ => return Err(cold_runtime("non-identity opcode in handle_identity")),
        };
        self.push(Val::bool(result));
        Ok(())
    }
}

/* The error of a `%` format that runs out of the `got` arguments it was given. */
#[cold]
fn short_of(got: usize) -> VmErr { VmErr::TypeMsg(crate::s!("not enough arguments for format string (got ", int got as i64, ")")) }

/* An operator error of `a op= b`, which names the augmented operator. */
#[cold]
fn inplace_err(e: VmErr) -> VmErr {
    match e {
        VmErr::TypeMsg(m) if m.starts_with("unsupported operand type(s) for ") => match m.split_once(": ") {
            Some((head, tail)) => VmErr::TypeMsg(crate::s!(str head, "=: ", str tail)),
            None => VmErr::TypeMsg(m),
        },
        e => e,
    }
}

/* `str % args` while the dunders of the instances it formats run, with the text, the values, those converted and the conversions left in order. */
struct Percent { fmt: String, arg: Val, args: alloc::vec::Vec<Val>, done: u64, plan: alloc::vec::Vec<(usize, crate::vm::jobs::Check)>, k: usize }

impl crate::vm::jobs::Resume for Percent {
    fn resume(&mut self, vm: &mut VM<'_>, v: Option<Val>) -> Result<crate::vm::jobs::Next, VmErr> {
        if let Some(v) = v && let Some(&(at, _)) = self.plan.get(self.k.wrapping_sub(1)) { self.args[at] = v; self.done |= 1 << at; }
        if let Some(&(at, c)) = self.plan.get(self.k) { self.k += 1; return Ok(crate::vm::jobs::Next::Convert(self.args[at], c)); }
        let chunk = vm.chunk;
        let out = vm.percent_with(&self.fmt, self.arg, Some(&self.args), self.done, None, false, chunk)?;
        Ok(crate::vm::jobs::Next::Give(vm.heap.alloc(HeapObj::Str(out))?))
    }

    // A format converts the few values it holds.
    fn few(&self) -> bool { true }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.arg); for &v in &self.args { f(v) } }
}
