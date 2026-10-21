/* Reads `edge.ept`, the rules per opcode at one Edge commit, refusing a file of another commit or version or with a line it does not know. */

use alloc::{format, string::{String, ToString}, vec::Vec};
use crate::parser::OpCode;
use super::Tags;

// The hash of the sources every rule is about, this build's own (build.rs).
pub const SOURCES: &str = include_str!(concat!(env!("OUT_DIR"), "/sources.hash"));

/* What a popped value must be for its row to apply. */
#[derive(Clone, Debug)]
pub enum Pat { In(Tags), Type(u8), Int(i64), None, Bool(bool) }

/* An operand, an operand less one (a bound `a<j>-1`), or a number. */
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rhs { A(usize), Less(usize), N(i64) }

#[derive(Clone, Debug, PartialEq)]
pub enum Learn { In(Tags), Rel(&'static str, Rhs) }

/* What a row says of a result: its exact integer by `op` (`=`, `-`, `+`, `*`, or `n` a number) on operands, its range, that it is an item of an operand, a container of operands, or one whose items have these tags and bounds. */
#[derive(Clone, Debug, PartialEq)]
pub enum Out { Exact(char, Rhs, Rhs), Range(i64, i64), Item(usize), Items(Vec<usize>), Holds(Tags, Rhs, Rhs) }

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub ins: Vec<Pat>,
    // None for `-> !`, an opcode that always raises.
    pub outs: Option<Vec<Tags>>,
    pub raises: Tags,
    // Raises only when the exact result `r` leaves `lo..=hi`.
    pub unless: Option<(usize, i64, i64)>,
    pub says: Vec<(usize, Out)>,
    // On the truthy (true) or falsy side of `r0`, what operand `a` is.
    pub learn: Vec<(bool, usize, Learn)>,
    // Container `a` takes operand `b` among its items, None for one or items not known.
    pub puts: Vec<(Option<usize>, Option<usize>)>,
    // The operands it may call, a function or class handed to it, usize::MAX for any.
    pub calls: Vec<usize>,
}

#[derive(Default)]
pub struct Rules {
    // Each exact type by its code, the place of its name in BUILTIN_TYPES, the program's own kinds past them; and each `$<n>` set.
    pub tags: Vec<(String, u8)>,
    sets: Vec<(String, Tags)>,
    // By opcode name and operand, `type.name` of a method, or `Truth`.
    rows: Vec<(String, Option<u16>, Row)>,
    // The places of each name's rows, so a step finds them without reading the file's every row.
    by: crate::util::hash::FxHashMap<String, Vec<u32>>,
}

fn bad(w: &str, what: &str) -> String { format!("edge.ept: '{w}' is not {what}") }
fn int(w: &str) -> Result<i64, String> { w.parse().map_err(|_| bad(w, "an integer")) }
fn pos(w: &str, side: char) -> Result<usize, String> { w.strip_prefix(side).and_then(|n| n.parse().ok()).ok_or_else(|| bad(w, "a position")) }
fn rhs(w: &str, open: i64) -> Result<Rhs, String> {
    if w.is_empty() { return Ok(Rhs::N(open)); }
    if let Some(a) = w.strip_suffix("-1").filter(|a| a.starts_with('a')) { return pos(a, 'a').map(Rhs::Less); }
    if w.starts_with('a') { pos(w, 'a').map(Rhs::A) } else { int(w).map(Rhs::N) }
}
fn span(w: &str) -> Result<(Rhs, Rhs), String> {
    let (lo, hi) = w.split_once("..").ok_or_else(|| bad(w, "a range"))?;
    Ok((rhs(lo, i64::MIN)?, rhs(hi, i64::MAX)?))
}
fn num(r: Rhs) -> Result<i64, String> { if let Rhs::N(n) = r { Ok(n) } else { Err("edge.ept: an operand where a number goes".into()) } }

impl Rules {
    pub fn read(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
        if lines.next() != Some(SOURCES) { return Err("edge.ept is of other Edge sources".into()); }
        if lines.next() != Some("ept 1") { return Err("edge.ept is not format 1".into()); }
        let lines: Vec<Vec<&str>> = lines.map(|l| l.split(' ').collect()).collect();
        let mut r = Rules::default();
        // The tags first, as rows name them, then the named sets of them.
        for w in &lines { if let ["tag", n, c] = w[..] { r.tags.push((n.to_string(), u8::try_from(int(c)?).ok().filter(|&c| c < 127).ok_or_else(|| bad(c, "a code below 127"))?)); } }
        for w in &lines { if let ["set", n, t] = w[..] && n.starts_with('$') { let t = r.set(t)?; r.sets.push((n.to_string(), t)); } }
        let ops: Vec<String> = OpCode::ALL.iter().map(|o| format!("{o:?}")).collect();
        for w in &lines {
            let (name, at) = match w.get(1).map(|n| n.split_once('@')) { Some(Some((n, a))) => (n, Some(int(a)? as u16)), _ => (*w.get(1).unwrap_or(&""), None) };
            let op = name == "Truth" || ops.iter().any(|o| o == name);
            match w[0] {
                "tag" | "set" if w.len() == 3 => {}
                "rule" if op => { let row = r.row(&w[2..])?; r.rows.push((name.into(), at, row)); }
                "builtin" if w.len() > 2 && at.is_none() => { let row = r.row(&w[2..])?; r.rows.push((alloc::format!("builtin.{name}"), None, row)); }
                "method" if name.contains('.') && at.is_none() => { let row = r.row(&w[2..])?; r.rows.push((name.into(), None, row)); }
                _ => return Err(bad(&w.join(" "), "a line of format 1")),
            }
        }
        for (i, row) in r.rows.iter().enumerate() { r.by.entry(row.0.clone()).or_default().push(i as u32); }
        Ok(r)
    }

    pub fn set(&self, w: &str) -> Result<Tags, String> {
        match w {
            "*" => Ok(super::ALL),
            "!" => Ok(0),
            _ if w.starts_with('$') => self.sets.iter().find(|s| s.0 == w).map(|s| s.1).ok_or_else(|| bad(w, "a named set")),
            _ => w.split('|').try_fold(0, |t, n| Ok(t | self.tag(n).ok_or_else(|| bad(n, "a tag"))?)),
        }
    }

    pub fn tag(&self, name: &str) -> Option<Tags> { self.tags.iter().find(|t| t.0 == name).map(|t| 1 << t.1) }

    fn row(&self, w: &[&str]) -> Result<Row, String> {
        let mut row = Row::default();
        let arrow = w.iter().position(|&x| x == "->").ok_or("edge.ept: a rule without ->")?;
        for &p in &w[..arrow] {
            row.ins.push(match p {
                "=None" => Pat::None,
                "=True" | "=False" => Pat::Bool(p == "=True"),
                _ if p.starts_with("=type:") => Pat::Type(self.tags.iter().find(|t| t.0 == p[6..]).ok_or_else(|| bad(p, "a type"))?.1),
                _ if p.starts_with('=') => Pat::Int(int(&p[1..])?),
                _ => Pat::In(self.set(p)?),
            });
        }
        let mut clauses = w[arrow + 1..].split(|&x| x == "|");
        let outs = clauses.next().unwrap_or(&[]);
        row.outs = if outs == ["!"] { None } else { Some(outs.iter().map(|o| self.set(o)).collect::<Result<_, _>>()?) };
        for c in clauses {
            let r = |w: &str| pos(w, 'r');
            match c {
                ["raises", t] => row.raises = self.set(t)?,
                ["raises", t, "unless", x, "in", lr] => { let (lo, hi) = span(lr)?; (row.raises, row.unless) = (self.set(t)?, Some((r(x)?, num(lo)?, num(hi)?))); }
                [x, "=", e] => row.says.push((r(x)?, Self::exact(e)?)),
                [x, "in", lr] => { let (lo, hi) = span(lr)?; row.says.push((r(x)?, Out::Range(num(lo)?, num(hi)?))); }
                [side @ ("true" | "false"), a, t] => row.learn.push((*side == "true", pos(a, 'a')?, Learn::In(self.set(t)?))),
                [side @ ("true" | "false"), a, rel, x] => {
                    let rel = ["<", "<=", ">", ">=", "==", "!="].into_iter().find(|r| r == rel).ok_or_else(|| bad(rel, "a relation"))?;
                    row.learn.push((*side == "true", pos(a, 'a')?, Learn::Rel(rel, rhs(x, 0)?)));
                }
                ["item", x, a] => row.says.push((r(x)?, Out::Item(pos(a, 'a')?))),
                ["items", x, rest @ ..] => row.says.push((r(x)?, Out::Items(rest.iter().map(|a| pos(a, 'a')).collect::<Result<_, _>>()?))),
                ["holds", x, t] => row.says.push((r(x)?, Out::Holds(self.set(t)?, Rhs::N(i64::MIN), Rhs::N(i64::MAX)))),
                ["holds", x, t, "in", lr] => { let (lo, hi) = span(lr)?; row.says.push((r(x)?, Out::Holds(self.set(t)?, lo, hi))); }
                ["puts", a, b] => row.puts.push((pos(a, 'a').ok(), pos(b, 'a').ok())),
                ["calls", a] => row.calls.push(if *a == "*" { usize::MAX } else { pos(a, 'a')? }),
                _ => return Err(bad(&c.join(" "), "a clause of format 1")),
            }
        }
        Ok(row)
    }

    fn exact(e: &str) -> Result<Out, String> {
        let a = |x: &str| pos(x, 'a').map(Rhs::A);
        Ok(match e.find(['+', '-', '*']).filter(|&i| i > 0) {
            Some(i) => Out::Exact(e.as_bytes()[i] as char, a(&e[..i])?, a(&e[i + 1..])?),
            None if e.starts_with("-a") => Out::Exact('-', Rhs::N(0), a(&e[1..])?),
            None if e.starts_with('a') => Out::Exact('=', a(e)?, Rhs::N(0)),
            None => Out::Exact('n', Rhs::N(int(e)?), Rhs::N(0)),
        })
    }

    /* The rows of opcode, method or `Truth` `name` at this operand with their places in the file, an operand's own rows winning over the plain ones. */
    pub fn rows<'r>(&'r self, name: &str, operand: u16) -> Vec<(u32, &'r Row)> {
        let mine = |at: Option<u16>| self.by.get(name).into_iter().flatten().map(|&i| (i, &self.rows[i as usize])).filter(|r| r.1.1 == at).map(|(i, r)| (i, &r.2)).collect::<Vec<_>>();
        let own = mine(Some(operand));
        if own.is_empty() { mine(None) } else { own }
    }

    /* The row at place `i` of the file, the one a value that learns names. */
    pub fn nth(&self, i: u32) -> Option<&Row> { self.rows.get(i as usize).map(|r| &r.2) }
}
