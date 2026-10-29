use cli::host::{Calls, Fault, Held, HostModule, Reply, Serve, WireValue};
use std::time::{SystemTime, UNIX_EPOCH};

/* the milliseconds a check reads from `now` once its manifest grants clock */
pub struct Clock(pub fn() -> u64);

impl HostModule for Clock {
    fn name(&self) -> &str {
        "clock"
    }

    // a grant gives the clock whole so it takes no scope
    fn scope(&self, _scope: &str) -> bool {
        false
    }

    // so no call of it reaches a scope the kit checks
    fn reaches(&self, _call: &str, _args: &[WireValue]) -> Result<Option<String>, Fault> {
        Ok(None)
    }

    fn open(&self, inst: &mut dyn Serve, spec: &str, _pkg: &str, _held: Held) -> Result<(), String> {
        inst.serve(spec, self.name(), &["now"], Box::new(Now(self.0)))
    }
}

struct Now(fn() -> u64);

impl Calls for Now {
    fn call(&mut self, _index: usize, _args: Vec<WireValue>, reply: Reply) {
        reply.ok(WireValue::Int((self.0)().into()));
    }
}

/* the wall clock in milliseconds since the epoch */
pub fn wall() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
