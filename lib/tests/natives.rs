use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use edge::code::SSAChunk;
use edge::embed::{call_native, Limits, VM};
use edge::lang::{kind_of, KindId, Native, Natives, Runtime};
use edge::value::{as_i128, DictMap, HeapObj, Val, ValSet};
use lib::data::{csv, json, lines};
use mini::Mini;
use mini::natives::{ARRAY, ATTR, BUFFER, BYTES, CONVERT, DATE, DICT, FLOAT, FOLDS, GEN, INT, IO, ITER, LIST, LIST_SORT, NEXT, NUM, RANGE, SET, SET_MANY, SORT, STR, TUPLE, TYPES};
use serde_yaml_ng::Value as Yaml;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// the kernels mini leaves out under names of this file so every module answers
const MORE: &[Native] = &lib::table![Mini;
    "lines" => lines::lines,
    "json_read" => json::read,
    "json_write" => json::write,
    "csv_read" => csv::read("line", "sep"),
    "csv_write" => csv::write("values", "sep"),
    "top" => lib::seq::top::top{"key"},
];

static NATIVES: Natives = edge::natives! {
    language: Over,
    fns: [IO, ITER, NEXT, NUM, FOLDS, CONVERT, BYTES, SORT, ATTR, TYPES, DATE, MORE],
    methods: [
        Str => [STR],
        Bytes => [BUFFER],
        ByteArray => [ARRAY],
        List => [LIST, LIST_SORT],
        Tuple => [TUPLE],
        Dict => [DICT],
        Set => [SET, SET_MANY],
        Int => [INT],
        Float => [FLOAT],
        Range => [RANGE],
        Coro => [GEN],
    ],
    types: mini::types::NAMES,
};

static RUNTIME: Runtime = Runtime::of::<Over>();

// mini's language over these natives
struct Over;
mini::over!(Over, &NATIVES);

// the modules of lib whose kernels these tables name each with a file of cases
const MODULES: &[&str] = &["bytes", "data", "date", "fmt", "io", "iter", "map", "num", "reflect", "seq", "set", "text", "types"];

const FIELDS: &[&str] = &["native", "recv", "args", "kw", "ret", "prints", "out", "error"];

/* one call of a native with what it must give */
struct Case {
    native: String,
    recv: Option<Yaml>,
    args: Vec<Yaml>,
    kw: Vec<(String, Yaml)>,
    ret: Option<Yaml>,
    prints: Option<String>,
    out: Option<String>,
    error: Option<String>,
}

fn text(m: &serde_yaml_ng::Mapping, key: &str) -> Result<Option<String>, String> {
    match m.get(key) {
        None => Ok(None),
        Some(Yaml::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("{key} holds {other:?} and not a text")),
    }
}

fn case_of(y: &Yaml) -> Result<Case, String> {
    let Yaml::Mapping(m) = y else { return Err("a case is a map of its fields".into()) };
    if let Some(k) = m.keys().find(|k| !k.as_str().is_some_and(|k| FIELDS.contains(&k))) { return Err(format!("no field {k:?}")); }
    let args = match m.get("args") { None => Vec::new(), Some(Yaml::Sequence(s)) => s.clone(), Some(_) => return Err("args is a list".into()) };
    let kw = match m.get("kw") {
        None => Vec::new(),
        Some(Yaml::Mapping(k)) => k.iter().map(|(n, v)| n.as_str().map(|n| (n.to_string(), v.clone())).ok_or("a keyword is named by a text")).collect::<Result<_, _>>()?,
        Some(_) => return Err("kw is a map".into()),
    };
    let case = Case { native: text(m, "native")?.ok_or("a case names its native")?, recv: m.get("recv").cloned(), args, kw, ret: m.get("ret").cloned(), prints: text(m, "prints")?, out: text(m, "out")?, error: text(m, "error")? };
    if case.ret.is_none() && case.prints.is_none() && case.out.is_none() && case.error.is_none() { return Err("a case holds a ret, prints, out or error".into()); }
    if case.error.is_some() && (case.ret.is_some() || case.prints.is_some()) { return Err("a case that raises returns nothing".into()); }
    Ok(case)
}

/* the hex text of a bytes tag as its bytes */
fn hex(s: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) { return Err(format!("{s:?} is not hex")); }
    digits.chunks(2).map(|p| std::str::from_utf8(p).ok().and_then(|p| u8::from_str_radix(p, 16).ok()).ok_or(format!("{s:?} is not hex"))).collect()
}

fn items(t: &serde_yaml_ng::value::TaggedValue) -> Result<&Vec<Yaml>, String> {
    match &t.value { Yaml::Sequence(s) => Ok(s), other => Err(format!("{} holds {other:?} and not a list", t.tag)) }
}

/* the pairs of a dict tag each a key and its value */
fn pairs(t: &serde_yaml_ng::value::TaggedValue) -> Result<Vec<(&Yaml, &Yaml)>, String> {
    items(t)?.iter().map(|p| match p { Yaml::Sequence(kv) if kv.len() == 2 => Ok((&kv[0], &kv[1])), _ => Err(format!("{p:?} is no pair")) }).collect()
}

/* the value a case spells built on the heap of `vm` */
fn value(vm: &mut VM<'_>, y: &Yaml) -> Result<Val, String> {
    let err = |e: edge::value::VmErr| e.to_string();
    Ok(match y {
        Yaml::Null => Val::none(),
        Yaml::Bool(b) => Val::bool(*b),
        Yaml::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => vm.heap_mut().int(i.into()).map_err(err)?,
            (None, Some(u)) => vm.heap_mut().int(u.into()).map_err(err)?,
            _ => Val::float(n.as_f64().ok_or("a number past every kind")?),
        },
        Yaml::String(s) => vm.heap_mut().alloc(HeapObj::Str(s.clone())).map_err(err)?,
        Yaml::Sequence(s) => {
            let items = s.iter().map(|y| value(vm, y)).collect::<Result<Vec<_>, _>>()?;
            vm.heap_mut().alloc(HeapObj::List(Rc::new(RefCell::new(items)))).map_err(err)?
        }
        Yaml::Mapping(m) => dict(vm, m.iter().collect())?,
        Yaml::Tagged(t) => match t.tag.to_string().as_str() {
            "!tuple" => {
                let items = items(t)?.iter().map(|y| value(vm, y)).collect::<Result<Vec<_>, _>>()?;
                vm.heap_mut().alloc(HeapObj::Tuple(items)).map_err(err)?
            }
            "!set" => {
                let items = items(t)?.iter().map(|y| value(vm, y)).collect::<Result<Vec<_>, _>>()?;
                let set = ValSet::build(&items, vm.heap()).map_err(err)?;
                vm.heap_mut().alloc(HeapObj::Set(Rc::new(RefCell::new(set)))).map_err(err)?
            }
            "!dict" => dict(vm, pairs(t)?)?,
            "!bytes" | "!bytearray" => {
                let Yaml::String(s) = &t.value else { return Err(format!("{} holds hex text", t.tag)) };
                let b = hex(s)?;
                vm.heap_mut().alloc(if t.tag == "bytearray" { HeapObj::ByteArray(b) } else { HeapObj::Bytes(b) }).map_err(err)?
            }
            "!int" => {
                let Yaml::String(s) = &t.value else { return Err("an int tag holds its digits as text".into()) };
                match s.parse::<i128>() {
                    Ok(i) => vm.heap_mut().int(i).map_err(err)?,
                    // past 128 bits the runtime's own int reads the digits
                    Err(_) => {
                        let digits = vm.heap_mut().alloc(HeapObj::Str(s.clone())).map_err(err)?;
                        call_native(vm, "int", Val::undef(), &[digits], &[]).map_err(err)?.ok_or("int opened a frame")?
                    }
                }
            }
            other => return Err(format!("no tag {other}")),
        },
    })
}

fn dict(vm: &mut VM<'_>, entries: Vec<(&Yaml, &Yaml)>) -> Result<Val, String> {
    let mut d = DictMap::new();
    for (k, v) in entries {
        let (k, v) = (value(vm, k)?, value(vm, v)?);
        d.insert(k, v, vm.heap());
    }
    vm.heap_mut().alloc(HeapObj::Dict(Rc::new(RefCell::new(d)))).map_err(|e| e.to_string())
}

/* the values a list tuple or set holds and the entries of a dict */
fn parts(vm: &VM<'_>, v: Val) -> (Vec<Val>, Vec<(Val, Val)>) {
    match vm.heap().try_get(v) {
        Some(HeapObj::List(l)) => (l.borrow().clone(), Vec::new()),
        Some(HeapObj::Tuple(t)) => (t.clone(), Vec::new()),
        Some(HeapObj::Set(s)) => (s.borrow().iter().copied().collect(), Vec::new()),
        Some(HeapObj::Dict(d)) => (Vec::new(), d.borrow().iter().collect()),
        _ => (Vec::new(), Vec::new()),
    }
}

fn all_same(vm: &mut VM<'_>, got: &[Val], want: &[Yaml]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(&v, y)| same(vm, v, y))
}

/* whether each value of `want` matches one of `got` and none is left over */
fn members(vm: &mut VM<'_>, mut got: Vec<Val>, want: &[Yaml]) -> bool {
    if got.len() != want.len() { return false; }
    for y in want {
        let Some(i) = (0..got.len()).find(|&i| same(vm, got[i], y)) else { return false };
        got.swap_remove(i);
    }
    true
}

fn same_entries(vm: &mut VM<'_>, got: &[(Val, Val)], want: &[(&Yaml, &Yaml)]) -> bool {
    got.len() == want.len() && got.iter().zip(want).all(|(&(k, v), &(yk, yv))| same(vm, k, yk) && same(vm, v, yv))
}

/* whether `v` is what a case spells with floats by bits and sets by membership */
fn same(vm: &mut VM<'_>, v: Val, y: &Yaml) -> bool {
    let kind = kind_of(vm.heap(), v);
    match y {
        Yaml::Null => v.is_none(),
        Yaml::Bool(b) => kind == KindId::Bool && v.as_bool() == *b,
        Yaml::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => kind == KindId::Int && as_i128(v, vm.heap()) == Some(i.into()),
            (None, Some(u), _) => kind == KindId::Int && as_i128(v, vm.heap()) == Some(u.into()),
            (_, _, Some(f)) => kind == KindId::Float && v.as_float().to_bits() == Val::float(f).as_float().to_bits(),
            _ => false,
        },
        Yaml::String(s) => matches!(vm.heap().try_get(v), Some(HeapObj::Str(t)) if t == s),
        Yaml::Sequence(s) => kind == KindId::List && { let (got, _) = parts(vm, v); all_same(vm, &got, s) },
        Yaml::Mapping(m) => kind == KindId::Dict && { let (_, got) = parts(vm, v); same_entries(vm, &got, &m.iter().collect::<Vec<_>>()) },
        Yaml::Tagged(t) => match t.tag.to_string().as_str() {
            "!tuple" => kind == KindId::Tuple && items(t).is_ok_and(|s| { let (got, _) = parts(vm, v); all_same(vm, &got, s) }),
            "!set" => kind == KindId::Set && items(t).is_ok_and(|s| { let (got, _) = parts(vm, v); members(vm, got, s) }),
            "!dict" => kind == KindId::Dict && pairs(t).is_ok_and(|p| { let (_, got) = parts(vm, v); same_entries(vm, &got, &p) }),
            "!bytes" | "!bytearray" => {
                let want = match &t.value { Yaml::String(s) => hex(s).ok(), _ => None };
                match vm.heap().try_get(v) {
                    Some(HeapObj::Bytes(b)) => t.tag == "bytes" && want.as_ref() == Some(b),
                    Some(HeapObj::ByteArray(b)) => t.tag == "bytearray" && want.as_ref() == Some(b),
                    _ => false,
                }
            }
            "!int" => kind == KindId::Int && matches!(&t.value, Yaml::String(s) if *s == vm.repr(v)),
            _ => false,
        },
    }
}

/* a value of a case on one line as its file spells it */
fn shown(y: &Yaml) -> String {
    let list = |s: &[Yaml]| s.iter().map(shown).collect::<Vec<_>>().join(", ");
    match y {
        Yaml::Null => "null".into(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Number(n) => n.to_string(),
        Yaml::String(s) => format!("{s:?}"),
        Yaml::Sequence(s) => format!("[{}]", list(s)),
        Yaml::Mapping(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", shown(k), shown(v))).collect::<Vec<_>>().join(", ")),
        Yaml::Tagged(t) => format!("{} {}", t.tag, shown(&t.value)),
    }
}

/* the lines `vm` printed since its output was last cleared */
fn printed(vm: &mut VM<'_>) -> Vec<String> { std::mem::take(&mut vm.output) }

fn lines_of(text: &str) -> Vec<String> { text.split('\n').map(String::from).collect() }

/* checks a case's call against what the case says naming the first mismatch */
fn check(vm: &mut VM<'_>, case: &Case) -> Result<(), String> {
    let recv = case.recv.as_ref().map_or(Ok(Val::undef()), |y| value(vm, y))?;
    let args = case.args.iter().map(|y| value(vm, y)).collect::<Result<Vec<_>, _>>()?;
    let kw = case.kw.iter().map(|(k, y)| value(vm, y).map(|v| (k.as_str(), v))).collect::<Result<Vec<_>, _>>()?;
    printed(vm);
    let called = call_native(vm, &case.native, recv, &args, &kw);
    let out = printed(vm);
    if let Some(want) = &case.out && out != lines_of(want) { return Err(format!("wrote {out:?} and not {:?}", lines_of(want))); }
    let got = match (called, &case.error) {
        (Ok(None), _) => return Err("its answer opened a frame so its case belongs with the engine cases".into()),
        (Ok(Some(v)), None) => v,
        (Ok(Some(v)), Some(want)) => return Err(format!("gave {} and raised no {want:?}", vm.repr(v))),
        (Err(e), Some(want)) => {
            let words = vm.error_text(&e);
            return if words.contains(want.as_str()) { Ok(()) } else { Err(format!("raised {words:?} and not {want:?}")) };
        }
        (Err(e), None) => return Err(format!("raised {:?}", vm.error_text(&e))),
    };
    if let Some(want) = &case.ret && !same(vm, got, want) {
        return Err(format!("gave {} and not {}", vm.repr(got), shown(want)));
    }
    if let Some(want) = &case.prints {
        let shown = call_native(vm, "print", Val::undef(), &[got], &[]).map_err(|e| vm.error_text(&e))?;
        let lines = printed(vm);
        if shown.is_none() || lines != lines_of(want) { return Err(format!("printed {lines:?} and not {:?}", lines_of(want))); }
    }
    Ok(())
}

/* each file of lib/tests/cases by its module with its cases */
fn files() -> Vec<(String, Vec<Yaml>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).expect("lib/tests/cases").map(|e| e.expect("a case file").path()).collect();
    paths.sort();
    paths.iter().map(|p| {
        let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        let cases: Vec<Yaml> = serde_yaml_ng::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        (p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), cases)
    }).collect()
}

#[test]
fn every_module_holds_its_cases() {
    let found: Vec<String> = files().into_iter().filter(|(_, cases)| !cases.is_empty()).map(|(m, _)| m).collect();
    assert_eq!(found, MODULES, "lib/tests/cases holds one file of cases for each module");
}

// one machine over every module runs each case and lists every mismatch
#[test]
fn natives_answer_as_their_cases_say() {
    let chunk = SSAChunk::default();
    let mut vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
    let (mut failures, mut ran) = (Vec::new(), 0);
    for (module, cases) in files() {
        for (i, y) in cases.iter().enumerate() {
            ran += 1;
            let checked = case_of(y).and_then(|case| {
                vm.reset_budget(Limits::sandbox().ops);
                check(&mut vm, &case).map_err(|why| format!("{} {why}", case.native))
            });
            let Err(why) = checked else { continue };
            failures.push(format!("{module}.yml case {i} {why}"));
            // a failed case may leave frames open so the next runs on a fresh machine
            vm = VM::with_runtime(&RUNTIME, &chunk, Limits::sandbox());
        }
    }
    assert!(ran > 0);
    assert!(failures.is_empty(), "{} of {ran} native cases failed\n{}", failures.len(), failures.join("\n"));
}
