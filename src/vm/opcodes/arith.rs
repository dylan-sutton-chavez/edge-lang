use super::*;
use crate::value::{BinOp, UnOp, unboxed};

use cache::OpcodeCache;

/* IC, same for comparison opcodes, reflected pairs collapse to the forward name. */
fn compare_dunder_name(op: OpCode) -> Option<&'static str> {
    super::dunder::compare_dunder_names(op).map(|(l, _)| l)
}

impl<'a> VM<'a> {

    /* Add/Sub/Mul/Div with IC, Mod/Pow/FloorDiv exact for ints of any size, Minus is unary. */
    pub(crate) fn handle_arith(&mut self, op: OpCode, operand: u16, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        if op == OpCode::Minus {
            return self.exec_unary(rip, cache, chunk, "__neg__", |v| Val::float(-v.as_float()), UnOp::Neg, "-");
        }
        if op == OpCode::Pos {
            // +float returns the value unchanged, bool drops its tag to int.
            return self.exec_unary(rip, cache, chunk, "__pos__", |v| v, UnOp::Pos, "+");
        }

        let (a, b) = self.pop2()?;
        let inplace = matches!(op, OpCode::InPlaceAdd | OpCode::InPlaceSub) || operand == crate::parser::INPLACE;

        // `name += rhs` extends a left list in place with any iterable (Python __iadd__). Other types behave as Add.
        let op = if op == OpCode::InPlaceAdd {
            // Guard is_heap since `i += 1` operands are inline ints, and heap.get on a non-heap Val indexes garbage.
            let a_is_list = a.is_heap() && matches!(self.heap.get(a), HeapObj::List(_));
            if a_is_list {
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

        let dunder = self.try_binary_dunder(op, a, b, inplace, chunk);

        // instance dunder protocol, try user-defined operator before any builtin coercion.
        if let Some(r) = dunder? {
            // record the resolved class+method so the IC can fire on subsequent iterations of a hot loop. Reflected ops deopt through `NotImplemented`.
            if let Some(name) = self.site_dunder_name(op, a, inplace) {
                self.record_dunder_hit(rip, cache, a, name, 2);
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
                  name: &'static str, ffl: fn(Val) -> Val, op: UnOp, sym: &'static str) -> Result<(), VmErr> {
        let v = self.pop()?;
        if let Some(r) = self.try_call_dunder(v, name, &[], chunk)? {
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
        let (args, spread): (alloc::vec::Vec<Val>, bool) = match self.heap.try_get(arg) {
            Some(HeapObj::Tuple(t)) => (t.clone(), true),
            _ => (alloc::vec![arg], false),
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
            let (conv, at) = (chars[i], i); i += 1;
            let val = match key {
                Some(k) => { keyed = true; self.format_key(arg, k, bytes, chunk)? }
                None if keyed => return Err(cold_value_s(crate::s!("format requires a parenthesised mapping key at position ", int start as i64))),
                None => { let v = *args.get(ai).ok_or_else(|| short_of(args.len()))?; ai += 1; v }
            };
            // A bad verb right after `%` is unsupported, one after flags or a width may end a stray `%`.
            let unsupported = || if at == start + 1 { cold_value_s(crate::s!("unsupported format %", char conv, " at position ", int start as i64)) }
                else { cold_value_s(crate::s!("stray % at position ", int start as i64, " or unexpected format character '", char conv, "' at position ", int at as i64)) };
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

    pub(crate) fn handle_compare(&mut self, op: OpCode, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        let (a, b) = self.pop2()?;

        let dunder = self.try_compare_dunder(op, a, b, chunk);

        // try the user-defined comparison dunder before falling back to numeric/string compare.
        if let Some(r) = dunder? {
            // monomorphic comparison sites cache the resolved method like arithmetic ones.
            if let Some(name) = compare_dunder_name(op) {
                self.record_dunder_hit(rip, cache, a, name, 2);
            }
            self.push(r);
            return Ok(());
        }

        // Set/Set uses subset/superset, NOT total order, the numeric `LtEq = !lt_vals(b, a)` identity is wrong here ({1,2} <= {2,3} would come back True), so we bypass `lt_vals`.
        if self.is_set_like(a) && self.is_set_like(b) { return self.set_compare_and_push(a, b, op); }
        if let Some((x, y)) = self.view_set_operands(a, b, false, chunk)? { return self.set_compare_and_push(x, y, op); }

        let result = match op {
            OpCode::Eq => self.values_eq(a, b, chunk)?,
            OpCode::NotEq => !self.values_eq(a, b, chunk)?,
            OpCode::Lt => self.values_lt(a, b, chunk).map_err(|e| self.ordering_err(e, "<", a, b))?,
            OpCode::Gt => self.values_lt(b, a, chunk).map_err(|e| self.ordering_err(e, ">", a, b))?,
            OpCode::LtEq => !self.values_lt(b, a, chunk).map_err(|e| self.ordering_err(e, "<=", a, b))?,
            OpCode::GtEq => !self.values_lt(a, b, chunk).map_err(|e| self.ordering_err(e, ">=", a, b))?,
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
