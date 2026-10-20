use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;
use crate::frontend::bigint::BigInt;
use crate::value::{DictMap, HeapObj, HeapPool, Host, IterFrame, Limits, Val, ValSet, VmErr};
use crate::vm::methods::{ALL_METHODS, Post, lookup_method, method_arity, post_result};

// Big holds only ints past i64 and sets compare without order.
#[derive(Clone, Debug)]
pub enum Plain { None, Bool(bool), Int(i64), Big(BigInt), Float(f64), Complex(f64, f64), Str(String), Bytes(Vec<u8>), ByteArray(Vec<u8>), List(Vec<Plain>), Tuple(Vec<Plain>), Dict(Vec<(Plain, Plain)>), FrozenDict(Vec<(Plain, Plain)>), Set(Vec<Plain>), FrozenSet(Vec<Plain>), Range(i64, i64, i64), Slice(alloc::boxed::Box<[Plain; 3]>) }

impl PartialEq for Plain {
    fn eq(&self, o: &Self) -> bool {
        use Plain::*;
        let unordered = |a: &[Plain], b: &[Plain]| a.len() == b.len() && a.iter().all(|x| b.contains(x));
        match (self, o) {
            (None, None) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Big(a), Big(b)) => a == b,
            // Two NaNs read as the same result.
            (Float(a), Float(b)) => a == b || a.is_nan() && b.is_nan(),
            (Complex(a, b), Complex(x, y)) => (a, b) == (x, y),
            (Str(a), Str(b)) => a == b,
            (Bytes(a), Bytes(b)) | (ByteArray(a), ByteArray(b)) => a == b,
            (List(a), List(b)) | (Tuple(a), Tuple(b)) => a == b,
            (Dict(a), Dict(b)) | (FrozenDict(a), FrozenDict(b)) => a.len() == b.len() && a.iter().all(|e| b.contains(e)),
            (Range(a, b, c), Range(x, y, z)) => (a, b, c) == (x, y, z),
            (Slice(a), Slice(b)) => a == b,
            (Set(a), Set(b)) | (FrozenSet(a), FrozenSet(b)) => unordered(a, b),
            _ => false,
        }
    }
}

// Class and message as the VM renders them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raised { pub class: String, pub message: String }

impl From<VmErr> for Raised {
    fn from(e: VmErr) -> Self { Self { class: e.class_name(), message: e.message() } }
}

pub type PlainFn = fn(&mut PlainHost, Val, &[Val]) -> Result<(), VmErr>;

// A heap without user code so every iterable is a builtin container.
pub struct PlainHost { heap: HeapPool, stack: Vec<Val>, budget: u64 }

impl Host for PlainHost {
    #[inline] fn heap(&self) -> &HeapPool { &self.heap }
    #[inline] fn heap_mut(&mut self) -> &mut HeapPool { &mut self.heap }
    #[inline] fn push(&mut self, v: Val) { self.stack.push(v); }
    #[inline] fn charge(&mut self, _n: usize) -> Result<(), VmErr> { Ok(()) }
}

impl crate::vm::natives::Frames for PlainHost {
    type Code = ();
    fn last(&self) -> Option<Val> { self.stack.last().copied() }
    fn stack(&mut self) -> &mut Vec<Val> { &mut self.stack }
    fn budget_mut(&mut self) -> &mut u64 { &mut self.budget }
    fn heap_budget(&mut self) -> (&HeapPool, &mut u64) { (&self.heap, &mut self.budget) }
    fn pop(&mut self) -> Result<Val, VmErr> { self.stack.pop().ok_or_else(|| crate::value::cold_runtime("stack underflow")) }
    fn pop_n(&mut self, n: usize) -> Result<Vec<Val>, VmErr> {
        let at = self.stack.len().checked_sub(n).ok_or_else(|| crate::value::cold_runtime("stack underflow"))?;
        Ok(self.stack.split_off(at))
    }
}

impl Default for PlainHost {
    fn default() -> Self {
        let mut heap = HeapPool::new(Limits::sandbox().memory);
        crate::frontend::install(&mut heap);
        Self { heap, stack: Vec::new(), budget: Limits::sandbox().ops }
    }
}

impl PlainHost {
    fn val(&mut self, p: &Plain) -> Result<Val, VmErr> {
        let items = |h: &mut Self, xs: &[Plain]| xs.iter().map(|x| h.val(x)).collect::<Result<Vec<_>, _>>();
        Ok(match p {
            Plain::None => Val::none(),
            Plain::Bool(b) => Val::bool(*b),
            Plain::Int(i) => self.heap.int(*i as i128)?,
            Plain::Big(b) => crate::frontend::int::val(&mut self.heap, b.clone())?,
            Plain::Float(f) => Val::float(*f),
            &Plain::Complex(re, im) => crate::frontend::complex::val(&mut self.heap, re, im)?,
            Plain::Str(s) => self.heap.alloc(HeapObj::Str(s.clone()))?,
            Plain::Bytes(b) => self.heap.alloc(HeapObj::Bytes(b.clone()))?,
            Plain::ByteArray(b) => self.heap.alloc(HeapObj::ByteArray(b.clone()))?,
            Plain::List(xs) => { let v = items(self, xs)?; self.alloc_list(v)? }
            Plain::Tuple(xs) => { let v = items(self, xs)?; self.heap.alloc(HeapObj::Tuple(v))? }
            Plain::Dict(es) | Plain::FrozenDict(es) => {
                let mut d = DictMap::with_capacity(es.len());
                for (k, v) in es { let (k, v) = (self.val(k)?, self.val(v)?); d.insert(k, v, &self.heap); }
                let d = Rc::new(RefCell::new(d));
                self.heap.alloc(if matches!(p, Plain::Dict(_)) { HeapObj::Dict(d) } else { HeapObj::FrozenDict(d) })?
            }
            &Plain::Range(a, b, c) => self.heap.alloc(HeapObj::Range(a, b, c))?,
            Plain::Slice(parts) => { let [a, b, c] = &**parts; let (a, b, c) = (self.val(a)?, self.val(b)?, self.val(c)?); self.heap.alloc(HeapObj::Slice(a, b, c))? }
            Plain::Set(xs) => { let v = items(self, xs)?; let s = ValSet::from_vals(&v, &self.heap); self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))? }
            Plain::FrozenSet(xs) => { let v = items(self, xs)?; let s = ValSet::from_vals(&v, &self.heap); self.heap.alloc(HeapObj::FrozenSet(Rc::new(s)))? }
        })
    }

    fn plain(&mut self, v: Val) -> Result<Plain, VmErr> {
        if v.is_none() { return Ok(Plain::None); }
        if v.is_bool() { return Ok(Plain::Bool(v.as_bool())); }
        if v.is_int() { return Ok(Plain::Int(v.as_int())); }
        if v.is_float() { return Ok(Plain::Float(v.as_float())); }
        if crate::frontend::complex::is_complex(&self.heap, v) && let Some(z) = crate::frontend::complex::parts(&self.heap, v) { return Ok(Plain::Complex(z.re, z.im)); }
        if let Some(b) = crate::frontend::int::of_val(&self.heap, v) { return Ok(b.to_i128().and_then(|i| i64::try_from(i).ok()).map_or(Plain::Big(b), Plain::Int)); }
        Ok(match self.heap.try_get(v) {
            Some(HeapObj::Str(s)) => Plain::Str(s.clone()),
            Some(HeapObj::Bytes(b)) => Plain::Bytes(b.clone()),
            Some(HeapObj::ByteArray(b)) => Plain::ByteArray(b.clone()),
            Some(HeapObj::List(l)) => { let xs = l.borrow().clone(); Plain::List(self.all(&xs)?) }
            Some(HeapObj::Tuple(t)) => { let xs = t.clone(); Plain::Tuple(self.all(&xs)?) }
            Some(HeapObj::Dict(d)) => { let es: Vec<(Val, Val)> = d.borrow().iter().collect(); Plain::Dict(self.pairs(&es)?) }
            Some(HeapObj::FrozenDict(d)) => { let es: Vec<(Val, Val)> = d.borrow().iter().collect(); Plain::FrozenDict(self.pairs(&es)?) }
            Some(&HeapObj::Range(a, b, c)) => Plain::Range(a, b, c),
            Some(&HeapObj::Slice(a, b, c)) => Plain::Slice(alloc::boxed::Box::new([self.plain(a)?, self.plain(b)?, self.plain(c)?])),
            // A builtin iterator reads to its end as a list.
            Some(HeapObj::Iter(frame, _)) if matches!(&*frame.borrow(), IterFrame::Seq { .. } | IterFrame::Watched { .. } | IterFrame::List { .. } | IterFrame::ListRev { .. }
                | IterFrame::Range { .. } | IterFrame::BigRange { .. } | IterFrame::DictWalk { .. } | IterFrame::Zip { .. } | IterFrame::Enumerate { .. }) => {
                let mut xs = Vec::new();
                while let Some(x) = crate::vm::natives::Frames::next_of(self, v, &())? { xs.push(x); }
                Plain::List(self.all(&xs)?)
            }
            Some(HeapObj::Set(s)) => { let xs: Vec<Val> = s.borrow().iter().copied().collect(); Plain::Set(self.all(&xs)?) }
            Some(HeapObj::FrozenSet(s)) => { let xs: Vec<Val> = s.iter().copied().collect(); Plain::FrozenSet(self.all(&xs)?) }
            _ => return Err(VmErr::TypeMsg(crate::s!("a '", str self.type_name(v), "' is not a plain value"))),
        })
    }

    fn all(&mut self, xs: &[Val]) -> Result<Vec<Plain>, VmErr> { xs.iter().map(|&x| self.plain(x)).collect() }

    fn pairs(&mut self, es: &[(Val, Val)]) -> Result<Vec<(Plain, Plain)>, VmErr> { es.iter().map(|&(k, v)| Ok((self.plain(k)?, self.plain(v)?))).collect() }

    // A method is `type.name` with its receiver first.
    pub fn call(&mut self, f: &str, args: &[Plain]) -> Result<Plain, Raised> {
        let Some((ty, name)) = f.split_once('.') else { return self.call_native(f, args) };
        let id = method_of(ty, name).ok_or_else(|| not_pure(f))?;
        let m = &ALL_METHODS[id.raw() as usize];
        let body = m.plain.ok_or_else(|| not_pure(f))?;
        // Methods the type answers itself take no receiver.
        let (recv, rest) = if matches!(name, "maketrans" | "fromkeys" | "fromhex" | "from_bytes" | "from_number") { (Val::none(), args) } else {
            let (recv, rest) = args.split_first().ok_or_else(|| Raised::from(VmErr::TypeMsg(crate::s!("unbound method ", str ty, ".", str name, "() needs an argument"))))?;
            let recv = self.val(recv)?;
            if self.type_name(recv) != ty {
                return Err(VmErr::TypeMsg(crate::s!("descriptor '", str name, "' for '", str ty, "' objects doesn't apply to a '", str self.type_name(recv), "' object")).into());
            }
            (recv, rest)
        };
        let pos = rest.iter().map(|a| self.val(a)).collect::<Result<Vec<_>, _>>()?;
        method_arity(id, pos.len())?;
        body(self, recv, &pos)?;
        let out = self.stack.pop().ok_or_else(|| not_pure(f))?;
        let out = if m.post == Post::None { out } else { post_result(self, m.post, out)? };
        Ok(self.plain(out)?)
    }

    // Arguments go on the stack as a call pushes them.
    fn call_native(&mut self, f: &str, args: &[Plain]) -> Result<Plain, Raised> {
        let id = crate::value::NativeFnId::from_name(f).filter(|id| id.pure()).ok_or_else(|| not_pure(f))?;
        let n = args.len() as u16;
        if !id.takes(n) { return Err(crate::vm::opcodes::function::arity_err(id, n).into()); }
        for a in args { let v = self.val(a)?; self.stack.push(v); }
        id.plain(self, n).ok_or_else(|| not_pure(f))??;
        let out = self.stack.pop().ok_or_else(|| not_pure(f))?;
        Ok(self.plain(out)?)
    }
}

// A dunder of a builtin value resolves among the operators its type answers.
fn method_of(ty: &str, name: &str) -> Option<crate::vm::BuiltinMethodId> {
    lookup_method(ty, name).or_else(|| crate::vm::methods::operator::applies(ty, name).then(|| lookup_method("op", name)).flatten())
}

fn not_pure(f: &str) -> Raised { Raised { class: "NotImplementedError".into(), message: crate::s!(str f, " has no pure form") } }

// Whether call_pure runs builtin `f`.
pub fn is_pure(f: &str) -> bool {
    match f.split_once('.') {
        Some((ty, name)) => method_of(ty, name).is_some_and(|id| ALL_METHODS[id.raw() as usize].plain.is_some()),
        None => crate::value::NativeFnId::from_name(f).is_some_and(|id| id.pure()),
    }
}

// The same body the VM runs with no VM, lexer or parser.
pub fn call_pure(f: &str, args: &[Plain]) -> Result<Plain, Raised> { PlainHost::default().call(f, args) }
