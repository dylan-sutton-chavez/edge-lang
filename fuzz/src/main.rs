use afl::fuzz;

use edge::lexer::lex_as;
use edge::code::Parser;
use edge::deps::NoopResolver;
use mini::Mini;
use edge::embed as snapshot;
use edge::value::{SchedulerStatus, VmErr};
use edge::embed::{Limits, VM};

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

        // mini's spelling which every program of the repo is written in
        let (tokens, _lex_errs) = lex_as::<Mini>(src);
        let (chunk, parse_errs) = Parser::<_, Mini>::spelled(src, tokens.into_iter(), Box::new(NoopResolver)).parse();

        // Only valid programs reach the VM, the chunk is unreliable after a parse error.
        if !parse_errs.is_empty() {
            return;
        }

        // Bounded budget turns runaway loops and allocations into VmErr, not hangs. Tight `ops` so bounded loops finish within AFL's hang timeout. Library default `sandbox()` is far larger.
        let limits = Limits { ops: 100_000, ..Limits::sandbox() };
        let mut vm = VM::with_runtime(&mini::RUNTIME, &chunk, limits);
        vm.set_preempt_interval(PREEMPT_EVERY);

        // Drive every park kind, snapshot the first.
        let mut hopped = false;
        let mut wakeups = 0;
        while let Err(VmErr::HostYield(park)) = vm.run() {
            let drivable = matches!(park, SchedulerStatus::Preempted | SchedulerStatus::PendingEvent);
            if !drivable { break; }

            if !hopped {
                hopped = true;
                // every kind mini registers saves and a refusing kind leaves the run as it is
                if let Ok(blob) = snapshot::save(&vm, src, "") {
                    let mut fresh = VM::with_runtime(&mini::RUNTIME, &chunk, Limits { ops: 100_000, ..Limits::sandbox() });
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
    });
}
