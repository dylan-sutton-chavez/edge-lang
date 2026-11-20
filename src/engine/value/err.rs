use alloc::string::String;

use super::{CallFrame, SchedulerStatus};
pub(crate) use crate::engine::lang::{ErrKind, Names};
// the box an engine error rides in which frees it in one call
pub(crate) use crate::engine::r#unsafe::Cold;

/* a runtime error a host reads by its kind and never by its variant */
#[derive(Debug, Clone)]
#[repr(transparent)]
pub struct VmErr(pub(crate) Cause);

/* what went wrong where `HostYield` is an uncatchable signal riding the `Result` chain through `?` */
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Cause {
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
    Engine(crate::engine::r#unsafe::Cold<crate::engine::lang::EngineError>),
}

// the engine's own tests build an error of any cause to read how it renders
#[cfg(feature = "unstable-internals")]
impl From<Cause> for VmErr {
    fn from(c: Cause) -> Self { VmErr(c) }
}

impl<T: Clone> Clone for Cold<T> {
    fn clone(&self) -> Self { Self::new(T::clone(self)) }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Cold<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { T::fmt(self, f) }
}

impl From<crate::engine::lang::EngineError> for VmErr {
    #[cold]
    #[inline(never)]
    fn from(e: crate::engine::lang::EngineError) -> Self {
        use crate::engine::lang::EngineError as E;
        match e {
            // the errors whose facts fit a variant of their own skip the box
            E::Undefined(n) => VmErr(Cause::Name(n)),
            E::ZeroDiv => VmErr(Cause::ZeroDiv),
            E::Overflow => VmErr(Cause::Overflow),
            e => VmErr(Cause::Engine(crate::engine::r#unsafe::Cold::new(e))),
        }
    }
}

impl VmErr {
    /* an error of engine kind `kind` that the runtime names with `message` */
    #[cold]
    pub fn new(kind: ErrKind, message: impl Into<String>) -> Self {
        let m = message.into();
        VmErr(match kind {
            ErrKind::Type => Cause::TypeMsg(m),
            ErrKind::Attribute => Cause::Attribute(m),
            k => Cause::Of(k, m),
        })
    }

    /* an error of the class a program raised with its text as `Class: message` */
    #[cold]
    pub fn raised(text: impl Into<String>) -> Self { VmErr(Cause::Raised(text.into())) }

    /* the error a host function returns to stop the run until the host answers it by id */
    pub fn deferred() -> Self { VmErr(Cause::HostCallDeferred) }

    /* whether this is the stop a host function asked for with `deferred` */
    pub fn is_deferred(&self) -> bool { matches!(self.0, Cause::HostCallDeferred) }

    /* the reason a run paused for its host and none for an error */
    pub fn paused(&self) -> Option<SchedulerStatus> {
        match self.0 { Cause::HostYield(s) => Some(s), _ => None }
    }

    /* whether the run ran out of its op budget, its memory or its call depth */
    pub fn is_limit(&self) -> bool { matches!(self.0, Cause::Budget | Cause::Heap | Cause::CallDepth) }

    /* the engine kind of the error and none for a raised class or a signal */
    #[inline]
    pub fn kind(&self) -> Option<ErrKind> {
        Some(match &self.0 {
            Cause::ZeroDiv => ErrKind::ZeroDiv,
            Cause::Overflow => ErrKind::Overflow,
            Cause::Type(_) | Cause::TypeMsg(_) => ErrKind::Type,
            Cause::Value(_) => ErrKind::Value,
            Cause::Attribute(_) => ErrKind::Attribute,
            Cause::Name(_) => ErrKind::Name,
            Cause::CallDepth => ErrKind::Recursion,
            Cause::Heap => ErrKind::Memory,
            Cause::Budget | Cause::Runtime(_) => ErrKind::Runtime,
            &Cause::Of(k, _) => k,
            Cause::Engine(e) => return e.kind(),
            Cause::Raised(_) | Cause::HostYield(_) | Cause::HostCallDeferred | Cause::Redo => return None,
        })
    }

    /* the name a lookup missed when the error is that miss */
    pub(crate) fn missing_name(&self) -> Option<&str> {
        match &self.0 {
            Cause::Name(n) => Some(n),
            Cause::Engine(e) => match &**e { crate::engine::lang::EngineError::Undefined(n) => Some(n), _ => None },
            _ => None,
        }
    }

    /* whether the engine raised the error under kind `k` and no program raise did */
    #[inline(never)]
    pub(crate) fn is_kind(&self, k: ErrKind) -> bool { self.kind() == Some(k) }

    /* the class name an unwind matches where `Raised` keeps the name the script gave */
    pub(crate) fn class_name(&self, names: &Names) -> String { String::from(self.class(names)) }

    /* `class_name` borrowed from the error or the runtime's names */
    internal! {
        #[inline]
        pub fn class<'a>(&'a self, names: &'a Names) -> &'a str {
            match &self.0 {
                // `Raised` holds a class with or without its message and the class alone matches
                Cause::Raised(s) => s.split(':').next().unwrap_or(s).trim(),
                // unreachable since the embedder catches `HostYield` before rendering a traceback
                Cause::HostYield(_) => "_HostYield",
                // unreachable since `call_extern` catches it
                Cause::HostCallDeferred => "_HostCallDeferred",
                Cause::Redo => "_Redo",
                _ => names.error(self.kind().unwrap_or(ErrKind::Runtime)),
            }
        }
    }

    /* whether the error is engine kind `k` or raises the runtime's class for it */
    internal! {
        pub fn is(&self, k: ErrKind, names: &Names) -> bool {
            match &self.0 {
                Cause::Raised(m) => { let c = names.error(k); crate::engine::lang::leads(m, c) && matches!(m.as_bytes().get(c.len()), None | Some(b':')) }
                _ => self.kind() == Some(k),
            }
        }
    }

    /* the class and message in the runtime's names built in one allocation */
    internal! {
        pub fn render(&self, names: &Names) -> String {
            // a raise already holds its class and message so it renders with no second prefix
            if let Cause::Raised(m) = &self.0 { return m.clone(); }
            // a signal leaking this far reads as a runtime error
            let class = if matches!(self.0, Cause::HostYield(_) | Cause::HostCallDeferred) { names.error(ErrKind::Runtime) } else { self.class(names) };
            let joined = |m: &str| crate::s!(cap: class.len() + 2 + m.len(); str class, ": ", str m);
            match &self.0 {
                Cause::Of(_, m) if m.is_empty() => String::from(class),
                Cause::Name(n) => crate::s!(cap: class.len() + n.len() + 15; str class, ": ", str n, " is not bound"),
                Cause::Type(m) | Cause::Value(m) | Cause::Runtime(m) => joined(m),
                Cause::TypeMsg(m) | Cause::Attribute(m) | Cause::Of(_, m) => joined(m),
                Cause::Engine(e) => joined(&crate::engine::lang::bare(e)),
                Cause::HostYield(_) => joined("scheduler suspended; embedder must drive `run_start` / `run_resume`"),
                Cause::HostCallDeferred => joined("HostCallDeferred leaked past `call_extern` (compiler bug)"),
                Cause::Overflow => joined(names.overflow),
                other => joined(Self::fixed(other)),
            }
        }
    }

    /* the message of an error whose words never change and empty for the rest */
    fn fixed(c: &Cause) -> &'static str {
        match c {
            Cause::ZeroDiv => "division by zero",
            Cause::CallDepth => "calls nested past the recursion limit",
            Cause::Heap => "memory limit",
            Cause::Budget => "budget exceeded",
            Cause::HostYield(_) => "scheduler suspended; embedder must drive run_start / run_resume",
            Cause::HostCallDeferred => "native call deferred to host",
            Cause::Redo => "op handed its call to the loop",
            _ => "",
        }
    }

    /* the message without its class prefix that feeds `e.args` */
    internal! {
        pub fn message(&self, names: &Names) -> String {
            match &self.0 {
                Cause::Name(n) => crate::s!(cap: n.len() + 13; str n, " is not bound"),
                Cause::Type(m) | Cause::Value(m) | Cause::Runtime(m) => String::from(*m),
                Cause::TypeMsg(m) | Cause::Attribute(m) | Cause::Of(_, m) => m.clone(),
                Cause::Engine(e) => crate::engine::lang::bare(e),
                // the part after the class so wrapping it in an `ExcInstance` never doubles the prefix
                Cause::Raised(m) => m.split_once(": ").map_or(String::new(), |(_, msg)| String::from(msg)),
                Cause::Overflow => String::from(names.overflow),
                // each fixed text as its own literal so its copy keeps a known length
                Cause::ZeroDiv => String::from("division by zero"),
                Cause::CallDepth => String::from("calls nested past the recursion limit"),
                Cause::Heap => String::from("memory limit"),
                Cause::Budget => String::from("budget exceeded"),
                other => String::from(Self::fixed(other)),
            }
        }
    }

}

/* a report headed by `msg` and anchored at `byte_pos` when there is one */
pub(crate) fn render_text_at(msg: alloc::string::String, src: &str, byte_pos: Option<usize>, path: Option<&str>) -> alloc::string::String {
    let Some(pos) = byte_pos else { return msg };
    crate::engine::syntax::diag::Diagnostic { start: pos, end: pos, msg }.render(src, path)
}

/* the called from notes of `frames` after a report with `entry_path` naming the entry file */
pub(crate) fn render_calls(out: &mut alloc::string::String, names: &Names, frames: &[CallFrame], function_names: &[alloc::string::String], entry_path: Option<&str>, entry_src: Option<&str>) {
    // split once for every frame that shares the source of the last one
    let mut lines: Option<crate::engine::syntax::diag::Lines> = None;
    // innermost first which is the order an error unwinds them
    for f in frames {
        // a function with no name of its own reads as the runtime's word for one
        let fname = function_names.get(f.fi).map_or(names.unnamed, |s| if s.is_empty() { names.anonymous } else { s.as_str() });
        let pos = f.call_byte_pos as usize;
        // an empty path is the entry file that the error line already names
        let src = f.caller_source.as_deref().map_or("", |s| s.as_str());
        let ran = entry_src.is_some_and(|e| !core::ptr::eq(e, src));
        let path: Option<&str> = f.caller_path.as_deref().map(|p| p.as_str()).filter(|p| !p.is_empty()).or(if ran { Some(names.compiled) } else { entry_path });
        if !lines.as_ref().is_some_and(|l| core::ptr::eq(l.source(), src)) { lines = Some(crate::engine::syntax::diag::Lines::new(src)); }
        out.push('\n');
        // only the top report leads with error so the outer frames become notes
        crate::engine::syntax::diag::Diagnostic {
            start: pos, end: pos,
            msg: crate::s!("called from ", str fname, "()"),
        }.render_to(out, lines.as_ref().unwrap_or(&crate::engine::syntax::diag::Lines::new("")), path, "note: ");
        // a recursion shows its call once with the count of the rest
        if f.repeats > 0 { out.push_str(&alloc::format!("\nnote: the same call {} more times\n", f.repeats)); }
    }
}

/* the names a host thread shows errors in which the last machine it booted set */
#[cfg(all(feature = "std", not(target_arch = "wasm32")))]
pub(crate) mod shown {
    use crate::engine::lang::Names;

    std::thread_local! { static NAMES: core::cell::Cell<&'static Names> = const { core::cell::Cell::new(&Names::PLAIN) }; }

    pub(crate) fn set(names: &'static Names) { NAMES.with(|n| n.set(names)); }
    pub(crate) fn get() -> &'static Names { NAMES.with(|n| n.get()) }
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
#[cold] #[inline(never)] pub(crate) fn cold_heap() -> VmErr { VmErr(Cause::Heap) }
#[cold] #[inline(never)] pub(crate) fn cold_budget() -> VmErr { VmErr(Cause::Budget) }
#[cold] #[inline(never)] pub(crate) fn cold_depth() -> VmErr { VmErr(Cause::CallDepth) }
#[cold] #[inline(never)] pub(crate) fn cold_type(m: &'static str) -> VmErr { VmErr(Cause::Type(m)) }
#[cold] #[inline(never)] pub(crate) fn cold_runtime(m: &'static str) -> VmErr { VmErr(Cause::Runtime(m)) }
#[cold] #[inline(never)] pub(crate) fn cold_overflow() -> VmErr { VmErr(Cause::Overflow) }
// an int result past the inline range of a language growing no int
#[cold] #[inline(never)] pub(crate) fn cold_int_range() -> VmErr { crate::engine::lang::EngineError::IntRange.into() }
