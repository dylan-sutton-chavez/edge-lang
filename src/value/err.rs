use alloc::string::String;

use super::{CallFrame, SchedulerStatus};
pub use crate::lang::{ErrKind, Names};
// the box an engine error rides in which frees it in one call
pub use crate::r#unsafe::Cold;

/* runtime errors where `HostYield` is an uncatchable signal riding the `Result` chain through `?` */
#[derive(Debug, Clone)]
pub enum VmErr {
    CallDepth, Heap, Budget, ZeroDiv, Overflow,
    Name(String),
    Type(&'static str),
    TypeMsg(String),
    Value(&'static str),
    Runtime(&'static str),
    Attribute(String),
    // an engine error kind whose class the runtime names with its message
    Of(ErrKind, String),
    Raised(String),
    HostYield(SchedulerStatus),
    // a deferred native `CallExtern` that `call_extern` catches before it propagates
    HostCallDeferred,
    /* an op handed its call to the loop which reruns the op after that frame */
    Redo,
    // the facts of an engine error worded where it is caught or leaves the machine
    Engine(crate::r#unsafe::Cold<crate::lang::EngineError>),
}

impl<T: Clone> Clone for Cold<T> {
    fn clone(&self) -> Self { Self::new(T::clone(self)) }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Cold<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { T::fmt(self, f) }
}

impl From<crate::lang::EngineError> for VmErr {
    #[cold]
    #[inline(never)]
    fn from(e: crate::lang::EngineError) -> Self {
        use crate::lang::EngineError as E;
        match e {
            // the errors whose facts fit a variant of their own skip the box
            E::Undefined(n) => VmErr::Name(n),
            E::ZeroDiv => VmErr::ZeroDiv,
            E::Overflow => VmErr::Overflow,
            e => VmErr::Engine(crate::r#unsafe::Cold::new(e)),
        }
    }
}

impl VmErr {
    /* the engine kind of the error and none for a raised class or a signal */
    #[inline]
    pub fn kind(&self) -> Option<ErrKind> {
        Some(match self {
            Self::ZeroDiv => ErrKind::ZeroDiv,
            Self::Overflow => ErrKind::Overflow,
            Self::Type(_) | Self::TypeMsg(_) => ErrKind::Type,
            Self::Value(_) => ErrKind::Value,
            Self::Attribute(_) => ErrKind::Attribute,
            Self::Name(_) => ErrKind::Name,
            Self::CallDepth => ErrKind::Recursion,
            Self::Heap => ErrKind::Memory,
            Self::Budget | Self::Runtime(_) => ErrKind::Runtime,
            &Self::Of(k, _) => k,
            Self::Engine(e) => return e.kind(),
            Self::Raised(_) | Self::HostYield(_) | Self::HostCallDeferred | Self::Redo => return None,
        })
    }

    /* the name a lookup missed when the error is that miss */
    pub fn missing_name(&self) -> Option<&str> {
        match self {
            Self::Name(n) => Some(n),
            Self::Engine(e) => match &**e { crate::lang::EngineError::Undefined(n) => Some(n), _ => None },
            _ => None,
        }
    }

    /* whether the engine raised the error under kind `k` and no program raise did */
    #[inline(never)]
    pub fn is_kind(&self, k: ErrKind) -> bool { self.kind() == Some(k) }

    /* the class name an unwind matches where `Raised` keeps the name the script gave */
    pub fn class_name(&self, names: &Names) -> String { String::from(self.class(names)) }

    /* `class_name` borrowed from the error or the runtime's names */
    #[inline]
    pub fn class<'a>(&'a self, names: &'a Names) -> &'a str {
        match self {
            // `Raised` holds a class with or without its message and the class alone matches
            Self::Raised(s) => s.split(':').next().unwrap_or(s).trim(),
            // unreachable since the embedder catches `HostYield` before rendering a traceback
            Self::HostYield(_) => "_HostYield",
            // unreachable since `call_extern` catches it
            Self::HostCallDeferred => "_HostCallDeferred",
            Self::Redo => "_Redo",
            e => names.error(e.kind().unwrap_or(ErrKind::Runtime)),
        }
    }

    /* whether the error is engine kind `k` or raises the runtime's class for it */
    pub fn is(&self, k: ErrKind, names: &Names) -> bool {
        match self {
            Self::Raised(m) => { let c = names.error(k); crate::lang::leads(m, c) && matches!(m.as_bytes().get(c.len()), None | Some(b':')) }
            e => e.kind() == Some(k),
        }
    }

    /* the class and message in the runtime's names built in one allocation */
    pub fn render(&self, names: &Names) -> String {
        // a raise already holds its class and message so it renders with no second prefix
        if let Self::Raised(m) = self { return m.clone(); }
        // a signal leaking this far reads as a runtime error
        let class = if matches!(self, Self::HostYield(_) | Self::HostCallDeferred) { names.error(ErrKind::Runtime) } else { self.class(names) };
        let joined = |m: &str| crate::s!(cap: class.len() + 2 + m.len(); str class, ": ", str m);
        match self {
            Self::Of(_, m) if m.is_empty() => String::from(class),
            Self::Name(n) => crate::s!(cap: class.len() + n.len() + 15; str class, ": ", str n, " is not bound"),
            Self::Type(m) | Self::Value(m) | Self::Runtime(m) => joined(m),
            Self::TypeMsg(m) | Self::Attribute(m) | Self::Of(_, m) => joined(m),
            Self::Engine(e) => joined(&crate::lang::bare(e)),
            Self::HostYield(_) => joined("scheduler suspended; embedder must drive `run_start` / `run_resume`"),
            Self::HostCallDeferred => joined("HostCallDeferred leaked past `call_extern` (compiler bug)"),
            Self::Overflow => joined(names.overflow),
            other => joined(other.fixed()),
        }
    }

    /* the message of an error whose words never change and empty for the rest */
    fn fixed(&self) -> &'static str {
        match self {
            Self::ZeroDiv => "division by zero",
            Self::CallDepth => "calls nested past the recursion limit",
            Self::Heap => "memory limit",
            Self::Budget => "budget exceeded",
            Self::HostYield(_) => "scheduler suspended; embedder must drive run_start / run_resume",
            Self::HostCallDeferred => "native call deferred to host",
            Self::Redo => "op handed its call to the loop",
            _ => "",
        }
    }

    /* the message without its class prefix that feeds `e.args` */
    pub fn message(&self, names: &Names) -> String {
        match self {
            Self::Name(n) => crate::s!(cap: n.len() + 13; str n, " is not bound"),
            Self::Type(m) | Self::Value(m) | Self::Runtime(m) => String::from(*m),
            Self::TypeMsg(m) | Self::Attribute(m) | Self::Of(_, m) => m.clone(),
            Self::Engine(e) => crate::lang::bare(e),
            // the part after the class so wrapping it in an `ExcInstance` never doubles the prefix
            Self::Raised(m) => m.split_once(": ").map_or(String::new(), |(_, msg)| String::from(msg)),
            Self::Overflow => String::from(names.overflow),
            // each fixed text as its own literal so its copy keeps a known length
            Self::ZeroDiv => String::from("division by zero"),
            Self::CallDepth => String::from("calls nested past the recursion limit"),
            Self::Heap => String::from("memory limit"),
            Self::Budget => String::from("budget exceeded"),
            other => String::from(other.fixed()),
        }
    }

    /* `render` with a caret preview at the byte offset when there is one */
    pub fn render_at(&self, names: &Names, src: &str, byte_pos: Option<usize>, path: Option<&str>) -> String { render_text_at(self.render(names), src, byte_pos, path) }

    /* an error line at the site then a called from note per outer frame */
    pub fn render_traceback(&self, names: &Names, error_src: &str, error_byte_pos: Option<usize>, error_path: Option<&str>, frames: &[CallFrame], function_names: &[String]) -> String {
        let mut out = self.render_at(names, error_src, error_byte_pos, error_path);
        render_calls(&mut out, names, frames, function_names, error_path, None);
        out
    }
}

/* a report headed by `msg` and anchored at `byte_pos` when there is one */
pub fn render_text_at(msg: alloc::string::String, src: &str, byte_pos: Option<usize>, path: Option<&str>) -> alloc::string::String {
    let Some(pos) = byte_pos else { return msg };
    crate::syntax::diag::Diagnostic { start: pos, end: pos, msg }.render(src, path)
}

/* the called from notes of `frames` after a report with `entry_path` naming the entry file */
pub fn render_calls(out: &mut alloc::string::String, names: &Names, frames: &[CallFrame], function_names: &[alloc::string::String], entry_path: Option<&str>, entry_src: Option<&str>) {
    // split once for every frame that shares the source of the last one
    let mut lines: Option<crate::syntax::diag::Lines> = None;
    // innermost first which is the order an error unwinds them
    for f in frames {
        // a function with no name of its own reads as the runtime's word for one
        let fname = function_names.get(f.fi).map_or(names.unnamed, |s| if s.is_empty() { names.anonymous } else { s.as_str() });
        let pos = f.call_byte_pos as usize;
        // an empty path is the entry file that the error line already names
        let src = f.caller_source.as_deref().map_or("", |s| s.as_str());
        let ran = entry_src.is_some_and(|e| !core::ptr::eq(e, src));
        let path: Option<&str> = f.caller_path.as_deref().map(|p| p.as_str()).filter(|p| !p.is_empty()).or(if ran { Some(names.compiled) } else { entry_path });
        if !lines.as_ref().is_some_and(|l| core::ptr::eq(l.source(), src)) { lines = Some(crate::syntax::diag::Lines::new(src)); }
        out.push('\n');
        // only the top report leads with error so the outer frames become notes
        crate::syntax::diag::Diagnostic {
            start: pos, end: pos,
            msg: crate::s!("called from ", str fname, "()"),
        }.render_to(out, lines.as_ref().unwrap_or(&crate::syntax::diag::Lines::new("")), path, "note: ");
        // a recursion shows its call once with the count of the rest
        if f.repeats > 0 { out.push_str(&alloc::format!("\nnote: the same call {} more times\n", f.repeats)); }
    }
}

/* the names a host thread shows errors in which the last machine it booted set */
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub mod shown {
    use crate::lang::Names;

    std::thread_local! { static NAMES: core::cell::Cell<&'static Names> = const { core::cell::Cell::new(&Names::PLAIN) }; }

    pub fn set(names: &'static Names) { NAMES.with(|n| n.set(names)); }
    pub fn get() -> &'static Names { NAMES.with(|n| n.get()) }
}

#[cfg(not(target_arch = "wasm32"))]
impl core::fmt::Display for VmErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "std")]
        let names = shown::get();
        #[cfg(not(feature = "std"))]
        let names = &Names::PLAIN;
        f.write_str(&self.render(names))
    }
}

/* out-of-line cold constructors keep the hot dispatch loop icache-friendly */
#[cold] #[inline(never)] pub fn cold_heap() -> VmErr { VmErr::Heap }
#[cold] #[inline(never)] pub fn cold_budget() -> VmErr { VmErr::Budget }
#[cold] #[inline(never)] pub fn cold_depth() -> VmErr { VmErr::CallDepth }
#[cold] #[inline(never)] pub fn cold_type(m: &'static str) -> VmErr { VmErr::Type(m) }
#[cold] #[inline(never)] pub fn cold_value(m: &'static str) -> VmErr { VmErr::Value(m) }
// a value error whose message is built at run time
#[cold] #[inline(never)] pub fn cold_value_s(m: alloc::string::String) -> VmErr { VmErr::Of(ErrKind::Value, m) }
#[cold] #[inline(never)] pub fn cold_index(m: &'static str) -> VmErr { VmErr::Of(ErrKind::Index, m.into()) }
#[cold] #[inline(never)] pub fn cold_runtime(m: &'static str) -> VmErr { VmErr::Runtime(m) }
#[cold] #[inline(never)] pub fn cold_overflow() -> VmErr { VmErr::Overflow }
// an int result past the inline range of a language growing no int
#[cold] #[inline(never)] pub fn cold_int_range() -> VmErr { crate::lang::EngineError::IntRange.into() }
