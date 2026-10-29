use alloc::format;
use alloc::string::{String, ToString};
use core::ops::Range;

use edge::code::{Builder, Value};
use edge::lang::Natives;

use crate::flow::{Agg, Format, Key, Program, Report, Sink, Sort, Source};
use crate::kernels::{CSV, JSON, JSON_DOC, JSON_LINES, LINES, TABLE};
use crate::TinyExpr;

type B<'b, 's> = Builder<'b, 's, TinyExpr>;

// a bare name reads the record's field and only a call reaches a native
static FIELDS: Natives = Natives::EMPTY;

fn text(s: &str) -> Value { Value::Str(s.to_string()) }

/* the names of report `i`'s sink and of the rows or records it gathers */
fn names(i: usize) -> (String, String) { (format!("@k{i}"), format!("@g{i}")) }

/* the channel a report writes and how it writes its rows */
fn channel(sink: &Sink) -> (String, i64) {
    match sink {
        Sink::Out(name) => (name.clone(), JSON),
        Sink::Csv(path) => (format!("file:{path}"), TABLE),
        Sink::Jsonl(path) => (format!("file:{path}"), JSON),
    }
}

fn kernel(agg: Agg) -> &'static str {
    match agg { Agg::Count => "@count", Agg::Sum => "@sum", Agg::Average => "@average", Agg::Min => "@min", Agg::Max => "@max" }
}

// the letter the rows kernel reads an aggregate's state by
fn letter(agg: Agg) -> &'static str {
    match agg { Agg::Count => "c", Agg::Sum => "s", Agg::Average => "a", Agg::Min => "n", Agg::Max => "x" }
}

/* builds the program of `prog` through `b` */
pub fn lower(b: &mut B<'_, '_>, prog: &Program) {
    let at = prog.from.span.start;
    match prog.from.source {
        Source::Net { .. } => { b.at(at).import("net", "@net"); }
        Source::Path { .. } => { b.at(at).import("files", "@files"); }
        Source::Input => {}
    }
    for (i, r) in prog.reports.iter().enumerate() {
        let (k, g) = names(i);
        let (name, kind) = channel(&r.sink);
        b.at(r.key.start).load("@sink").constant(text(&name)).constant(Value::Int(kind)).call(2, 0).store(&k);
        // an aggregate keeps a row per key and a sort with none holds the records
        if r.fold.is_some() { b.map(0).store(&g); } else if r.sort.is_some() { b.list(0).store(&g); }
    }
    // one call runs every report over a batch so the source is read once
    b.at(at).function("@each", &["@b"], |b| {
        b.load("@b").for_each(&["@it"], |b| {
            b.subject("@it", &FIELDS);
            match &prog.filter {
                Some(w) => { let _ = b.arms().arm(|b| { b.at(w.span.start).expr(w.expr.clone()); }, |b| reports(b, prog)); }
                None => reports(b, prog),
            }
            b.no_subject();
        });
        b.constant(Value::None).ret();
    });
    source(b, prog);
    // every report ends in one call so a flow builds in linear time
    let ends: alloc::vec::Vec<(usize, &Report)> = prog.reports.iter().enumerate().filter(|(_, r)| r.fold.is_some() || r.sort.is_some()).collect();
    if let Some(&(_, first)) = ends.first() {
        b.at(first.key.start).lambda(&[], |b| {
            for &(i, r) in &ends { finish(b, r, i); }
            b.constant(Value::None);
        }).call(0, 0).pop();
    }
}

/* each report's part of one record the top where kept */
fn reports(b: &mut B<'_, '_>, prog: &Program) {
    for (i, r) in prog.reports.iter().enumerate() {
        match &r.filter {
            Some(w) => { let _ = b.arms().arm(|b| { b.at(w.span.start).expr(w.expr.clone()); }, |b| take(b, r, i)); }
            None => take(b, r, i),
        }
    }
}

/* what report `i` does with a record its where kept */
fn take(b: &mut B<'_, '_>, r: &Report, i: usize) {
    let (k, g) = names(i);
    let Some(f) = &r.fold else {
        let (kernel, into) = if r.sort.is_some() { ("@push", &g) } else { ("@emit", &k) };
        b.at(r.key.start).load(kernel).load(into).load("@it").call(2, 0).pop();
        return;
    };
    b.at(f.span.start).load(kernel(f.agg)).load(&g);
    if let Some(m) = &f.measure { b.expr(m.clone()); }
    for key in &f.keys {
        b.at(f.by.as_ref().map_or(f.span.start, |by| by.start));
        match key {
            Key::Path { parts, .. } => {
                b.load("@it");
                for p in parts { b.constant(text(p)).item(); }
            }
            Key::Named { expr, .. } => { b.expr(expr.clone()); }
        }
    }
    let args = 1 + u16::from(f.measure.is_some()) + f.keys.len() as u16;
    b.at(f.span.start).call(args, 0).pop();
}

/* feeds the source's batches to the reports */
fn source(b: &mut B<'_, '_>, prog: &Program) {
    let from = &prog.from;
    let at = from.span.start;
    let format = Value::Int(match from.format { Format::JsonLines => JSON_LINES, Format::Json => JSON_DOC, Format::Csv => CSV, Format::Lines => LINES });
    let gzip = |inflate: bool| if inflate { text("gzip") } else { Value::None };
    // the module whose reads feed the source or none for host input
    let module = match &from.source {
        Source::Net { url, inflate } => {
            b.at(at).load("@net").field("request").constant(text("GET")).constant(text(url)).constant(Value::None).constant(Value::None)
                .constant(gzip(*inflate)).call(5, 0).store("@id");
            b.at(at).load("@source").load("@id").constant(format).constant(text(url)).call(3, 0).store("@s");
            b.at(at).load("@status").load("@s").load("@net").field("response").load("@id").call(1, 0).call(2, 0).pop();
            Some("@net")
        }
        Source::Path { path, inflate } => {
            b.at(at).load("@files").field("open").constant(text(path)).constant(gzip(*inflate)).call(2, 0).store("@id");
            b.at(at).load("@source").load("@id").constant(format).constant(text(path)).call(3, 0).store("@s");
            Some("@files")
        }
        Source::Input => {
            b.at(at).load("@source").constant(Value::None).constant(format).constant(text("the input")).call(3, 0).store("@s");
            None
        }
    };
    b.at(at).load("@iter").lambda(&[], |b| { b.load("@more").load("@s").call(1, 0); }).constant(Value::Bool(false)).call(2, 0).for_each(&["@t"], |b| {
        b.at(at).load("@each");
        match module {
            Some(module) => { b.load("@feed").load("@s").load(module).field("read").load("@id").call(1, 0).call(2, 0); }
            None => { b.load("@lines").load("@s").call(1, 0); }
        }
        b.call(1, 0).pop();
    });
}

/* report `i`'s rows once the source ends through keep and its order to its sink */
fn finish(b: &mut B<'_, '_>, r: &Report, i: usize) {
    let (k, g) = names(i);
    b.at(r.key.start);
    match &r.fold {
        Some(f) => {
            b.load("@rows").load(&g);
            for key in &f.keys {
                let col = key.column();
                for part in &col { b.constant(text(part)); }
                b.list(col.len() as u16);
            }
            b.list(f.keys.len() as u16).constant(text(letter(f.agg))).constant(text(f.agg.word())).call(4, 0).store("@r");
        }
        None => { b.load(&g).store("@r"); }
    }
    if let Some(keep) = &r.keep {
        b.list(0).store("@kept");
        b.load("@r").for_each(&["@it"], |b| {
            b.at(keep.span.start);
            b.subject("@it", &FIELDS);
            let _ = b.arms().arm(|b| { b.expr(keep.expr.clone()); }, |b| { b.load("@push").load("@kept").load("@it").call(2, 0).pop(); });
            b.no_subject();
        });
        b.load("@kept").store("@r");
    }
    order(b, r);
    b.at(r.key.start).load("@emit_all").load(&k).load("@r").call(2, 0).pop();
}

/* the rows of a report in the order top and sort give them */
fn order(b: &mut B<'_, '_>, r: &Report) {
    let count = |n: u64| Value::Int(i64::try_from(n).unwrap_or(i64::MAX));
    match (&r.sort, &r.top) {
        (None, None) => {}
        (None, Some((span, n))) => {
            let word = r.fold.as_ref().map_or("", |f| f.agg.word());
            b.at(span.start).load("@top").load("@r").constant(count(*n)).constant(text("key")).lambda(&["@it"], |b| {
                b.at(span.start).load("@it").constant(text(word)).item();
            }).call(2, 1).store("@r");
        }
        (Some(Sort { span, keys }), top) => {
            // one descending key then top keeps the greatest rows without sorting all of them
            if let (Some((_, n)), [(e, true)]) = (top, keys.as_slice()) {
                b.at(span.start).load("@top").load("@r").constant(count(*n)).constant(text("key")).lambda(&["@it"], |b| key(b, span.start, e)).call(2, 1).store("@r");
                return;
            }
            for (e, desc) in keys.iter().rev() {
                b.at(span.start).load("@sorted").load("@r").constant(text("key")).lambda(&["@it"], |b| key(b, span.start, e))
                    .constant(text("reverse")).constant(Value::Bool(*desc)).call(1, 2).store("@r");
            }
            if let Some((at, n)) = top { b.at(at.start).load("@first").load("@r").constant(count(*n)).call(2, 0).store("@r"); }
        }
    }
}

/* the sort key of the row a lambda is handed */
fn key(b: &mut B<'_, '_>, at: usize, e: &Range<usize>) {
    b.at(at);
    b.subject("@it", &FIELDS);
    b.expr(e.clone());
    b.no_subject();
}
