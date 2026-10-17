mod common;

// Every run meters what it allocates, as compiler.wasm does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

mod abi;
mod lexer;
mod parser;
mod vm;
mod modules;
mod snapshot;
mod coverage;
mod embed;
mod memory;

/* cases/pending/asyncio.json holds the cases written for the async builtins the engine used to ship, `run`, `sleep`,
   `gather`, `with_timeout` and `cancel`, grouped by the suite each came from: vm.json, snapshot.json, modules.json
   and the CLI hosts.json. The engine now implements only the coroutine protocol of Python, `await`, `send`,
   `throw`, `close` and async generators, and leaves scheduling to an event loop written in Python, so no suite
   loads this file and nothing in it runs. The cases come back once asyncio arrives transpiled with the rest of the
   standard library, each one rewritten from the old builtins to asyncio (`asyncio.run`, `asyncio.sleep`,
   `asyncio.gather`, `asyncio.wait_for`, `Task.cancel`) and moved back into the suite it came from, which is how
   they keep checking interleaving, cancellation, timeouts and host calls that wait inside a task. Three Rust tests
   of vm.rs went with them, a gather child whose host call fails pointing at its own call and leaving no stale
   position once caught, and a host call answered inside a `with_timeout`, and come back the same way. */
