use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use edge::code::Diagnostic;
use edge::lexer::{lex_as, TokenType as T};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    LParen, RParen, LBrace, RBrace, Comma, Dot, Minus, Plus, Semi, Slash, Star,
    Bang, BangEq, Eq, EqEq, Gt, GtEq, Lt, LtEq, Ident, Str, Num,
    And, Class, Else, False, For, Fun, If, Nil, Or, Print, Return, Super, This, True, Var, While,
    Error, Eof,
}

#[derive(Clone, Copy)]
struct Tok { k: K, start: usize, end: usize, msg: &'static str }

/* the lox token a word or symbol of the table is */
fn kind(t: T, text: &str) -> Option<K> {
    Some(match t {
        T::ParenOpen => K::LParen, T::ParenClose => K::RParen, T::BraceOpen | T::BlockOpen => K::LBrace, T::BraceClose | T::BlockClose => K::RBrace,
        T::Comma => K::Comma, T::Dot => K::Dot, T::Minus => K::Minus, T::Plus => K::Plus, T::Semi => K::Semi, T::Slash => K::Slash, T::Star => K::Star,
        T::Not => K::Bang, T::NotEqual => K::BangEq, T::Equal => K::Eq, T::EqualEqual => K::EqEq, T::Greater => K::Gt, T::GreaterEqual => K::GtEq,
        T::Less => K::Lt, T::LessEqual => K::LtEq, T::String => K::Str, T::Int | T::Float => K::Num,
        T::Comment | T::Newline | T::LineBreak | T::Indent | T::Dedent => return None,
        T::Eof => K::Eof,
        _ => match text {
            "and" => K::And, "class" => K::Class, "else" => K::Else, "false" => K::False, "for" => K::For, "fun" => K::Fun, "if" => K::If,
            "nil" => K::Nil, "or" => K::Or, "print" => K::Print, "return" => K::Return, "super" => K::Super, "this" => K::This,
            "true" => K::True, "var" => K::Var, "while" => K::While, _ => K::Ident,
        },
    })
}

/* the tokens of `src` with each scan error as an error token where it rose */
fn scan(src: &str) -> Vec<Tok> {
    let (toks, errs) = lex_as::<crate::Lox>(src);
    let mut out: Vec<Tok> = toks.iter().filter_map(|t| kind(t.kind, &src[t.start..t.end]).map(|k| Tok { k, start: t.start, end: t.end, msg: "" })).collect();
    for e in errs {
        let msg = if e.msg.contains("unterminated string") { "Unterminated string." } else { "Unexpected character." };
        let at = out.partition_point(|t| t.start < e.start && t.k != K::Eof);
        out.insert(at, Tok { k: K::Error, start: e.start, end: e.end, msg });
    }
    out
}

/* the modules `src` imports as `import("spec")` names them */
pub fn imports(src: &str) -> Vec<edge::deps::ImportSpec> {
    use edge::deps::ImportSpec;
    let toks = scan(src);
    toks.windows(4).filter(|w| w[0].k == K::Ident && &src[w[0].start..w[0].end] == "import" && w[1].k == K::LParen && w[2].k == K::Str && w[3].k == K::RParen)
        .map(|w| String::from(&src[w[2].start + 1..w[2].end - 1]))
        .map(|s| if s.starts_with("./") || s.starts_with("../") { ImportSpec::Relative(s) } else if s.contains('/') { ImportSpec::Root(s) } else { ImportSpec::Bare(s) })
        .collect()
}

/* where a variable lives once resolved */
#[derive(Clone, Debug)]
pub enum Place { Local(u32), Global(String) }

pub enum Expr {
    Num(f64), Str(String), Bool(bool), Nil,
    Get(Place, usize),
    Set(Place, Box<Expr>, usize),
    Neg(Box<Expr>, usize),
    Not(Box<Expr>),
    Binary(edge::code::OpCode, Box<Expr>, Box<Expr>, usize),
    Logic(bool, Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>, usize),
    Field(Box<Expr>, String, usize),
    SetField(Box<Expr>, String, Box<Expr>, usize),
    Super { sup: Place, this: Place, name: String, at: usize },
    Import(String, usize),
}

pub enum Stmt {
    Expr(Expr),
    Print(Expr, usize),
    Define(Place, Expr),
    Fun(Place, Box<Fun>),
    Block(Vec<Stmt>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    While(Expr, Box<Stmt>),
    Return(Option<Expr>),
    Class { place: Place, name: String, sup: Option<(Expr, u32, usize)>, methods: Vec<Fun> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind { Script, Function, Method, Init }

pub struct Fun {
    pub name: String,
    pub kind: Kind,
    // the receiver first for a method then the declared parameters
    pub params: Vec<u32>,
    pub body: Vec<Stmt>,
    // the names it reads or writes which bodies around it own
    pub outer: Vec<String>,
}

pub struct Local { pub name: String, pub captured: bool }

pub struct Program { pub script: Fun, pub locals: Vec<Local> }

impl Program {
    /* the engine name of local `id` which no lox name can spell */
    pub fn name(&self, id: u32) -> String { format!("{}@{id}", self.locals[id as usize].name) }
}

struct Scope { name: String, depth: i32, id: u32 }

struct Compiler { kind: Kind, scope: Vec<Scope>, depth: i32, outer: Vec<String> }

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Prec { None, Assign, Or, And, Equality, Comparison, Term, Factor, Unary, Call }

struct Parser<'s> {
    src: &'s str,
    toks: Vec<Tok>,
    at: usize,
    prev: Tok,
    cur: Tok,
    panic: bool,
    errors: Vec<Diagnostic>,
    fns: Vec<Compiler>,
    classes: Vec<bool>,
    locals: Vec<Local>,
}

/* the program of `src` resolved or every error in the book's words */
pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let toks = scan(src);
    let eof = Tok { k: K::Eof, start: src.len(), end: src.len(), msg: "" };
    let mut p = Parser { src, toks, at: 0, prev: eof, cur: eof, panic: false, errors: Vec::new(), fns: Vec::new(), classes: Vec::new(), locals: Vec::new() };
    p.open(Kind::Script);
    p.advance();
    let mut body = Vec::new();
    while !p.matches(K::Eof) { body.push(p.declaration()); }
    let (_, outer) = p.close();
    let script = Fun { name: "script".into(), kind: Kind::Script, params: Vec::new(), body, outer };
    if p.errors.is_empty() { Ok(Program { script, locals: p.locals }) } else { Err(p.errors) }
}

impl Parser<'_> {
    fn text(&self, t: Tok) -> &str { &self.src[t.start..t.end] }

    fn error_at(&mut self, t: Tok, msg: &str) {
        if self.panic { return; }
        self.panic = true;
        let msg = match t.k {
            K::Eof => format!("Error at end: {msg}"),
            K::Error => format!("Error: {msg}"),
            _ => format!("Error at '{}': {msg}", self.text(t)),
        };
        self.errors.push(Diagnostic { start: t.start, end: t.end, msg });
    }

    fn error(&mut self, msg: &str) { self.error_at(self.prev, msg); }

    fn advance(&mut self) {
        self.prev = self.cur;
        loop {
            self.cur = self.toks.get(self.at).copied().unwrap_or(Tok { k: K::Eof, start: self.src.len(), end: self.src.len(), msg: "" });
            self.at += 1;
            if self.cur.k != K::Error { break; }
            self.error_at(self.cur, self.cur.msg);
        }
    }

    fn consume(&mut self, k: K, msg: &str) {
        if self.cur.k == k { return self.advance(); }
        self.error_at(self.cur, msg);
    }

    fn check(&self, k: K) -> bool { self.cur.k == k }

    fn matches(&mut self, k: K) -> bool {
        if !self.check(k) { return false; }
        self.advance();
        true
    }

    fn open(&mut self, kind: Kind) {
        let mut c = Compiler { kind, scope: Vec::new(), depth: 0, outer: Vec::new() };
        // slot zero holds the receiver of a method and nothing a name reaches otherwise
        let receiver = if matches!(kind, Kind::Method | Kind::Init) { "this" } else { "" };
        c.scope.push(Scope { name: receiver.into(), depth: 0, id: self.new_local(receiver) });
        self.fns.push(c);
    }

    fn close(&mut self) -> (u32, Vec<String>) {
        let c = self.fns.pop().expect("an open function");
        (c.scope[0].id, c.outer)
    }

    fn new_local(&mut self, name: &str) -> u32 {
        self.locals.push(Local { name: name.into(), captured: false });
        self.locals.len() as u32 - 1
    }

    fn top(&mut self) -> &mut Compiler { self.fns.last_mut().expect("an open function") }

    fn begin_scope(&mut self) { self.top().depth += 1; }

    fn end_scope(&mut self) {
        let c = self.top();
        c.depth -= 1;
        while c.scope.last().is_some_and(|l| l.depth > c.depth) { c.scope.pop(); }
    }

    fn synchronize(&mut self) {
        self.panic = false;
        while self.cur.k != K::Eof {
            if self.prev.k == K::Semi { return; }
            if matches!(self.cur.k, K::Class | K::Fun | K::Var | K::For | K::If | K::While | K::Print | K::Return) { return; }
            self.advance();
        }
    }

    fn declaration(&mut self) -> Stmt {
        let s = if self.matches(K::Class) { self.class() }
            else if self.matches(K::Fun) { self.fun_decl() }
            else if self.matches(K::Var) { self.var_decl() }
            else { self.statement() };
        if self.panic { self.synchronize(); }
        s
    }

    fn statement(&mut self) -> Stmt {
        if self.matches(K::Print) {
            let at = self.prev.start;
            let e = self.expression();
            self.consume(K::Semi, "Expect ';' after value.");
            Stmt::Print(e, at)
        } else if self.matches(K::For) {
            self.for_stmt()
        } else if self.matches(K::If) {
            self.consume(K::LParen, "Expect '(' after 'if'.");
            let c = self.expression();
            self.consume(K::RParen, "Expect ')' after condition.");
            let then = Box::new(self.statement());
            let other = if self.matches(K::Else) { Some(Box::new(self.statement())) } else { None };
            Stmt::If(c, then, other)
        } else if self.matches(K::Return) {
            self.return_stmt()
        } else if self.matches(K::While) {
            self.consume(K::LParen, "Expect '(' after 'while'.");
            let c = self.expression();
            self.consume(K::RParen, "Expect ')' after condition.");
            Stmt::While(c, Box::new(self.statement()))
        } else if self.matches(K::LBrace) {
            self.begin_scope();
            let b = self.block();
            self.end_scope();
            Stmt::Block(b)
        } else {
            let e = self.expression();
            self.consume(K::Semi, "Expect ';' after expression.");
            Stmt::Expr(e)
        }
    }

    fn block(&mut self) -> Vec<Stmt> {
        let mut out = Vec::new();
        while !self.check(K::RBrace) && !self.check(K::Eof) { out.push(self.declaration()); }
        self.consume(K::RBrace, "Expect '}' after block.");
        out
    }

    fn return_stmt(&mut self) -> Stmt {
        let kind = self.top().kind;
        if kind == Kind::Script { self.error("Can't return from top-level code."); }
        if self.matches(K::Semi) { return Stmt::Return(None); }
        if kind == Kind::Init { self.error("Can't return a value from an initializer."); }
        let e = self.expression();
        self.consume(K::Semi, "Expect ';' after return value.");
        Stmt::Return(Some(e))
    }

    // the clauses run as the book's while loop inside a scope of their own
    fn for_stmt(&mut self) -> Stmt {
        self.begin_scope();
        self.consume(K::LParen, "Expect '(' after 'for'.");
        let init = if self.matches(K::Semi) { None } else if self.matches(K::Var) { Some(self.var_decl()) } else {
            let e = self.expression();
            self.consume(K::Semi, "Expect ';' after expression.");
            Some(Stmt::Expr(e))
        };
        let cond = if self.matches(K::Semi) { Expr::Bool(true) } else {
            let e = self.expression();
            self.consume(K::Semi, "Expect ';' after loop condition.");
            e
        };
        let step = if self.matches(K::RParen) { None } else {
            let e = self.expression();
            self.consume(K::RParen, "Expect ')' after for clauses.");
            Some(Stmt::Expr(e))
        };
        let body = self.statement();
        self.end_scope();
        let body = Stmt::Block(core::iter::once(body).chain(step).collect());
        Stmt::Block(init.into_iter().chain(core::iter::once(Stmt::While(cond, Box::new(body)))).collect())
    }

    /* declares the name just read in the current scope and gives where it lives */
    fn declare(&mut self) -> Place {
        let name = self.text(self.prev).to_string();
        if self.top().depth == 0 {
            let c = self.top();
            if !c.outer.contains(&name) { c.outer.push(name.clone()); }
            return Place::Global(name);
        }
        let c = self.fns.last().expect("an open function");
        let clash = c.scope.iter().rev().take_while(|l| l.depth == -1 || l.depth >= c.depth).any(|l| l.name == name);
        if clash { self.error("Already a variable with this name in this scope."); }
        let id = self.new_local(&name);
        self.top().scope.push(Scope { name, depth: -1, id });
        Place::Local(id)
    }

    fn variable(&mut self, msg: &str) -> Place {
        self.consume(K::Ident, msg);
        self.declare()
    }

    fn initialized(&mut self) {
        let c = self.top();
        if c.depth == 0 { return; }
        let depth = c.depth;
        if let Some(l) = c.scope.last_mut() { l.depth = depth; }
    }

    fn var_decl(&mut self) -> Stmt {
        let place = self.variable("Expect variable name.");
        let e = if self.matches(K::Eq) { self.expression() } else { Expr::Nil };
        self.consume(K::Semi, "Expect ';' after variable declaration.");
        self.initialized();
        Stmt::Define(place, e)
    }

    fn fun_decl(&mut self) -> Stmt {
        let place = self.variable("Expect function name.");
        self.initialized();
        let f = self.function(Kind::Function);
        Stmt::Fun(place, Box::new(f))
    }

    fn function(&mut self, kind: Kind) -> Fun {
        let name = self.text(self.prev).to_string();
        self.open(kind);
        self.begin_scope();
        self.consume(K::LParen, "Expect '(' after function name.");
        let mut params = Vec::new();
        if matches!(kind, Kind::Method | Kind::Init) { params.push(self.top().scope[0].id); }
        let mut arity = 0;
        if !self.check(K::RParen) {
            loop {
                arity += 1;
                if arity > 255 { self.error_at(self.cur, "Can't have more than 255 parameters."); }
                if let Place::Local(id) = self.variable("Expect parameter name.") { params.push(id); }
                self.initialized();
                if !self.matches(K::Comma) { break; }
            }
        }
        self.consume(K::RParen, "Expect ')' after parameters.");
        self.consume(K::LBrace, "Expect '{' before function body.");
        let body = self.block();
        let (_, outer) = self.close();
        Fun { name, kind, params, body, outer }
    }

    fn class(&mut self) -> Stmt {
        self.consume(K::Ident, "Expect class name.");
        let name = self.text(self.prev).to_string();
        let place = self.declare();
        self.initialized();
        self.classes.push(false);
        let mut sup = None;
        if self.matches(K::Lt) {
            self.consume(K::Ident, "Expect superclass name.");
            let at = self.prev.start;
            let value = self.named(self.prev, false);
            if self.text(self.prev) == name { self.error("A class can't inherit from itself."); }
            self.begin_scope();
            let id = self.new_local("super");
            let depth = self.top().depth;
            self.top().scope.push(Scope { name: "super".into(), depth, id });
            if let Some(has) = self.classes.last_mut() { *has = true; }
            sup = Some((value, id, at));
        }
        self.consume(K::LBrace, "Expect '{' before class body.");
        let mut methods = Vec::new();
        while !self.check(K::RBrace) && !self.check(K::Eof) {
            self.consume(K::Ident, "Expect method name.");
            let kind = if self.text(self.prev) == "init" { Kind::Init } else { Kind::Method };
            methods.push(self.function(kind));
        }
        self.consume(K::RBrace, "Expect '}' after class body.");
        if sup.is_some() { self.end_scope(); }
        self.classes.pop();
        Stmt::Class { place, name, sup, methods }
    }

    /* the slot of `name` in function `f` checking it is no longer being initialized */
    fn local_in(&mut self, f: usize, name: &str) -> Option<u32> {
        let (id, depth) = self.fns[f].scope.iter().rev().find(|l| l.name == name).map(|l| (l.id, l.depth))?;
        if depth == -1 { self.error("Can't read local variable in its own initializer."); }
        Some(id)
    }

    /* where `name` lives as seen from the innermost function noting what it captures */
    fn resolve(&mut self, name: &str) -> Place {
        let last = self.fns.len() - 1;
        if let Some(id) = self.local_in(last, name) { return Place::Local(id); }
        for f in (0..last).rev() {
            if let Some(id) = self.local_in(f, name) {
                self.locals[id as usize].captured = true;
                let full = format!("{name}@{id}");
                for c in &mut self.fns[f + 1..] { if !c.outer.contains(&full) { c.outer.push(full.clone()); } }
                return Place::Local(id);
            }
        }
        let c = self.top();
        if !c.outer.iter().any(|n| n == name) { c.outer.push(name.into()); }
        Place::Global(name.into())
    }

    fn named(&mut self, t: Tok, can_assign: bool) -> Expr {
        let name = self.text(t).to_string();
        let place = self.resolve(&name);
        if can_assign && self.matches(K::Eq) {
            let v = self.expression();
            return Expr::Set(place, Box::new(v), t.start);
        }
        Expr::Get(place, t.start)
    }

    fn expression(&mut self) -> Expr { self.precedence(Prec::Assign) }

    fn infix(k: K) -> Prec {
        match k {
            K::LParen | K::Dot => Prec::Call,
            K::Minus | K::Plus => Prec::Term,
            K::Slash | K::Star => Prec::Factor,
            K::BangEq | K::EqEq => Prec::Equality,
            K::Gt | K::GtEq | K::Lt | K::LtEq => Prec::Comparison,
            K::And => Prec::And,
            K::Or => Prec::Or,
            _ => Prec::None,
        }
    }

    fn precedence(&mut self, prec: Prec) -> Expr {
        self.advance();
        let can_assign = prec <= Prec::Assign;
        let Some(mut e) = self.prefix(can_assign) else {
            self.error("Expect expression.");
            return Expr::Nil;
        };
        while prec <= Self::infix(self.cur.k) {
            self.advance();
            e = self.infix_of(e, can_assign);
        }
        if can_assign && self.matches(K::Eq) { self.error("Invalid assignment target."); }
        e
    }

    fn prefix(&mut self, can_assign: bool) -> Option<Expr> {
        let t = self.prev;
        Some(match t.k {
            K::LParen => {
                let e = self.expression();
                self.consume(K::RParen, "Expect ')' after expression.");
                e
            }
            K::Minus => Expr::Neg(Box::new(self.precedence(Prec::Unary)), t.start),
            K::Bang => Expr::Not(Box::new(self.precedence(Prec::Unary))),
            K::Ident => self.named(t, can_assign),
            K::Str => Expr::Str(self.src[t.start + 1..t.end - 1].into()),
            K::Num => Expr::Num(self.text(t).parse().unwrap_or(0.0)),
            K::False => Expr::Bool(false),
            K::True => Expr::Bool(true),
            K::Nil => Expr::Nil,
            K::This => {
                if self.classes.is_empty() {
                    self.error("Can't use 'this' outside of a class.");
                    return Some(Expr::Nil);
                }
                self.named(t, false)
            }
            K::Super => return Some(self.super_expr(t)),
            _ => return None,
        })
    }

    fn super_expr(&mut self, t: Tok) -> Expr {
        match self.classes.last() {
            None => self.error("Can't use 'super' outside of a class."),
            Some(false) => self.error("Can't use 'super' in a class with no superclass."),
            Some(true) => {}
        }
        self.consume(K::Dot, "Expect '.' after 'super'.");
        self.consume(K::Ident, "Expect superclass method name.");
        let name = self.text(self.prev).to_string();
        let this = self.resolve("this");
        let sup = self.resolve("super");
        let e = Expr::Super { sup, this, name, at: t.start };
        if self.matches(K::LParen) {
            let (args, at) = self.arguments();
            return Expr::Call(Box::new(e), args, at);
        }
        e
    }

    fn arguments(&mut self) -> (Vec<Expr>, usize) {
        let mut args = Vec::new();
        if !self.check(K::RParen) {
            loop {
                args.push(self.expression());
                if args.len() == 256 { self.error("Can't have more than 255 arguments."); }
                if !self.matches(K::Comma) { break; }
            }
        }
        self.consume(K::RParen, "Expect ')' after arguments.");
        (args, self.prev.start)
    }

    fn infix_of(&mut self, left: Expr, can_assign: bool) -> Expr {
        use edge::code::OpCode as O;
        let t = self.prev;
        let op = match t.k {
            K::LParen => {
                let (args, at) = self.arguments();
                // a module named by a literal binds as the program compiles
                if let (Expr::Get(Place::Global(f), at), [Expr::Str(spec)]) = (&left, args.as_slice()) && f == "import" {
                    return Expr::Import(spec.clone(), *at);
                }
                return Expr::Call(Box::new(left), args, at);
            }
            K::Dot => {
                self.consume(K::Ident, "Expect property name after '.'.");
                let (name, at) = (self.text(self.prev).to_string(), self.prev.start);
                if can_assign && self.matches(K::Eq) {
                    let v = self.expression();
                    return Expr::SetField(Box::new(left), name, Box::new(v), at);
                }
                if self.matches(K::LParen) {
                    let (args, call) = self.arguments();
                    return Expr::Call(Box::new(Expr::Field(Box::new(left), name, at)), args, call);
                }
                return Expr::Field(Box::new(left), name, at);
            }
            K::And | K::Or => {
                let right = self.precedence(if t.k == K::And { Prec::And } else { Prec::Or });
                return Expr::Logic(t.k == K::And, Box::new(left), Box::new(right));
            }
            K::Plus => O::Add, K::Minus => O::Sub, K::Star => O::Mul, K::Slash => O::Div,
            K::EqEq => O::Eq, K::BangEq => O::NotEq, K::Lt => O::Lt, K::LtEq => O::LtEq, K::Gt => O::Gt, K::GtEq => O::GtEq,
            _ => return left,
        };
        let next = match Self::infix(t.k) { Prec::Term => Prec::Factor, Prec::Factor => Prec::Unary, Prec::Equality => Prec::Comparison, _ => Prec::Term };
        let right = self.precedence(next);
        Expr::Binary(op, Box::new(left), Box::new(right), t.start)
    }
}
