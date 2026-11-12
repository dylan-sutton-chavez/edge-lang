use afl::fuzz;

use edge::code::{SSAChunk, Parser};
use edge::deps::NoopResolver;
use edge::embed as snapshot;
use edge::embed::{Limits, VM};
use edge::lang::Runtime;
use edge::lexer::lex_as;
use edge::value::{SchedulerStatus, VmErr};
use rig::Rig;

// every run meters what it allocates as edge.wasm does
edge::allocator!();

// Tight so fuzzed loops preempt and unwind.
const PREEMPT_EVERY: usize = 7;
// Caps wake-ups so receive loops terminate.
const MAX_WAKEUPS: u32 = 16;

fn main() {
    fuzz!(|data: &[u8]| {
        // Source is text, reject non-UTF-8 rather than counting it as coverage.
        let Ok(src) = core::str::from_utf8(data) else { return };

        // every input reads through the Lox compiler first
        let (chunk, errs) = edge::code::compile(&lox::FRONT, src);
        if errs.is_empty() { drive(&lox::RUNTIME, &chunk, src); }

        // the rig reaches the spelled parser and the VM pieces Lox lacks
        let (tokens, _lex_errs) = lex_as::<Rig>(src);
        let (chunk, parse_errs) = Parser::<_, Rig>::spelled(src, tokens.into_iter(), Box::new(NoopResolver)).parse();
        // Only valid programs reach the VM, the chunk is unreliable after a parse error.
        if parse_errs.is_empty() { drive(&rig::RUNTIME, &chunk, src); }
    });
}

/* runs `chunk` on `rt` through every park kind and a snapshot of the first */
fn drive(rt: &'static Runtime, chunk: &SSAChunk, src: &str) {
    // Bounded budget turns runaway loops and allocations into VmErr, not hangs. Tight `ops` so bounded loops finish within AFL's hang timeout. Library default `sandbox()` is far larger.
    let limits = Limits { ops: 100_000, ..Limits::sandbox() };
    let mut vm = VM::with_runtime(rt, chunk, limits);
    vm.set_preempt_interval(PREEMPT_EVERY);

    let mut hopped = false;
    let mut wakeups = 0;
    while let Err(VmErr::HostYield(park)) = vm.run() {
        let drivable = matches!(park, SchedulerStatus::Preempted | SchedulerStatus::PendingEvent);
        if !drivable { break; }

        if !hopped {
            hopped = true;
            // every kind a runtime registers saves and a refusing kind leaves the run as it is
            if let Ok(blob) = snapshot::save(&vm, src, "") {
                let mut fresh = VM::with_runtime(rt, chunk, limits);
                fresh.set_preempt_interval(PREEMPT_EVERY);
                if snapshot::restore(&mut fresh, &blob).is_ok() {
                    vm = fresh;
                }
            }
        }

        if matches!(park, SchedulerStatus::Preempted) { continue; }
        // receive() needs an event.
        if wakeups >= MAX_WAKEUPS { break; }
        wakeups += 1;
        if matches!(park, SchedulerStatus::PendingEvent) && vm.push_event("e").is_err() { break; }
    }
}
