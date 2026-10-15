use afl::fuzz;

use compiler::lexer::lex;
use compiler::parser::Parser;
use compiler::vm::snapshot;
use compiler::vm::types::{SchedulerStatus, VmErr};
use compiler::vm::{Limits, VM};

// Every run meters what it allocates, as compiler.wasm does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

// Tight so fuzzed loops preempt and unwind.
const PREEMPT_EVERY: usize = 7;
// Caps wake-ups so receive loops terminate.
const MAX_WAKEUPS: u32 = 16;

fn main() {
    fuzz!(|data: &[u8]| {
        // Source is text, reject non-UTF-8 rather than counting it as coverage.
        let Ok(src) = core::str::from_utf8(data) else { return };

        let (tokens, _lex_errs) = lex(src);
        let (chunk, parse_errs) = Parser::new(src, tokens.into_iter()).parse();

        // Only valid programs reach the VM, the chunk is unreliable after a parse error.
        if !parse_errs.is_empty() {
            return;
        }

        // Bounded budget turns runaway loops and allocations into VmErr, not hangs. Tight `ops` so bounded loops finish within AFL's hang timeout. Library default `sandbox()` is far larger.
        let limits = Limits { ops: 100_000, ..Limits::sandbox() };
        let mut vm = VM::with_limits(&chunk, limits);
        vm.set_preempt_interval(PREEMPT_EVERY);

        // Drive every park kind, snapshot the first.
        let mut hopped = false;
        let mut wakeups = 0;
        while let Err(VmErr::HostYield(park)) = vm.run() {
            let drivable = matches!(park, SchedulerStatus::Preempted | SchedulerStatus::PendingEvent);
            if !drivable { break; }

            if !hopped {
                hopped = true;
                // Every kind Python registers saves, a kind that refuses leaves the run where it is.
                if let Ok(blob) = snapshot::save(&vm, src) {
                    let mut fresh = VM::with_limits(&chunk, Limits { ops: 100_000, ..Limits::sandbox() });
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
