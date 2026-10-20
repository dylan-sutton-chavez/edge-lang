use alloc::string::String;

use super::{CallFrame, SchedulerStatus};

/* Runtime errors, static variants alloc-free, dynamic carry user text. `HostYield` is a control-flow signal (not catchable by Python try/except), riding the `Result` chain so `?` propagation reuses without parallel signaling. */
#[derive(Debug, Clone)]
pub enum VmErr {
    CallDepth, Heap, Budget, ZeroDiv, Overflow,
    Name(String),
    Type(&'static str),
    TypeMsg(String),
    Value(&'static str),
    Runtime(&'static str),
    Attribute(String),
    Raised(String),
    HostYield(SchedulerStatus),
    /// Native `CallExtern` deferred to host, caught locally by `call_extern`, never propagates.
    HostCallDeferred,
    /* An op's handler handed its call to the loop, which runs it as a frame and the op again after it, so it never propagates past the loop. */
    Redo,
}

impl VmErr {
    /* Class-name lookup used by the exception unwind path and the `with` cleanup opcode. Native variants map below, `Raised` keeps the user-supplied name. */
    pub fn class_name(&self) -> alloc::string::String {
        match self {
            Self::ZeroDiv => "ZeroDivisionError".into(),
            Self::Overflow => "OverflowError".into(),
            Self::Type(_) | Self::TypeMsg(_) => "TypeError".into(),
            Self::Value(_) => "ValueError".into(),
            Self::Attribute(_) => "AttributeError".into(),
            Self::Name(_) => "NameError".into(),
            Self::CallDepth => "RecursionError".into(),
            Self::Heap => "MemoryError".into(),
            Self::Budget | Self::Runtime(_) => "RuntimeError".into(),
            // `Raised` carries "Class" or "Class: message", the bare class name drives except-matching.
            Self::Raised(s) => s.split(':').next().unwrap_or(s).trim().into(),
            // Unreachable in correct hosts, embedder catches HostYield before traceback rendering.
            Self::HostYield(_) => "_HostYield".into(),
            // Caught locally in call_extern, unreachable here.
            Self::HostCallDeferred => "_HostCallDeferred".into(),
            Self::Redo => "_Redo".into(),
        }
    }

    pub fn render(&self) -> alloc::string::String {
        use crate::s;
        match self {
            Self::Name(n) => s!("NameError: name '", str n, "' is not defined"),
            // Already formatted as "Class" or "Class: message", render verbatim (no double prefix).
            Self::Raised(m) => m.clone(),
            Self::Type(m) => s!("TypeError: ", str m),
            Self::TypeMsg(m) => s!("TypeError: ", str m),
            Self::Value(m) => s!("ValueError: ", str m),
            Self::Runtime(m) => s!("RuntimeError: ", str m),
            Self::Attribute(m) => s!("AttributeError: ", str m),
            Self::HostYield(_) => alloc::string::String::from("RuntimeError: scheduler suspended; embedder must drive `run_start` / `run_resume`"),
            Self::HostCallDeferred => alloc::string::String::from("RuntimeError: HostCallDeferred leaked past `call_extern` (compiler bug)"),
            other => s!(str &other.class_name(), ": ", str &other.message()),
        }
    }

    /* Message-only form (no "Class:" prefix), feeds `e.args`. */
    pub fn message(&self) -> alloc::string::String {
        use alloc::string::String;
        match self {
            Self::Name(n) => crate::s!("name '", str n, "' is not defined"),
            Self::Type(m) | Self::Value(m) | Self::Runtime(m) => String::from(*m),
            Self::TypeMsg(m) | Self::Attribute(m) => m.clone(),
            // `Raised` carries "Class" or "Class: message". The message is the part after the class (empty for a bare class), so re-wrapping into an ExcInstance doesn't double the class prefix.
            Self::Raised(m) => m.split_once(": ").map_or(String::new(), |(_, msg)| String::from(msg)),
            Self::ZeroDiv => String::from("division by zero"),
            Self::Overflow => String::from("Python int too large to convert to C ssize_t"),
            Self::CallDepth => String::from("maximum recursion depth exceeded"),
            Self::Heap => String::from("memory limit"),
            Self::Budget => String::from("budget exceeded"),
            Self::HostYield(_) => String::from("scheduler suspended; embedder must drive run_start / run_resume"),
            Self::HostCallDeferred => String::from("native call deferred to host"),
            Self::Redo => String::from("op handed its call to the loop"),
        }
    }

    /* `render()` anchored at a byte offset for rustc-style caret preview, falls back when absent. */
    pub fn render_at(&self, src: &str, byte_pos: Option<usize>, path: Option<&str>) -> alloc::string::String { render_text_at(self.render(), src, byte_pos, path) }

    /* Multi-frame traceback, `error:` at the site, then `note: called from ...` outward. */
    pub fn render_traceback(&self, error_src: &str, error_byte_pos: Option<usize>, error_path: Option<&str>, frames: &[CallFrame], function_names: &[alloc::string::String]) -> alloc::string::String {
        let mut out = self.render_at(error_src, error_byte_pos, error_path);
        render_calls(&mut out, frames, function_names, error_path, None);
        out
    }
}

/* A report headed by `msg`, anchored at a byte offset for the caret preview when there is one. */
pub fn render_text_at(msg: alloc::string::String, src: &str, byte_pos: Option<usize>, path: Option<&str>) -> alloc::string::String {
    let Some(pos) = byte_pos else { return msg };
    crate::parser::Diagnostic { start: pos, end: pos, msg }.render(src, path)
}

/* The `note: called from ...` lines of `frames` after a report, `entry_path` naming frames of the entry file, whose
   text, when known, tells it from a text `exec` ran there. */
pub fn render_calls(out: &mut alloc::string::String, frames: &[CallFrame], function_names: &[alloc::string::String], entry_path: Option<&str>, entry_src: Option<&str>) {
    // The lines of the source the last frame named, found once for the frames that share it.
    let mut lines: Option<crate::parser::Lines> = None;
    // Frames come innermost first, the order an error unwinds them.
    for f in frames {
        let fname = match f.fi {
            super::EXEC_FRAME => "exec",
            super::EVAL_FRAME => "eval",
            fi => function_names.get(fi).map(|s| s.as_str()).unwrap_or("<anonymous>"),
        };
        let pos = f.call_byte_pos as usize;
        // An empty path is the entry file, which the error line names.
        let src = f.caller_source.as_deref().map_or("", |s| s.as_str());
        let ran = entry_src.is_some_and(|e| !core::ptr::eq(e, src));
        let path: Option<&str> = f.caller_path.as_deref().map(|p| p.as_str()).filter(|p| !p.is_empty()).or(if ran { Some("<string>") } else { entry_path });
        if !lines.as_ref().is_some_and(|l| core::ptr::eq(l.source(), src)) { lines = Some(crate::parser::Lines::new(src)); }
        let note = crate::parser::Diagnostic {
            start: pos, end: pos,
            msg: alloc::format!("called from {}()", fname),
        }.render_in(lines.as_ref().unwrap_or(&crate::parser::Lines::new("")), path);
        // Demote chained-frame prefix so only the top line reads as `error:`.
        let note = note.replacen("error:", "note:", 1);
        out.push('\n');
        out.push_str(&note);
        // A recursion shows its call once, with the count of the rest.
        if f.repeats > 0 { out.push_str(&alloc::format!("\nnote: the same call {} more times\n", f.repeats)); }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl core::fmt::Display for VmErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.render())
    }
}

/* Cold, out-of-line constructors keep the hot dispatch loop icache-friendly. */
#[cold] #[inline(never)] pub fn cold_heap() -> VmErr { VmErr::Heap }
#[cold] #[inline(never)] pub fn cold_budget() -> VmErr { VmErr::Budget }
#[cold] #[inline(never)] pub fn cold_depth() -> VmErr { VmErr::CallDepth }
#[cold] #[inline(never)] pub fn cold_type(m: &'static str) -> VmErr { VmErr::Type(m) }
#[cold] #[inline(never)] pub fn cold_value(m: &'static str) -> VmErr { VmErr::Value(m) }
// A ValueError whose message is built at run time.
#[cold] #[inline(never)] pub fn cold_value_s(m: alloc::string::String) -> VmErr { VmErr::Raised(alloc::format!("ValueError: {m}")) }
#[cold] #[inline(never)] pub fn cold_index(m: &'static str) -> VmErr { VmErr::Raised(crate::s!("IndexError: ", str m)) }
#[cold] #[inline(never)] pub fn cold_runtime(m: &'static str) -> VmErr { VmErr::Runtime(m) }
#[cold] #[inline(never)] pub fn cold_overflow() -> VmErr { VmErr::Overflow }
