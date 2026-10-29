use std::collections::BTreeMap;
use std::path::Path;

use edge::code::Value as Constant;
use edge::lexer::{Token, TokenType as T};
use serde_json::Value;

use crate::{respell, yml, Spell};

// where a vm case goes once the corpus splits
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Engine,
    Native,
    Syntax,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Engine => "engine",
            Kind::Native => "native",
            Kind::Syntax => "syntax",
        }
    }
}

// the natives a native case calls by name with the module of lib holding each
const FNS: &[(&str, &str)] = &[
    ("len", "seq"), ("range", "iter"), ("all", "iter"), ("any", "iter"),
    ("abs", "num"), ("round", "num"), ("divmod", "num"), ("pow", "num"), ("sum", "iter"),
    ("str", "text"), ("int", "num"), ("float", "num"), ("bool", "types"), ("list", "seq"),
    ("tuple", "seq"), ("dict", "map"), ("set", "set"), ("chr", "text"), ("ord", "text"),
    ("bin", "num"), ("oct", "num"), ("hex", "num"), ("repr", "fmt"), ("ascii", "fmt"),
    ("hash", "num"), ("callable", "types"), ("bytes", "bytes"), ("bytearray", "bytes"),
];

const TEXT: &[&str] = &[
    "encode", "upper", "lower", "strip", "capitalize", "title", "lstrip", "rstrip", "isdigit", "isalpha", "isalnum",
    "startswith", "endswith", "find", "count", "split", "join", "replace", "removeprefix", "removesuffix", "splitlines",
    "partition", "rpartition", "center", "zfill", "rsplit", "casefold", "swapcase", "ljust", "rjust", "expandtabs", "rfind",
    "index", "rindex", "isspace", "isupper", "islower", "istitle", "isascii", "isdecimal", "isnumeric", "isprintable",
];

const BYTES: &[&str] = &[
    "decode", "hex", "startswith", "endswith", "find", "index", "count", "replace", "split", "lower", "upper", "strip",
    "lstrip", "rstrip", "join", "fromhex", "rfind", "rindex", "rsplit", "splitlines", "partition", "rpartition",
    "removeprefix", "removesuffix", "center", "ljust", "rjust", "zfill", "expandtabs", "capitalize", "title", "swapcase",
    "isalnum", "isalpha", "isascii", "isdigit", "islower", "isspace", "istitle", "isupper",
];

const LIST: &[&str] = &["index", "count", "copy", "append", "clear", "reverse", "extend", "insert", "remove", "pop"];
const TUPLE: &[&str] = &["index", "count"];
const MAP: &[&str] = &["keys", "values", "items", "copy", "popitem", "get", "update", "pop", "setdefault", "fromkeys", "clear"];

const SET: &[&str] = &[
    "add", "remove", "discard", "pop", "clear", "update", "copy", "union", "intersection", "difference",
    "symmetric_difference", "intersection_update", "difference_update", "symmetric_difference_update", "issubset",
    "issuperset", "isdisjoint",
];

const INT: &[&str] = &["bit_length", "bit_count", "to_bytes", "from_bytes", "is_integer"];
const FLOAT: &[&str] = &["is_integer", "hex", "fromhex"];

// the module of lib that holds method `method` of `kind` where mini's table gives it
fn method_module(kind: &str, method: &str) -> Option<&'static str> {
    let (names, module) = match kind {
        "str" => (TEXT, "text"),
        "bytes" | "bytearray" => (BYTES, "bytes"),
        "int" | "bool" => (INT, "num"),
        "float" => (FLOAT, "num"),
        "list" => (LIST, "seq"),
        "tuple" => (TUPLE, "seq"),
        "dict" => (MAP, "map"),
        "set" => (SET, "set"),
        _ => return None,
    };
    let natives = &mini::NATIVES;
    let k = natives.names().kind_named(kind)?;
    let held = natives.method(k, method).or_else(|| (k == edge::lang::KindId::Bool).then(|| natives.method(edge::lang::KindId::Int, method)).flatten());
    if !names.contains(&method) || held.is_none() { return None; }
    Some(if kind == "str" && method == "encode" { "bytes" } else { module })
}

// a literal of a native call with ints held as their digits
#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Nil,
    Bool(bool),
    Int(String),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Lit>),
    Tuple(Vec<Lit>),
    Dict(Vec<(Lit, Lit)>),
    Set(Vec<Lit>),
}

impl Lit {
    // the kind a method call on the literal finds its method under
    fn kind(&self) -> Option<&'static str> {
        Some(match self {
            Lit::Str(_) => "str",
            Lit::Bytes(_) => "bytes",
            Lit::Int(_) => "int",
            Lit::Float(_) => "float",
            Lit::Bool(_) => "bool",
            Lit::List(_) => "list",
            Lit::Tuple(_) => "tuple",
            Lit::Dict(_) => "dict",
            Lit::Set(_) => "set",
            Lit::Nil => return None,
        })
    }

    fn negated(self) -> Option<Lit> {
        match self {
            Lit::Int(d) if d == "0" => Some(Lit::Int(d)),
            Lit::Int(d) => Some(Lit::Int(d.strip_prefix('-').map_or_else(|| format!("-{d}"), String::from))),
            Lit::Float(f) => Some(Lit::Float(-f)),
            _ => None,
        }
    }

    // whether a set may hold the literal or a map key by it
    fn hashable(&self) -> bool {
        match self {
            Lit::List(_) | Lit::Dict(_) | Lit::Set(_) => false,
            Lit::Tuple(items) => items.iter().all(Lit::hashable),
            _ => true,
        }
    }

    // whether the literal builds without raising as every set item and key hashes
    fn builds(&self) -> bool {
        match self {
            Lit::List(items) | Lit::Tuple(items) => items.iter().all(Lit::builds),
            Lit::Set(items) => items.iter().all(|i| i.hashable() && i.builds()),
            Lit::Dict(pairs) => pairs.iter().all(|(k, v)| k.hashable() && k.builds() && v.builds()),
            _ => true,
        }
    }

    fn scalar(&self) -> bool { matches!(self, Lit::Nil | Lit::Bool(_) | Lit::Float(_) | Lit::Str(_)) || matches!(self, Lit::Int(d) if d.parse::<i64>().is_ok()) }

    // the literal as the yaml of a native case
    pub fn yml(&self) -> String {
        let list = |items: &[Lit]| items.iter().map(Lit::yml).collect::<Vec<_>>().join(", ");
        match self {
            Lit::Nil => "null".into(),
            Lit::Bool(b) => b.to_string(),
            Lit::Int(d) if d.parse::<i64>().is_ok() => d.clone(),
            Lit::Int(d) => format!("!int {}", yml::quoted(d)),
            Lit::Float(f) if f.is_nan() => ".nan".into(),
            Lit::Float(f) if f.is_infinite() => if *f > 0.0 { ".inf".into() } else { "-.inf".into() },
            Lit::Float(f) => format!("{f:?}"),
            Lit::Str(s) => yml::quoted(s),
            Lit::Bytes(b) => format!("!bytes \"{}\"", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
            Lit::List(items) => format!("[{}]", list(items)),
            Lit::Tuple(items) => format!("!tuple [{}]", list(items)),
            Lit::Set(items) => format!("!set [{}]", list(items)),
            Lit::Dict(pairs) => {
                let keys: Vec<String> = pairs.iter().map(|(k, _)| k.yml()).collect();
                let distinct = keys.iter().collect::<std::collections::HashSet<_>>().len() == keys.len();
                // keys a yaml map cannot hold stay pairs in a list
                if distinct && pairs.iter().all(|(k, _)| k.scalar()) && keys.iter().all(|k| k.len() <= 256) {
                    format!("{{{}}}", pairs.iter().zip(&keys).map(|((_, v), k)| format!("{k}: {}", v.yml())).collect::<Vec<_>>().join(", "))
                } else {
                    format!("!dict [{}]", pairs.iter().map(|(k, v)| format!("[{}, {}]", k.yml(), v.yml())).collect::<Vec<_>>().join(", "))
                }
            }
        }
    }
}

// the decimal text of `digits` in `base` most significant first
fn decimal(digits: &[u8], base: u32) -> String {
    let mut out: Vec<u32> = vec![0];
    for &d in digits {
        let mut carry = d as u32;
        for x in out.iter_mut() { let v = *x * base + carry; (*x, carry) = (v % 10, v / 10); }
        while carry > 0 { out.push(carry % 10); carry /= 10; }
    }
    while out.len() > 1 && out.last() == Some(&0) { out.pop(); }
    out.iter().rev().map(|d| char::from(b'0' + *d as u8)).collect()
}

// the keywords of a call each by its name
pub type Keywords = Vec<(String, Lit)>;

// one call of a native on literals
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub native: String,
    pub module: &'static str,
    pub recv: Option<Lit>,
    pub args: Vec<Lit>,
    pub kw: Keywords,
}

// a native call on literals or a print of one or of a literal
enum Stmt {
    Printed(Call),
    Shown(Lit),
    Bare(Call),
}

// the tokens of one statement read as calls on literals
struct Calls<'a> {
    toks: &'a [Token],
    src: &'a str,
    spell: Spell,
    i: usize,
}

impl Calls<'_> {
    fn kind(&self, at: usize) -> T { self.toks.get(at).map_or(T::Eof, |t| t.kind) }

    fn text(&self, at: usize) -> &str { self.toks.get(at).map_or("", |t| &self.src[t.start..t.end]) }

    fn eat(&mut self, k: T) -> bool {
        let hit = self.kind(self.i) == k;
        self.i += hit as usize;
        hit
    }

    // the value tokens `a` to `b` hold as the compiler reads them
    fn constant(&self, a: usize, b: usize) -> Option<Lit> {
        let (chunk, errors) = self.spell.compile(&format!("x = {}\n", &self.src[self.toks[a].start..self.toks[b - 1].end]));
        if !errors.is_empty() || chunk.constants.len() != 1 { return None; }
        Some(match &chunk.constants[0] {
            Constant::Int(i) => Lit::Int(i.to_string()),
            Constant::Wide { neg, base, digits } => Lit::Int(format!("{}{}", if *neg { "-" } else { "" }, decimal(digits, u32::from(*base)))),
            Constant::Float(f) => Lit::Float(*f),
            Constant::Str(s) => Lit::Str(s.clone()),
            Constant::Bytes(b) => Lit::Bytes(b.clone()),
            _ => return None,
        })
    }

    // literals up to `close` with a comma after each but the last
    fn items(&mut self, close: T) -> Option<Vec<Lit>> {
        let mut out = Vec::new();
        while !self.eat(close) {
            out.push(self.lit()?);
            if !self.eat(T::Comma) && self.kind(self.i) != close { return None; }
        }
        Some(out)
    }

    fn lit(&mut self) -> Option<Lit> {
        let (at, k) = (self.i, self.kind(self.i));
        self.i += 1;
        Some(match k {
            T::Int | T::Float => self.constant(at, at + 1)?,
            T::Minus | T::Plus if matches!(self.kind(self.i), T::Int | T::Float) => {
                self.i += 1;
                let v = self.constant(at + 1, at + 2)?;
                if k == T::Minus { v.negated()? } else { v }
            }
            T::String | T::Bytes => {
                while self.kind(self.i) == k { self.i += 1; }
                self.constant(at, self.i)?
            }
            T::True => Lit::Bool(true),
            T::False => Lit::Bool(false),
            T::Nil => Lit::Nil,
            T::BracketOpen => Lit::List(self.items(T::BracketClose)?),
            T::ParenOpen => {
                if self.eat(T::ParenClose) { return Some(Lit::Tuple(Vec::new())); }
                let first = self.lit()?;
                if self.eat(T::ParenClose) { return Some(first); }
                if !self.eat(T::Comma) { return None; }
                let mut items = vec![first];
                items.extend(self.items(T::ParenClose)?);
                Lit::Tuple(items)
            }
            T::BraceOpen => {
                if self.eat(T::BraceClose) { return Some(Lit::Dict(Vec::new())); }
                let first = self.lit()?;
                if !self.eat(T::Colon) {
                    if !self.eat(T::Comma) && self.kind(self.i) != T::BraceClose { return None; }
                    let mut items = vec![first];
                    items.extend(self.items(T::BraceClose)?);
                    return Some(Lit::Set(items));
                }
                let mut pairs = vec![(first, self.lit()?)];
                while !self.eat(T::BraceClose) {
                    if !self.eat(T::Comma) { return None; }
                    if self.eat(T::BraceClose) { break; }
                    let key = self.lit()?;
                    if !self.eat(T::Colon) { return None; }
                    pairs.push((key, self.lit()?));
                }
                Lit::Dict(pairs)
            }
            _ => return None,
        })
    }

    // the positionals and keywords after `(` each a literal
    fn args(&mut self) -> Option<(Vec<Lit>, Keywords)> {
        let (mut pos, mut kw) = (Vec::new(), Vec::new());
        while !self.eat(T::ParenClose) {
            if self.kind(self.i) == T::Name && self.kind(self.i + 1) == T::Equal {
                let name = self.text(self.i).to_string();
                self.i += 2;
                kw.push((name, self.lit()?));
            } else if kw.is_empty() {
                pos.push(self.lit()?);
            } else {
                return None;
            }
            if !self.eat(T::Comma) && self.kind(self.i) != T::ParenClose { return None; }
        }
        Some((pos, kw))
    }

    // a native called by name or off its type or on a literal receiver
    fn call(&mut self) -> Option<Call> {
        let at = self.i;
        if self.kind(at) == T::Name && self.kind(at + 1) == T::Dot && self.kind(at + 2) == T::Name && self.kind(at + 3) == T::ParenOpen {
            let (kind, method) = (self.text(at).to_string(), self.text(at + 2).to_string());
            let module = method_module(&kind, &method)?;
            self.i = at + 4;
            let (args, kw) = self.args()?;
            return Some(Call { native: format!("{kind}.{method}"), module, recv: None, args, kw });
        }
        if self.kind(at) == T::Name && self.kind(at + 1) == T::ParenOpen {
            let name = self.text(at).to_string();
            let &(_, module) = FNS.iter().find(|(n, _)| *n == name)?;
            mini::NATIVES.find(&name)?;
            self.i = at + 2;
            let (args, kw) = self.args()?;
            return Some(Call { native: name, module, recv: None, args, kw });
        }
        let recv = self.lit()?;
        let kind = recv.kind()?;
        if !(self.eat(T::Dot) && self.kind(self.i) == T::Name && self.kind(self.i + 1) == T::ParenOpen) { return None; }
        let method = self.text(self.i).to_string();
        let module = method_module(kind, &method)?;
        self.i += 2;
        let (args, kw) = self.args()?;
        Some(Call { native: format!("{kind}.{method}"), module, recv: Some(recv), args, kw })
    }

    fn statement(&mut self) -> Option<Stmt> {
        let at = self.i;
        if self.kind(at) == T::Name && self.text(at) == "print" && self.kind(at + 1) == T::ParenOpen {
            self.i = at + 2;
            if let Some(call) = self.call() && self.eat(T::ParenClose) { return Some(Stmt::Printed(call)); }
            self.i = at + 2;
            let lit = self.lit()?;
            return self.eat(T::ParenClose).then_some(Stmt::Shown(lit));
        }
        self.call().map(Stmt::Bare)
    }
}

// each statement of `src` with its first line where all call natives on literals
fn statements(spell: Spell, src: &str) -> Option<Vec<(Stmt, usize)>> {
    let (toks, errors) = spell.lex(src);
    if errors > 0 { return None; }
    let toks: Vec<Token> = toks.into_iter().filter(|t| !matches!(t.kind, T::Comment | T::LineBreak | T::Eof)).collect();
    if toks.iter().any(|t| matches!(t.kind, T::Indent | T::Dedent | T::BlockOpen | T::BlockClose)) { return None; }
    let mut out = Vec::new();
    for stmt in toks.split(|t| matches!(t.kind, T::Newline | T::Semi)).filter(|s| !s.is_empty()) {
        let mut p = Calls { toks: stmt, src, spell, i: 0 };
        let s = p.statement()?;
        if p.i != stmt.len() { return None; }
        out.push((s, crate::line_of(src, stmt[0].start)));
    }
    Some(out)
}

// what a native case holds its call to
#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Prints(String),
    Out(String),
    Error(String),
}

// one native case a vm case gives
#[derive(Clone, Debug, PartialEq)]
pub struct NativeCase {
    pub call: Call,
    pub want: Want,
}

impl NativeCase {
    // the case as an entry of its module's file
    pub fn yml(&self) -> String {
        let c = &self.call;
        let mut out = format!("- native: {}\n", c.native);
        if let Some(r) = &c.recv { out += &format!("  recv: {}\n", r.yml()); }
        if !c.args.is_empty() { out += &format!("  args: [{}]\n", c.args.iter().map(Lit::yml).collect::<Vec<_>>().join(", ")); }
        if !c.kw.is_empty() { out += &format!("  kw: {{{}}}\n", c.kw.iter().map(|(k, v)| format!("{}: {}", yml::key(k), v.yml())).collect::<Vec<_>>().join(", ")); }
        let (field, text) = match &self.want { Want::Prints(t) => ("prints", t), Want::Out(t) => ("out", t), Want::Error(t) => ("error", t) };
        out + &format!("  {field}: {}\n", yml::quoted(text))
    }
}

// a compile time diagnostic of a vm case with any indent text spell cannot make
#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxCase {
    pub mini: String,
    pub indent: Option<String>,
    pub error: String,
}

impl SyntaxCase {
    pub fn yml(&self) -> String {
        let indent = self.indent.as_ref().map_or(String::new(), |t| format!("  indent: {}\n", yml::text(t, 4)));
        format!("- mini: {}\n{indent}  error: {}\n", yml::text(&self.mini, 4), yml::quoted(&self.error))
    }
}

// what one vm case becomes once the corpus splits
pub enum Fate {
    Engine,
    Native(Vec<NativeCase>),
    Syntax(SyntaxCase),
    // a case the rules send to native or syntax that stays for the reason given
    Kept(Kind, &'static str),
}

impl Fate {
    pub fn kind(&self) -> Kind {
        match self {
            Fate::Engine | Fate::Kept(..) => Kind::Engine,
            Fate::Native(_) => Kind::Native,
            Fate::Syntax(_) => Kind::Syntax,
        }
    }
}

// the native cases of a vm case's statements or why its lines do not pair
fn natives_of(stmts: Vec<(Stmt, usize)>, case: &Value) -> Result<Vec<NativeCase>, &'static str> {
    let error = case["error"].as_str();
    let lines: Vec<&str> = case["expected"].as_str().map_or(Vec::new(), |e| e.split('\n').collect());
    let n = stmts.len();
    // each statement before a raise prints one line and a lone one may print several
    let paired = match error { Some(_) => lines.len() + 1 == n, None => lines.len() == n || (n == 1 && !lines.is_empty()) };
    if !paired { return Err("its lines do not pair with its statements"); }
    if error.is_some() && case["error_line"].as_u64().is_some_and(|at| Some(at as usize) != stmts.last().map(|s| s.1)) { return Err("its error line is not the line of its last statement"); }
    let lits = |c: &Call| c.recv.iter().chain(&c.args).chain(c.kw.iter().map(|k| &k.1)).all(Lit::builds);
    if !stmts.iter().all(|(s, _)| match s { Stmt::Printed(c) | Stmt::Bare(c) => lits(c), Stmt::Shown(l) => l.builds() }) { return Err("a literal of it raises as it is built"); }
    let mut out = Vec::with_capacity(n);
    for (i, (stmt, _)) in stmts.into_iter().enumerate() {
        let want = match (error, i + 1 == n) {
            (Some(e), true) => Want::Error(e.to_string()),
            _ if n == 1 => Want::Prints(lines.join("\n")),
            _ => Want::Prints(lines[i].to_string()),
        };
        let (call, want) = match stmt {
            Stmt::Printed(call) => (call, want),
            Stmt::Bare(call) if matches!(want, Want::Error(_)) => (call, want),
            Stmt::Bare(_) => return Err("a bare call prints nothing to compare"),
            Stmt::Shown(lit) => {
                let want = match want { Want::Prints(t) => Want::Out(t), w => w };
                (Call { native: "print".into(), module: "io", recv: None, args: vec![lit], kw: Vec::new() }, want)
            }
        };
        out.push(NativeCase { call, want });
    }
    Ok(out)
}

// where a case goes by the rules of the split in spelling `spell`
pub fn fate(spell: Spell, case: &Value) -> Fate {
    let src = case["case"].as_str().unwrap_or("");
    let error = case["error"].as_str();
    if let Some(want) = error && spell.compile(src).1.iter().any(|m| m.contains(want)) {
        // a syntax case holds no line so a line past the first keeps its case
        if case["error_line"].as_u64().is_some_and(|l| l > 1) { return Fate::Kept(Kind::Syntax, "its error line names a line past the first"); }
        let reports = |text: &str| Spell::Indent.compile(text).1.iter().any(|m| m.contains(want));
        // a source the translator refuses may read the same in both tables
        let indent = match respell(spell, Spell::Indent, src) {
            Ok(text) if reports(&text) => None,
            _ if reports(src) => Some(src.to_string()),
            _ => return Fate::Kept(Kind::Syntax, "the indent spelling reports it in other words"),
        };
        return Fate::Syntax(SyntaxCase { mini: src.to_string(), indent, error: want.to_string() });
    }
    if ["input", "events", "interactive_events", "sends"].iter().any(|k| !case[*k].is_null()) { return Fate::Engine; }
    let Some(stmts) = statements(spell, src) else { return Fate::Engine };
    // a bare call counts only as the sole statement of a case expecting its error
    let bare = stmts.iter().any(|s| matches!(s.0, Stmt::Bare(_)));
    if stmts.is_empty() || (bare && !(error.is_some() && stmts.len() == 1)) { return Fate::Engine; }
    match natives_of(stmts, case) {
        Ok(cases) => Fate::Native(cases),
        Err(why) => Fate::Kept(Kind::Native, why),
    }
}

// one vm case with where it goes
pub struct Row {
    pub file: String,
    pub index: usize,
    pub line: usize,
    pub key: String,
    pub fate: Fate,
}

// where each case of a topic file starts at a dash in the first column
fn starts(text: &str) -> Vec<usize> {
    let mut out: Vec<usize> = text.match_indices("\n-").map(|(at, _)| at + 1).collect();
    if text.starts_with('-') { out.insert(0, 0); }
    out
}

// every vm case of `root` with its fate in file order
pub fn classify_tree(root: &Path, spell: Spell) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for (file, objects) in crate::cases::vm_files(root) {
        let text = std::fs::read_to_string(root.join("tests/cases/vm").join(&file)).map_err(|e| format!("{file} {e}"))?;
        let cases: Vec<Value> = serde_yaml_ng::from_str(&text).map_err(|e| format!("{file} {e}"))?;
        let at = starts(&text);
        if at.len() != cases.len() || objects.len() != cases.len() { return Err(format!("{file} holds {} cases where {} start at a dash", cases.len(), at.len())); }
        for (index, (case, object)) in cases.iter().zip(&objects).enumerate() {
            rows.push(Row { file: file.clone(), index, line: crate::line_of(&text, at[index]), key: crate::cases::key(object), fate: fate(spell, case) });
        }
    }
    Ok(rows)
}

// the files a split writes by path and the sampled cases it moved
pub struct Moved {
    pub files: Vec<(String, String)>,
    pub sampled: Vec<String>,
}

// the case files once each native and syntax case of `rows` leaves the vm files
pub fn moved(root: &Path, rows: &[Row]) -> Result<Moved, String> {
    let read = |rel: &str| std::fs::read_to_string(root.join(rel)).map_err(|e| format!("{rel} {e}"));
    let mut files = Vec::new();
    let mut natives: BTreeMap<&str, String> = BTreeMap::new();
    let mut syntax = String::new();
    // a blank line parts each case from the one before as in every case file
    let add = |file: &mut String, entry: String| { if !file.is_empty() { file.push('\n'); } file.push_str(&entry); };
    for r in rows {
        match &r.fate {
            Fate::Native(cases) => for c in cases { add(natives.entry(c.call.module).or_default(), c.yml()); },
            Fate::Syntax(c) => add(&mut syntax, c.yml()),
            _ => {}
        }
    }
    for (module, add) in &natives {
        let rel = format!("lib/tests/cases/{module}.yml");
        let old = read(&rel).unwrap_or_default();
        files.push((rel, joined(&old, add)?));
    }
    let rel = "tests/cases/syntax/diagnostics.yml";
    let old = read(rel).unwrap_or_default();
    files.push((rel.to_string(), joined(&old, &syntax)?));
    // the vm files lose the moved cases and keep every other byte
    let mut gone: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for r in rows.iter().filter(|r| r.fate.kind() != Kind::Engine) { gone.entry(r.file.as_str()).or_default().push(r.index); }
    for (file, indexes) in &gone {
        let rel = format!("tests/cases/vm/{file}");
        let text = read(&rel)?;
        let at = starts(&text);
        let mut out = String::with_capacity(text.len());
        for (i, &a) in at.iter().enumerate() {
            if !indexes.contains(&i) { out.push_str(&text[a..at.get(i + 1).copied().unwrap_or(text.len())]); }
        }
        // the case kept last ends the file as the last case did
        if indexes.contains(&(at.len() - 1)) && let Some(cut) = out.strip_suffix("\n\n") { out = format!("{cut}\n"); }
        let before: Vec<Value> = serde_yaml_ng::from_str(&text).map_err(|e| e.to_string())?;
        let after: Vec<Value> = serde_yaml_ng::from_str(&out).map_err(|e| format!("{rel} once cut {e}"))?;
        let kept: Vec<Value> = before.into_iter().enumerate().filter(|(i, _)| !indexes.contains(i)).map(|(_, c)| c).collect();
        if kept != after { return Err(format!("{rel} reads other cases once the moved ones are cut")); }
        files.push((rel, out));
    }
    let keys: Vec<String> = rows.iter().filter(|r| r.fate.kind() != Kind::Engine).map(|r| r.key.clone()).collect();
    let snapshot = read("bench/.snapshot")?;
    files.push(("bench/.snapshot".into(), crate::rekey::apply(&snapshot, &[], &keys)?.text));
    // the sample of Miri names each kept case at its new place
    let tsv = read("tests/cases/miri.tsv")?;
    let mut sampled = Vec::new();
    let mut out = String::new();
    for line in tsv.split_inclusive('\n') {
        let cols: Vec<&str> = line.trim_end_matches('\n').split('\t').collect();
        let Some(index) = cols.get(2).and_then(|i| i.parse::<usize>().ok()).filter(|_| cols.len() == 3) else { out.push_str(line); continue };
        let cut = gone.get(cols[1]).map_or(&[][..], Vec::as_slice);
        if cut.contains(&index) { sampled.push(line.trim_end().to_string()); continue; }
        out.push_str(&format!("{}\t{}\t{}\n", cols[0], cols[1], index - cut.iter().filter(|&&i| i < index).count()));
    }
    files.push(("tests/cases/miri.tsv".into(), out));
    Ok(Moved { files, sampled })
}

// a case file with `add` after its cases where both read back as they were
fn joined(old: &str, add: &str) -> Result<String, String> {
    let head = if old.trim().is_empty() { "" } else { old };
    let sep = if head.is_empty() || add.is_empty() { "" } else { "\n" };
    let text = format!("{head}{sep}{add}");
    let parse = |t: &str| serde_yaml_ng::from_str::<Option<Vec<serde_yaml_ng::Value>>>(t).map(Option::unwrap_or_default).map_err(|e| e.to_string());
    let (a, b, c) = (parse(head)?, parse(add)?, parse(&text)?);
    if c.len() != a.len() + b.len() || c[..a.len()] != a[..] || c[a.len()..] != b[..] { return Err("a case file reads otherwise once the split appends to it".into()); }
    Ok(text)
}
