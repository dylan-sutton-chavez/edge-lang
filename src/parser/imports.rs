use crate::s;

use super::Parser;
use super::types::{Diagnostic, ImportEntry, ImportKind, NativeClassEntry, OpCode, SSAChunk, parse_string, ssa_strip};
use crate::lexer::{Token, TokenType, lex};
use crate::modules::Resolved;
use crate::util::hash::FxHashSet;

use alloc::{string::{String, ToString}, vec::Vec};

/* Compile-time import lowering where Native binds to extern_table (CallExtern) and Code inlines MakeFunction+StoreName. */
impl<'src, I: Iterator<Item = Token>> Parser<'src, I> {

    /* `import name [as alias]` resolves and binds module as HeapObj::Module under alias. Compile-time only, no import opcodes reach the VM. */
    pub(super) fn import_stmt(&mut self) {
        self.advance(); // 'import'
        loop {
            let Some((spec, alias_hint, span)) = self.read_module_spec() else { return; };
            let alias = if self.eat_if(TokenType::As) {
                self.advance_text().to_string()
            } else {
                alias_hint
            };
            self.resolve_and_bind_all(&spec, span, &alias);
            if !self.eat_if(TokenType::Comma) { break; }
        }
    }

    /* `from <spec> import names|*`, spec is a bare name or dotted path, `*` binds all exports. Names may be parenthesized for multi-line lists, trailing comma allowed. */
    pub(super) fn parse_from_stmt(&mut self) {
        self.advance(); // 'from'
        let Some((spec, _, spec_span)) = self.read_module_spec() else { return; };
        self.eat(TokenType::Import);

        if self.eat_if(TokenType::Star) {
            self.resolve_and_bind_star(&spec, spec_span);
            return;
        }

        let parens = self.eat_if(TokenType::Lpar);

        let mut names: Vec<(String, String)> = Vec::new();
        loop {
            // Peek flushes Nl/Comment inside parens and lets a trailing `,` end the list.
            if parens && matches!(self.peek(), Some(TokenType::Rpar)) { break; }
            let name = self.advance_text();
            let alias = if self.eat_if(TokenType::As) { self.advance_text() } else { name };
            names.push((name.to_string(), alias.to_string()));
            if !self.eat_if(TokenType::Comma) { break; }
        }

        if parens { self.eat(TokenType::Rpar); }

        self.resolve_and_bind_named(&spec, spec_span, names);
    }

    /* Reads a module spec, leading dots importer-relative, dotted names root-relative, else bare, returning spec, alias, span. */
    fn read_module_spec(&mut self) -> Option<(String, String, (usize, usize))> {
        let first = self.advance();
        let first_start = first.start;
        if first.kind == TokenType::String {
            let spec = parse_string(self.lexeme(&first));
            self.error_at(first.start, first.end, &s!("module '", str &spec, "' not found"));
            return None;
        }
        let ups = match first.kind {
            TokenType::Dot => 1 + self.eat_dots(),
            TokenType::Ellipsis => 3 + self.eat_dots(),
            _ => 0,
        };
        let first_name = if ups > 0 {
            // `from . import x` names no module, packages do that and Edge has none.
            if !matches!(self.peek(), Some(TokenType::Name)) {
                self.error_at(first.start, first.end, "expected a module name after '.'");
                return None;
            }
            self.advance()
        } else if matches!(first.kind, TokenType::Name | TokenType::Underscore | TokenType::Match | TokenType::Case | TokenType::Type) {
            first
        } else {
            self.error_at(first.start, first.end, "expected a module name");
            return None;
        };
        let (name, end) = self.dotted_name(first_name);
        let alias = name.split('.').next().unwrap_or(&name).to_string();
        let path = name.replace('.', "/");
        let spec = match ups {
            0 if name.contains('.') => s!(str &path, ".py"),
            0 => name,
            1 => s!("./", str &path, ".py"),
            _ => s!(str &"../".repeat(ups - 1), str &path, ".py"),
        };
        Some((spec, alias, (first_start, end)))
    }

    /* Consumes `name(.name)*` given the first name token, returns (dotted name, end position). */
    fn dotted_name(&mut self, first: Token) -> (String, usize) {
        let mut name = self.lexeme(&first).to_string();
        let mut end = first.end;
        while self.eat_if(TokenType::Dot) {
            let next = self.advance();
            name.push('.');
            name.push_str(self.lexeme(&next));
            end = next.end;
        }
        (name, end)
    }

    fn eat_dots(&mut self) -> usize {
        let mut n = 0;
        while self.eat_if(TokenType::Dot) { n += 1; }
        n
    }

    /* Parses or returns cached SSAChunk. Only path/URL specs cached, bare names skipped to avoid cross-manifest collisions. */
    fn parse_or_get_cached(&mut self, spec: &str, src: &str, span: (usize, usize)) -> Option<alloc::rc::Rc<SSAChunk>> {
        let cache_safe = spec.contains('/') || spec.contains("://");
        if cache_safe
            && let Some(cached) = self.module_cache.borrow().iter().find(|(k, _)| k == spec).map(|(_, c)| c.clone())
        {
            return Some(cached);
        }
        let owned = src.to_string();
        let (tokens, lex_errs) = lex(&owned);
        let mut sub_parser = Parser::with_shared_cache(
            &owned, tokens.into_iter(),
            self.resolver.child(spec),
            self.module_cache.clone(),
        );
        // Set path so tracebacks show the module file, not '<module>'.
        sub_parser.chunk.path = alloc::sync::Arc::new(spec.to_string());
        for e in lex_errs {
            sub_parser.errors.push(Diagnostic {
                start: e.start, end: e.end, msg: e.msg.to_string(),
            });
        }
        let (sub, errs) = sub_parser.parse();
        if !errs.is_empty() {
            self.error_at(span.0, span.1,
                &s!("module '", str spec, "' parse error: ", str &errs[0].msg));
            return None;
        }
        let rc = alloc::rc::Rc::new(sub);
        if cache_safe {
            self.module_cache.borrow_mut().push((spec.to_string(), rc.clone()));
        }
        Some(rc)
    }

    /* Registers import deduped by spec, returns LoadModule operand index. */
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

    /* Collects top-level names from StoreName/MakeFunction, import-star keeps only the public ones. */
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

    /* Build a Native ImportKind from resolved bindings/classes/consts. */
    fn native_import_kind(bindings: &[crate::modules::NativeBinding], classes: &[crate::modules::NativeClass], consts: &[crate::modules::NativeBinding]) -> ImportKind {
        ImportKind::Native {
            funcs: bindings.to_vec(),
            classes: classes.iter().map(|c| NativeClassEntry {
                name: c.name.clone(),
                methods: c.methods.clone(),
            }).collect(),
            consts: consts.to_vec(),
        }
    }

    /* The host resolution of `spec`, a failure reported at `span`. */
    fn resolve_at(&mut self, spec: &str, span: (usize, usize)) -> Option<Resolved> {
        self.resolver.resolve(spec).map_err(|msg| self.error_at(span.0, span.1, &msg)).ok()
    }

    /* Binds native `b` for direct CallExtern under `alias`, false once the 256 slot table is full. */
    fn push_extern(&mut self, b: &crate::modules::NativeBinding, alias: &str, span: (usize, usize)) -> bool {
        let idx = self.chunk.extern_table.len() as u16;
        if idx > 0xFF { self.error_at(span.0, span.1, "too many native imports (max 256 per module)"); return false; }
        self.chunk.extern_table.push(b.clone());
        self.chunk.extern_index.insert(alias.to_string(), idx);
        true
    }

    /* Emit `LoadModule + LoadAttr(name) + StoreName(alias)` to bind a module attribute under `alias`. */
    fn bind_module_attr(&mut self, import_idx: u16, name: &str, alias: &str) {
        self.chunk.emit(OpCode::LoadModule, import_idx);
        let attr_idx = self.chunk.push_name(name);
        self.chunk.emit(OpCode::LoadAttr, attr_idx);
        self.emit_store_new(alias);
    }

    /* Named import registers module, emits LoadModule+LoadAttr+StoreName, Native also populates extern_table for functions. */
    fn resolve_and_bind_named(&mut self, spec: &str, span: (usize, usize), names: Vec<(String, String)>) {
        let Some(resolved) = self.resolve_at(spec, span) else { return; };
        let (Resolved::Code { canonical, .. } | Resolved::Native { canonical, .. }) = &resolved;
        let url = canonical.clone();
        match resolved {
            Resolved::Native { bindings, classes, consts, .. } => {
                // Register module first so LoadModule can target it for class and const imports.
                let import_idx = self.register_import(&url, Self::native_import_kind(&bindings, &classes, &consts));
                // Every name binds as a module attribute, so it exports and reads as a value, and functions also take the extern_table fast path.
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

    /* `import X` registers module, emits LoadModule+StoreName, VM builds a singleton Val at init. */
    fn resolve_and_bind_all(&mut self, spec: &str, span: (usize, usize), alias: &str) {
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

    /* Star import, Native fills extern_index, Code scans top-level and emits LoadModule+LoadAttr+StoreName per export. */
    fn resolve_and_bind_star(&mut self, spec: &str, span: (usize, usize)) {
        let Some(resolved) = self.resolve_at(spec, span) else { return; };
        match resolved {
            Resolved::Native { bindings, classes, consts, canonical } => {
                for b in &bindings {
                    if !self.push_extern(b, &b.name, span) { break; }
                }
                let import_idx = self.register_import(&canonical, Self::native_import_kind(&bindings, &classes, &consts));
                // Star import binds each export via LoadModule+LoadAttr+StoreName under its name.
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
