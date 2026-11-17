use super::{ChannelSink, Engine, Fetched, Host, Instance, Loaded, Project, Remote, Sink, Status, Walk};
use crate::kit::Descriptor;
use anyhow::Result;
use interpreter::embed::Limits;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "native")]
use super::{library, Wiring};

/* the runtime an actor pool shares across its scheduler threads */
#[derive(Clone)]
pub enum Runtime {
    Wasm(Arc<Loaded>),
    #[cfg(feature = "native")]
    Native(Arc<library::Library>, Arc<Wiring>),
}

impl Runtime {
    /* advances the epoch untrusted groups count their deadlines in */
    pub fn start_ticker(&self) {
        match self {
            Runtime::Wasm(loaded) => loaded.start_ticker(),
            #[cfg(feature = "native")]
            Runtime::Native(..) => {}
        }
    }

    pub fn native(&self) -> bool {
        !matches!(self, Runtime::Wasm(_))
    }
}

/* what one scheduler thread makes its instances and interpreters from */
pub enum PoolHost {
    Wasm(Rc<Host>),
    #[cfg(feature = "native")]
    Native(Arc<library::Library>, Arc<Wiring>),
}

impl PoolHost {
    pub fn new(runtime: &Runtime) -> Result<PoolHost> {
        Ok(match runtime {
            Runtime::Wasm(loaded) => PoolHost::Wasm(Host::new(loaded.clone())?),
            #[cfg(feature = "native")]
            Runtime::Native(lib, wiring) => PoolHost::Native(lib.clone(), wiring.clone()),
        })
    }

    pub fn descriptor(&self) -> &'static Descriptor {
        match self {
            PoolHost::Wasm(host) => host.descriptor(),
            #[cfg(feature = "native")]
            PoolHost::Native(lib, _) => lib.descriptor,
        }
    }

    /* an instance whose slots the actors of a group run in */
    pub fn instance(&self, sink: Sink, project: Project, deadline: Option<u64>, memory: usize) -> Result<PoolInstance> {
        Ok(match self {
            PoolHost::Wasm(host) => PoolInstance::Wasm(host.instance(sink, project, deadline, memory)?),
            #[cfg(feature = "native")]
            PoolHost::Native(lib, wiring) => PoolInstance::Native(Rc::new(RefCell::new(library::Instance::fresh(lib.clone(), wiring.clone(), sink, project)?))),
        })
    }

    /* one capped interpreter for an untrusted program which only the sandbox of a wasm runtime bounds */
    pub fn vm(&self, sink: Sink, project: Project, deadline: Option<u64>, memory: Option<usize>) -> Result<PoolVm> {
        match self {
            PoolHost::Wasm(host) => Ok(PoolVm::Wasm(host.vm(sink, project, deadline, memory)?)),
            #[cfg(feature = "native")]
            PoolHost::Native(..) => anyhow::bail!("an eval group runs untrusted code so it runs only on a wasm runtime"),
        }
    }
}

/* one runtime instance of a group with the interpreter slots it holds */
#[derive(Clone)]
pub enum PoolInstance {
    Wasm(Rc<RefCell<Instance>>),
    #[cfg(feature = "native")]
    Native(Rc<RefCell<library::Instance>>),
}

impl PoolInstance {
    pub fn slots(&self) -> usize {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow().slots(),
            #[cfg(feature = "native")]
            PoolInstance::Native(inst) => inst.borrow().slots(),
        }
    }

    pub fn poisoned(&self) -> bool {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow().poisoned(),
            #[cfg(feature = "native")]
            PoolInstance::Native(inst) => inst.borrow().poisoned(),
        }
    }

    pub fn accept_sends(&self) {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow_mut().accept_sends(),
            #[cfg(feature = "native")]
            PoolInstance::Native(inst) => inst.borrow_mut().accept_sends(),
        }
    }

    /* another interpreter slot of this instance that frees when its vm drops */
    pub fn slot(&self) -> Result<PoolVm> {
        Ok(match self {
            PoolInstance::Wasm(inst) => PoolVm::Wasm(Instance::slot(inst)?),
            #[cfg(feature = "native")]
            PoolInstance::Native(inst) => PoolVm::Native(library::Vm::slot(inst)?),
        })
    }
}

/* one interpreter of an actor whichever runtime the pool runs on */
pub enum PoolVm {
    Wasm(super::Vm),
    #[cfg(feature = "native")]
    Native(library::Vm),
}

// each call goes to the interpreter of the runtime the pool runs on
macro_rules! each {
    ($self:expr, $vm:ident => $body:expr) => {
        match $self {
            PoolVm::Wasm($vm) => $body,
            #[cfg(feature = "native")]
            PoolVm::Native($vm) => $body,
        }
    };
}

impl PoolVm {
    pub fn poll(&mut self) -> Result<usize> {
        each!(self, vm => vm.poll())
    }

    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        each!(self, vm => vm.take_sends())
    }

    pub fn unsent(&self) -> Option<String> {
        each!(self, vm => vm.unsent())
    }

    pub fn walk(&mut self, src: &str) -> Result<Walk, String> {
        each!(self, vm => vm.walk(src))
    }

    pub fn walk_on(&mut self, walk: &mut Walk, fetched: Option<Fetched>) -> Result<Option<Remote>, String> {
        each!(self, vm => vm.walk_on(walk, fetched))
    }

    /* whether the instance this interpreter runs in failed and runs nothing again */
    pub fn instance_poisoned(&self) -> bool {
        each!(self, vm => vm.instance().borrow().poisoned())
    }
}

impl Engine for PoolVm {
    fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        each!(self, vm => vm.start(src, input))
    }

    fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status> {
        each!(self, vm => vm.repl_eval(src, input))
    }

    fn resume(&mut self) -> Result<Status> {
        each!(self, vm => vm.resume())
    }

    fn set_entry(&mut self, entry: &str) {
        each!(self, vm => vm.set_entry(entry))
    }

    fn save_state(&mut self) -> Result<Option<Vec<u8>>> {
        each!(self, vm => vm.save_state())
    }

    fn restore_state(&mut self, blob: &[u8]) -> Result<Status> {
        each!(self, vm => vm.restore_state(blob))
    }

    fn set_limits(&mut self, limits: &Limits) -> Result<()> {
        each!(self, vm => vm.set_limits(limits))
    }

    fn set_preempt_interval(&mut self, n: usize) -> Result<()> {
        each!(self, vm => vm.set_preempt_interval(n))
    }

    fn reset(&mut self) -> Result<()> {
        each!(self, vm => vm.reset())
    }

    fn push_event(&mut self, message: &str) -> bool {
        each!(self, vm => vm.push_event(message))
    }

    fn dispatch(&mut self) {
        each!(self, vm => vm.dispatch())
    }

    fn inflight(&self) -> usize {
        each!(self, vm => vm.inflight())
    }

    fn wait(&mut self, timeout: Option<Duration>) -> Result<usize> {
        each!(self, vm => vm.wait(timeout))
    }

    fn drain_buffered(&mut self) -> usize {
        each!(self, vm => vm.drain_buffered())
    }

    fn poisoned(&self) -> bool {
        each!(self, vm => vm.poisoned())
    }

    fn send_to(&mut self, channel: ChannelSink) {
        each!(self, vm => vm.send_to(channel))
    }

    fn channel_error(&mut self) -> Option<String> {
        each!(self, vm => vm.channel_error())
    }
}

