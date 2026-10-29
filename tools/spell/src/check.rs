use edge::code::{Parser, SSAChunk};
use edge::lexer::{lex_as, Spelling};
use edge::deps::NoopResolver;

// the chunk a source compiles to unfolded and every message its lexer and parser report
pub fn compiled<S: Spelling>(src: &str) -> (SSAChunk, Vec<String>) {
    let (tokens, lex_errs) = lex_as::<S>(src);
    let (chunk, errs) = Parser::<_, S>::spelled(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
    (chunk, lex_errs.iter().map(|e| e.msg.to_string()).chain(errs.into_iter().map(|d| d.msg)).collect())
}

// where two chunks part with each name of `a` renamed as `b` spells it
pub fn differ(a: &SSAChunk, b: &SSAChunk, at: &str, rename: &dyn Fn(&str) -> String) -> Option<String> {
    let ins = |c: &SSAChunk| c.instructions.iter().map(|i| (i.opcode, i.operand)).collect::<Vec<_>>();
    let names: Vec<String> = a.names.iter().map(|n| rename(n)).collect();
    if names != b.names { return Some(format!("{at} names {names:?} against {:?}", b.names)); }
    if a.constants != b.constants { return Some(format!("{at} constants {:?} against {:?}", a.constants, b.constants)); }
    if ins(a) != ins(b) { return Some(format!("{at} instructions differ")); }
    if (a.functions.len(), a.classes.len(), a.bases, a.is_generator) != (b.functions.len(), b.classes.len(), b.bases, b.is_generator) {
        return Some(format!("{at} holds other bodies"));
    }
    for (i, (f, g)) in a.functions.iter().zip(&b.functions).enumerate() {
        let params: Vec<String> = f.0.iter().map(|n| rename(n)).collect();
        if (&params, f.2, f.3) != (&g.0, g.2, g.3) { return Some(format!("{at} function {i} has another signature")); }
        if let Some(d) = differ(&f.1, &g.1, &format!("{at} function {i}"), rename) { return Some(d); }
    }
    a.classes.iter().zip(&b.classes).enumerate().find_map(|(i, (c, d))| differ(c, d, &format!("{at} class {i}"), rename))
}

// a translation fails on another program or on errors the source does not have
pub fn same_program(a: &(SSAChunk, Vec<String>), b: &(SSAChunk, Vec<String>)) -> Result<(), String> {
    same_renamed(a, b, &|n| n.to_string())
}

// the same where `rename` gives each name of `a` the name it takes in `b`
pub fn same_renamed((a, ea): &(SSAChunk, Vec<String>), (b, eb): &(SSAChunk, Vec<String>), rename: &dyn Fn(&str) -> String) -> Result<(), String> {
    if ea.is_empty() != eb.is_empty() { return Err(format!("errors {ea:?} against {eb:?}")); }
    differ(a, b, "module", rename).map_or(Ok(()), Err)
}

// the first statement whose line moved so an error points elsewhere in the other spelling
pub fn moved_line(a: &SSAChunk, src_a: &str, b: &SSAChunk, src_b: &str) -> Option<(usize, usize)> {
    let line = |src: &str, at: u32| src[..(at as usize).min(src.len())].matches('\n').count() + 1;
    let lines = |c: &SSAChunk, src: &str| {
        let mut out = vec![(c.first_line as usize, 0)];
        out.extend(c.stmt_pos.iter().map(|&(ip, at)| (line(src, at), ip as usize)));
        out
    };
    let (la, lb) = (lines(a, src_a), lines(b, src_b));
    if let Some(((x, _), (y, _))) = la.iter().zip(&lb).find(|(x, y)| x != y) { return Some((*x, *y)); }
    a.functions.iter().zip(&b.functions).find_map(|(f, g)| moved_line(&f.1, src_a, &g.1, src_b))
        .or_else(|| a.classes.iter().zip(&b.classes).find_map(|(c, d)| moved_line(c, src_a, d, src_b)))
}
