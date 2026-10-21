/* Reads `edge.ept`, the rules per opcode at one Edge commit, refusing a file of another commit or version or with a line it does not know. */

use alloc::{string::{String, ToString}, vec::Vec};
use crate::parser::OpCode;
use super::Tags;

// The commit every rule is about, the one this build of Edge is.
pub const COMMIT: &str = "8855578768056f9e68dab3f2a3361e158ed7d9e2";

/* A count of the stack line, the sum of its terms. */
#[derive(Clone, Debug)]
pub enum Term { N(u32), Bits(u32, u16, u16), Defaults, ClassArgs }

/* What one popped value must be for its row to apply. */
#[derive(Clone, Debug)]
pub enum Pat { In(Tags), Type(u8), Int(i64), None, Bool(bool) }

/* The right side of what a comparison learns, an operand or an integer. */
#[derive(Clone, Copy, Debug)]
pub enum Rhs { A(usize), N(i64) }

#[derive(Clone, Debug)]
pub enum Learn { In(Tags), Rel(&'static str, Rhs) }

/* The exact integer of a result, from its operands. */
#[derive(Clone, Copy, Debug)]
pub enum Exact { A(usize), Neg(usize), Add(usize, usize), Sub(usize, usize), Mul(usize, usize), N(i64) }

#[derive(Clone, Debug, Default)]
pub struct Row {
    pub ins: Vec<Pat>,
    // None for `-> !`, the opcode always raising.
    pub outs: Option<Vec<Tags>>,
    pub raises: Tags,
    // Raises only when the exact result `r` leaves `lo..=hi`.
    pub unless: Option<(usize, i64, i64)>,
    pub exact: Vec<(usize, Exact)>,
    pub ranges: Vec<(usize, i64, i64)>,
    // On the truthy (true) or falsy side of `r0`, what operand `a` is.
    pub learn: Vec<(bool, usize, Learn)>,
    pub item: Vec<(usize, usize)>,
    pub items: Vec<(usize, Vec<usize>)>,
    // Container `a` takes operand `b` among its items, None for a container or items not known.
    pub puts: Vec<(Option<usize>, Option<usize>)>,
}

#[derive(Default)]
pub struct Rules {
    pub tags: Vec<(String, u8, u8)>,
    // By opcode byte, the plain stack line and those of single operands.
    stack: Vec<Option<(Vec<Term>, Vec<Term>)>>,
    stack_at: Vec<(u8, u16, (Vec<Term>, Vec<Term>))>,
    rows: Vec<Vec<(Option<u16>, Row)>>,
    // By `type.name` of ALL_METHODS.
    methods: Vec<(String, Row)>,
    // What testing a value for truth learns, `rule Truth`.
    pub truth: Vec<Row>,
}

fn op_of(name: &str) -> Option<OpCode> {
    OpCode::ALL.iter().copied().find(|o| alloc::format!("{o:?}") == name)
}

fn int(w: &str) -> Result<i64, String> { w.parse().map_err(|_| s(w, "an integer")) }

fn s(w: &str, what: &str) -> String { alloc::format!("edge.ept: '{w}' is not {what}") }

fn terms(w: &str) -> Result<Vec<Term>, String> {
    w.split('+').map(|t| {
        let (k, rest) = match t.split_once('*') { Some((k, r)) => (int(k)? as u32, r), None => (1, t) };
        Ok(match rest {
            "defaults" => Term::Defaults,
            "classargs" => Term::ClassArgs,
            r if r.starts_with("o[") => {
                let (lo, hi) = r[2..].trim_end_matches(']').split_once(':').ok_or_else(|| s(r, "a bit field"))?;
                Term::Bits(k, int(lo)? as u16, int(hi)? as u16)
            }
            r => Term::N(int(r)? as u32 * k),
        })
    }).collect()
}

fn pos(w: &str, side: char) -> Result<usize, String> {
    w.strip_prefix(side).and_then(|n| n.parse().ok()).ok_or_else(|| s(w, "a position"))
}

fn range(w: &str) -> Result<(i64, i64), String> {
    let (lo, hi) = w.split_once("..").ok_or_else(|| s(w, "a range"))?;
    Ok((if lo.is_empty() { i64::MIN } else { int(lo)? }, if hi.is_empty() { i64::MAX } else { int(hi)? }))
}

impl Rules {
    pub fn read(text: &str) -> Result<Self, String> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
        if lines.next() != Some(COMMIT) { return Err("edge.ept is of another Edge commit".into()); }
        if lines.next() != Some("ept 1") { return Err("edge.ept is not format 1".into()); }
        let mut r = Rules { stack: alloc::vec![None; OpCode::ALL.len()], rows: alloc::vec![Vec::new(); OpCode::ALL.len()], ..Default::default() };
        let rest: Vec<&str> = lines.collect();
        // Tags first, as the rows read them by name.
        for l in &rest {
            if let ["tag", n, lo, hi] = l.split(' ').collect::<Vec<_>>()[..] { r.tags.push((n.to_string(), int(lo)? as u8, int(hi)? as u8)); }
        }
        for l in rest {
            let w: Vec<&str> = l.split(' ').collect();
            match w[0] {
                "tag" => {}
                "stack" if w.len() == 4 => {
                    let (name, at) = match w[1].split_once('@') { Some((n, a)) => (n, Some(int(a)? as u16)), None => (w[1], None) };
                    let op = op_of(name).ok_or_else(|| s(name, "an opcode"))?;
                    let e = (terms(w[2])?, terms(w[3])?);
                    match at { Some(a) => r.stack_at.push((op as u8, a, e)), None => r.stack[op as usize] = Some(e) }
                }
                "rule" if w[1] == "Truth" => { let row = r.row(&w[2..])?; r.truth.push(row); }
                "rule" => {
                    let (name, at) = match w[1].split_once('@') { Some((n, a)) => (n, Some(int(a)? as u16)), None => (w[1], None) };
                    let op = op_of(name).ok_or_else(|| s(name, "an opcode"))?;
                    let row = r.row(&w[2..])?;
                    r.rows[op as usize].push((at, row));
                }
                "method" if w.len() > 2 => { let row = r.row(&w[2..])?; r.methods.push((w[1].to_string(), row)); }
                _ => return Err(s(l, "a line of format 1")),
            }
        }
        Ok(r)
    }

    pub fn set(&self, w: &str) -> Result<Tags, String> {
        match w {
            "*" => Ok(Tags::ALL),
            "!" => Ok(Tags::NONE),
            _ => w.split('|').try_fold(Tags::NONE, |t, n| Ok(t | self.tag(n).ok_or_else(|| s(n, "a tag"))?)),
        }
    }

    // The codes of tag `name`.
    pub fn tag(&self, name: &str) -> Option<Tags> {
        self.tags.iter().find(|t| t.0 == name).map(|&(_, lo, hi)| Tags::range(lo, hi))
    }

    fn row(&self, w: &[&str]) -> Result<Row, String> {
        let mut row = Row::default();
        let arrow = w.iter().position(|&x| x == "->").ok_or("edge.ept: a rule without ->")?;
        for &p in &w[..arrow] {
            row.ins.push(match p {
                "=None" => Pat::None,
                "=True" => Pat::Bool(true),
                "=False" => Pat::Bool(false),
                _ if p.starts_with("=type:") => Pat::Type(self.tags.iter().find(|t| t.0 == p[6..]).ok_or_else(|| s(p, "a type"))?.1),
                _ if p.starts_with('=') => Pat::Int(int(&p[1..])?),
                _ => Pat::In(self.set(p)?),
            });
        }
        let mut clauses = w[arrow + 1..].split(|&x| x == "|");
        let outs = clauses.next().unwrap_or(&[]);
        row.outs = if outs == ["!"] { None } else { Some(outs.iter().map(|o| self.set(o)).collect::<Result<_, _>>()?) };
        for c in clauses {
            match c {
                ["raises", t] => row.raises = self.set(t)?,
                ["raises", t, "unless", r, "in", lr] => { row.raises = self.set(t)?; let (lo, hi) = range(lr)?; row.unless = Some((pos(r, 'r')?, lo, hi)); }
                [r, "=", e] => row.exact.push((pos(r, 'r')?, Self::exact(e)?)),
                [r, "in", lr] => { let (lo, hi) = range(lr)?; row.ranges.push((pos(r, 'r')?, lo, hi)); }
                [side @ ("true" | "false"), a, t] => row.learn.push((*side == "true", pos(a, 'a')?, Learn::In(self.set(t)?))),
                [side @ ("true" | "false"), a, rel, x] => {
                    let rel = ["<", "<=", ">", ">=", "==", "!="].into_iter().find(|r| r == rel).ok_or_else(|| s(rel, "a relation"))?;
                    let x = if x.starts_with('a') { Rhs::A(pos(x, 'a')?) } else { Rhs::N(int(x)?) };
                    row.learn.push((*side == "true", pos(a, 'a')?, Learn::Rel(rel, x)));
                }
                ["item", r, a] => row.item.push((pos(r, 'r')?, pos(a, 'a')?)),
                ["items", r, rest @ ..] => row.items.push((pos(r, 'r')?, rest.iter().map(|a| pos(a, 'a')).collect::<Result<_, _>>()?)),
                ["puts", a, b] => row.puts.push((pos(a, 'a').ok(), pos(b, 'a').ok())),
                _ => return Err(s(&c.join(" "), "a clause of format 1")),
            }
        }
        Ok(row)
    }

    fn exact(e: &str) -> Result<Exact, String> {
        let a = |x: &str| pos(x, 'a');
        Ok(if let Some(x) = e.strip_prefix('-').filter(|x| x.starts_with('a')) { Exact::Neg(a(x)?) }
        else if let Some((x, y)) = e.split_once('+') { Exact::Add(a(x)?, a(y)?) }
        else if let Some((x, y)) = e.split_once('-').filter(|(x, _)| !x.is_empty()) { Exact::Sub(a(x)?, a(y)?) }
        else if let Some((x, y)) = e.split_once('*') { Exact::Mul(a(x)?, a(y)?) }
        else if e.starts_with('a') { Exact::A(a(e)?) }
        else { Exact::N(int(e)?) })
    }

    /* The values `op` pops and pushes at this operand, None for an opcode the file does not cover. */
    pub fn effect(&self, op: OpCode, operand: u16, chunk: &crate::parser::SSAChunk) -> Option<(usize, usize)> {
        let e = self.stack_at.iter().find(|s| s.0 == op as u8 && s.1 == operand).map(|s| &s.2).or(self.stack[op as usize].as_ref())?;
        let sum = |ts: &[Term]| ts.iter().map(|t| match *t {
            Term::N(n) => Some(n as usize),
            Term::Bits(k, lo, hi) => Some(k as usize * ((operand as u32 >> lo) & ((1u32 << (hi - lo)) - 1)) as usize),
            Term::Defaults => chunk.functions.get(operand as usize).map(|f| f.2 as usize),
            Term::ClassArgs => chunk.classes.get(operand as usize).map(|c| if c.bases == crate::parser::SPREAD_BASES { 1 } else { c.bases as usize } + 2 * c.class_kw as usize),
        }).sum::<Option<usize>>();
        Some((sum(&e.0)?, sum(&e.1)?))
    }

    /* The rows of builtin method `ty.name`, none for a method not covered. */
    pub fn method<'r>(&'r self, ty: &'r str, name: &'r str) -> impl Iterator<Item = &'r Row> {
        self.methods.iter().filter(move |m| m.0.split_once('.') == Some((ty, name))).map(|m| &m.1)
    }

    /* The rows of `op` at this operand, an operand's own rows winning over the plain ones. */
    pub fn rows(&self, op: OpCode, operand: u16) -> impl Iterator<Item = &Row> {
        let own = self.rows[op as usize].iter().any(|r| r.0 == Some(operand));
        self.rows[op as usize].iter().filter(move |r| r.0 == if own { Some(operand) } else { None }).map(|r| &r.1)
    }
}
