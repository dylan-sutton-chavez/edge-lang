use crate::s;

use super::{Body, Parser, Unit};
use crate::code::OpCode;
use crate::syntax::{chunk::{ImportEntry, ImportKind, NativeClassEntry, SSAChunk}, ssa::ssa_strip, text::parse_string};
use crate::syntax::lexer::{Spelling, Token, TokenType};
use crate::deps::Resolved;
use crate::util::hash::FxHashSet;

use alloc::{string::{String, ToString}, vec::Vec};

// modules import modules this deep so a chain of them never exhausts the stack
const MAX_IMPORT_DEPTH: usize = 64;

/* imports lower at compile time with natives bound to `extern_table` and code inlined */
impl<'src, I: Iterator<Item = Token>, S: Spelling> Parser<'src, I, S> {

    /* `import` binds the module at compile time so no import opcode reaches the VM */
    pub(super) fn import_stmt(&mut self) {
        self.advance();
        loop {
            let Some((spec, alias_hint, span)) = self.read_module_spec() else { return; };
            let alias = match self.eat_if(TokenType::As) {
                true => match self.name_text() { Some(alias) => alias.to_string(), None => return },
                false => alias_hint,
            };
            self.resolve_and_bind_all(&spec, span, &alias);
            if !self.eat_if(TokenType::Comma) { break; }
        }
    }

    /* `from spec import` with names in optional parentheses and a trailing comma allowed */
    pub(super) fn parse_from_stmt(&mut self) {
        self.advance();
        let Some((spec, _, spec_span)) = self.read_module_spec() else { return; };
        self.eat(TokenType::Import);

        if self.eat_if(TokenType::Star) {
            self.resolve_and_bind_star(&spec, spec_span);
            return;
        }

        let parens = self.eat_if(TokenType::ParenOpen);

        let mut names: Vec<(String, String)> = Vec::new();
        loop {
            // peeking skips newlines and comments inside parentheses so a trailing comma ends the list
            if parens && matches!(self.peek(), Some(TokenType::ParenClose)) { break; }
            let Some(name) = self.name_text() else { return };
            let alias = if self.eat_if(TokenType::As) { let Some(alias) = self.name_text() else { return }; alias } else { name };
            names.push((name.to_string(), alias.to_string()));
            if !self.eat_if(TokenType::Comma) { break; }
        }

        if parens { self.eat(TokenType::ParenClose); }

        self.resolve_and_bind_named(&spec, spec_span, names);
    }

    /* the name at the cursor or none once a diagnostic says a name belongs there */
    fn name_text(&mut self) -> Option<&'src str> {
        if matches!(self.peek_same_line(), Some(TokenType::Name | TokenType::Underscore)) { return Some(self.advance_text()); }
        self.error("expected a name");
        None
    }

    /* the import spec at the cursor read as the walk reads it with its span */
    fn read_module_spec(&mut self) -> Option<(String, String, (usize, usize))> {
        use crate::deps::{module_spec, ImportSpec, NoSpec};
        let at = self.tokens.pos;
        match module_spec::<S>(self.source, &self.tokens.toks, at) {
            Ok((spec, first, next)) => {
                let alias = self.lexeme(&self.tokens.toks[first]).to_string();
                let span = (self.tokens.toks[at].start, self.tokens.toks[next - 1].end);
                while self.tokens.pos < next { self.advance(); }
                let (ImportSpec::Bare(spec) | ImportSpec::Relative(spec) | ImportSpec::Root(spec)) = spec;
                Some((spec, alias, span))
            }
            Err((why, k)) => {
                let n = self.source.len();
                let t = self.tokens.toks.get(k).copied().unwrap_or(Token { kind: TokenType::Eof, line: self.last_line, start: n, end: n, comp: false, assigns: 0 });
                let msg = match why {
                    NoSpec::Quoted => s!("module '", str &parse_string::<S>(self.lexeme(&t)), "' not found"),
                    // `from . import x` names no module since Edge has no packages
                    NoSpec::Dots | NoSpec::Segment => s!(str Self::MODULE_AFTER_DOT),
                    NoSpec::Name => s!("expected a module name"),
                };
                self.error_at(t.start, t.end, &msg);
                None
            }
        }
    }

    /* the module at canonical `spec` parsed once whatever its shape and however many import it */
    fn parse_or_get_cached(&mut self, spec: &str, src: &str, span: (usize, usize)) -> Option<alloc::rc::Rc<SSAChunk>> {
        // the first import makes the cache every module under this source shares
        let cache = self.module_cache.get_or_insert_with(Default::default).clone();
        // the modules still parsing are the chain of imports this one sits under
        let (mut held, mut parsing) = (None, 0);
        for (k, c) in cache.borrow().iter() {
            if k == spec { held = Some(c.clone()); }
            parsing += usize::from(c.is_none());
        }
        match held {
            Some(Some(cached)) => return Some(cached),
            // a module still parsing further up imports itself again
            Some(None) => {
                self.error_at(span.0, span.1, &s!("circular import: '", str spec, "'"));
                return None;
            }
            None if parsing >= MAX_IMPORT_DEPTH => {
                self.error_at(span.0, span.1, &s!("imports nest deeper than ", int MAX_IMPORT_DEPTH, " modules"));
                return None;
            }
            None => {}
        }
        cache.borrow_mut().push((spec.to_string(), None));
        // tracebacks show the module file instead of `<module>`
        let path = Some(alloc::sync::Arc::new(spec.to_string()));
        let unit = Unit { resolver: self.resolver.child(spec), path, body: Body::Program, cache: Some(cache.clone()), modules: self.modules };
        // a module parses through the front end of the source importing it
        let parsed = (self.modules)(src, unit);
        {
            // it leaves the parsing set and stays for the next importer once it parsed
            let mut held = cache.borrow_mut();
            if let Some(i) = held.iter().position(|(k, c)| k == spec && c.is_none()) {
                match &parsed {
                    Ok(sub) => held[i].1 = Some(sub.clone()),
                    Err(_) => { held.remove(i); }
                }
            }
        }
        match parsed {
            Ok(sub) => Some(sub),
            Err(errs) => {
                self.error_at(span.0, span.1,
                    &s!("module '", str spec, "' parse error: ", str errs.first().map_or("", |e| e.msg.as_str())));
                None
            }
        }
    }

    /* the `LoadModule` operand of an import registered once per spec */
    fn register_import(&mut self, spec: &str, kind: ImportKind) -> u16 {
        if let Some(i) = self.chunk.imports.iter().position(|e| e.spec == spec) {
            return i as u16;
        }
        let i = self.chunk.imports.len() as u16;
        self.chunk.imports.push(ImportEntry {
            spec: spec.to_string(),
            kind,
        });
        i
    }

    /* top-level names a module stores or defines and only public ones when `public` */
    fn module_exports(sub: &SSAChunk, public: bool) -> Vec<String> {
        let mut exports: Vec<String> = Vec::new();
        let mut seen: FxHashSet<String> = FxHashSet::default();
        for ins in &sub.instructions {
            let slot_idx = match ins.opcode {
                OpCode::StoreName => Some(ins.operand as usize),
                OpCode::MakeFunction | OpCode::MakeCoroutine => sub.functions
                    .get(ins.operand as usize)
                    .map(|f| f.3 as usize),
                _ => None,
            };
            let Some(s) = slot_idx else { continue };
            let Some(name) = sub.names.get(s) else { continue };
            let bare = ssa_strip(name).to_string();
            if public && bare.starts_with('_') { continue; }
            if seen.insert(bare.clone()) { exports.push(bare); }
        }
        exports
    }

    fn native_import_kind(bindings: &[crate::deps::NativeBinding], classes: &[crate::deps::NativeClass], consts: &[crate::deps::NativeBinding]) -> ImportKind {
        ImportKind::Native {
            funcs: bindings.to_vec(),
            classes: classes.iter().map(|c| NativeClassEntry {
                name: c.name.clone(),
                methods: c.methods.clone(),
            }).collect(),
            consts: consts.to_vec(),
        }
    }

    /* resolves `spec` through the host with a failure reported at `span` */
    fn resolve_at(&mut self, spec: &str, span: (usize, usize)) -> Option<Resolved> {
        self.resolver.resolve(spec).map_err(|msg| self.error_at(span.0, span.1, &msg)).ok()
    }

    /* binds native `b` under `alias` for `CallExtern` and false once the 256 slots fill */
    fn push_extern(&mut self, b: &crate::deps::NativeBinding, alias: &str, span: (usize, usize)) -> bool {
        let idx = self.chunk.extern_table.len() as u16;
        if idx > 0xFF { self.error_at(span.0, span.1, "too many native imports (max 256 per module)"); return false; }
        self.chunk.extern_table.push(b.clone());
        self.chunk.extern_index.insert(alias.to_string(), idx);
        true
    }

    fn bind_module_attr(&mut self, import_idx: u16, name: &str, alias: &str) {
        self.chunk.emit(OpCode::LoadModule, import_idx);
        let attr_idx = self.chunk.push_name(name);
        self.chunk.emit(OpCode::LoadAttr, attr_idx);
        self.emit_store_new(alias);
    }

    /* a named import binds module attributes and native functions also fill `extern_table` */
    fn resolve_and_bind_named(&mut self, spec: &str, span: (usize, usize), names: Vec<(String, String)>) {
        let Some(resolved) = self.resolve_at(spec, span) else { return; };
        let (Resolved::Code { canonical, .. } | Resolved::Native { canonical, .. }) = &resolved;
        let url = canonical.clone();
        match resolved {
            Resolved::Native { bindings, classes, consts, .. } => {
                let import_idx = self.register_import(&url, Self::native_import_kind(&bindings, &classes, &consts));
                // functions take the extern fast path yet still bind as attributes that export as values
                for (name, alias) in &names {
                    if classes.iter().any(|c| c.name == *name) || consts.iter().any(|c| c.name == *name) {
                        self.bind_module_attr(import_idx, name, alias);
                        continue;
                    }
                    let Some(b) = bindings.iter().find(|b| b.name == *name) else {
                        self.error_at(span.0, span.1,
                            &s!("module '", str &url, "' has no export '", str name, "'"));
                        continue;
                    };
                    if !self.push_extern(b, alias, span) { continue; }
                    self.bind_module_attr(import_idx, name, alias);
                }
            }
            Resolved::Code { src, canonical } => {
                let Some(sub) = self.parse_or_get_cached(&canonical, &src, span) else { return; };
                let exports = Self::module_exports(&sub, false);
                let import_idx = self.register_import(&canonical, ImportKind::Code(sub));
                for (name, alias) in &names {
                    if !exports.iter().any(|e| e == name) {
                        self.error_at(span.0, span.1,
                            &s!("module '", str &canonical, "' has no export '", str name, "'"));
                        continue;
                    }
                    self.bind_module_attr(import_idx, name, alias);
                }
            }
        }
    }

    /* `import X` binds the module value the VM builds once at init */
    pub(super) fn resolve_and_bind_all(&mut self, spec: &str, span: (usize, usize), alias: &str) {
        let Some(resolved) = self.resolve_at(spec, span) else { return; };
        let import_idx = match resolved {
            Resolved::Native { bindings, classes, consts, canonical } => {
                self.register_import(&canonical, Self::native_import_kind(&bindings, &classes, &consts))
            }
            Resolved::Code { src, canonical } => {
                let Some(sub) = self.parse_or_get_cached(&canonical, &src, span) else { return; };
                self.register_import(&canonical, ImportKind::Code(sub))
            }
        };
        self.chunk.emit(OpCode::LoadModule, import_idx);
        self.emit_store_new(alias);
    }

    /* a star import fills `extern_index` for natives and binds every export of the module */
    fn resolve_and_bind_star(&mut self, spec: &str, span: (usize, usize)) {
        let Some(resolved) = self.resolve_at(spec, span) else { return; };
        match resolved {
            Resolved::Native { bindings, classes, consts, canonical } => {
                for b in &bindings {
                    if !self.push_extern(b, &b.name, span) { break; }
                }
                let import_idx = self.register_import(&canonical, Self::native_import_kind(&bindings, &classes, &consts));
                for name in bindings.iter().map(|b| &b.name)
                    .chain(classes.iter().map(|c| &c.name))
                    .chain(consts.iter().map(|c| &c.name)) {
                    self.bind_module_attr(import_idx, name, name);
                }
            }
            Resolved::Code { src, canonical } => {
                let Some(sub) = self.parse_or_get_cached(&canonical, &src, span) else { return; };
                let exports = Self::module_exports(&sub, true);
                let import_idx = self.register_import(&canonical, ImportKind::Code(sub));
                for name in &exports {
                    self.bind_module_attr(import_idx, name, name);
                }
            }
        }
    }

}
