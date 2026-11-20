use super::{resolver::{Fetched, Remote, Walk}, Engine, Host, Instance, Loaded, Project, Sink};
use crate::engine::cli::kit::Descriptor;
use anyhow::Result;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(feature = "cli-library")]
use super::{library, Wiring};

/* the runtime an actor pool shares across its scheduler threads */
#[derive(Clone)]
pub(crate) enum Runtime {
    Wasm(Arc<Loaded>),
    #[cfg(feature = "cli-library")]
    Native(Arc<library::Library>, Arc<Wiring>),
}

impl Runtime {
    /* advances the epoch untrusted groups count their deadlines in */
    pub(crate) fn start_ticker(&self) {
        match self {
            Runtime::Wasm(loaded) => loaded.start_ticker(),
            #[cfg(feature = "cli-library")]
            Runtime::Native(..) => {}
        }
    }

    pub(crate) fn native(&self) -> bool {
        !matches!(self, Runtime::Wasm(_))
    }
}

/* what one scheduler thread makes its instances and interpreters from */
pub(crate) enum PoolHost {
    Wasm(Rc<Host>),
    #[cfg(feature = "cli-library")]
    Native(Arc<library::Library>, Arc<Wiring>),
}

impl PoolHost {
    pub(crate) fn new(runtime: &Runtime) -> Result<PoolHost> {
        Ok(match runtime {
            Runtime::Wasm(loaded) => PoolHost::Wasm(Host::new(loaded.clone())?),
            #[cfg(feature = "cli-library")]
            Runtime::Native(lib, wiring) => PoolHost::Native(lib.clone(), wiring.clone()),
        })
    }

    pub(crate) fn descriptor(&self) -> &'static Descriptor {
        match self {
            PoolHost::Wasm(host) => host.descriptor(),
            #[cfg(feature = "cli-library")]
            PoolHost::Native(lib, _) => lib.descriptor,
        }
    }

    /* an instance whose slots the actors of a group run in */
    pub(crate) fn instance(&self, sink: Sink, project: Project, deadline: Option<u64>, memory: usize) -> Result<PoolInstance> {
        Ok(match self {
            PoolHost::Wasm(host) => PoolInstance::Wasm(host.instance(sink, project, deadline, memory)?),
            #[cfg(feature = "cli-library")]
            PoolHost::Native(lib, wiring) => PoolInstance::Native(Rc::new(RefCell::new(library::Instance::fresh(lib.clone(), wiring.clone(), sink, project)?))),
        })
    }

    /* one capped interpreter for an untrusted program which only the sandbox of a wasm runtime bounds */
    pub(crate) fn vm(&self, sink: Sink, project: Project, deadline: Option<u64>, memory: Option<usize>) -> Result<PoolVm> {
        match self {
            PoolHost::Wasm(host) => Ok(Box::new(host.vm(sink, project, deadline, memory)?)),
            #[cfg(feature = "cli-library")]
            PoolHost::Native(..) => anyhow::bail!("an eval group runs untrusted code so it runs only on a wasm runtime"),
        }
    }
}

/* one runtime instance of a group with the interpreter slots it holds */
#[derive(Clone)]
pub(crate) enum PoolInstance {
    Wasm(Rc<RefCell<Instance>>),
    #[cfg(feature = "cli-library")]
    Native(Rc<RefCell<library::Instance>>),
}

impl PoolInstance {
    pub(crate) fn slots(&self) -> usize {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow().slots(),
            #[cfg(feature = "cli-library")]
            PoolInstance::Native(inst) => inst.borrow().slots(),
        }
    }

    pub(crate) fn poisoned(&self) -> bool {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow().poisoned(),
            #[cfg(feature = "cli-library")]
            PoolInstance::Native(inst) => inst.borrow().poisoned(),
        }
    }

    pub(crate) fn accept_sends(&self) {
        match self {
            PoolInstance::Wasm(inst) => inst.borrow_mut().accept_sends(),
            #[cfg(feature = "cli-library")]
            PoolInstance::Native(inst) => inst.borrow_mut().accept_sends(),
        }
    }

    /* another interpreter slot of this instance that frees when its vm drops */
    pub(crate) fn slot(&self) -> Result<PoolVm> {
        Ok(match self {
            PoolInstance::Wasm(inst) => Box::new(Instance::slot(inst)?),
            #[cfg(feature = "cli-library")]
            PoolInstance::Native(inst) => Box::new(library::Vm::slot(inst)?),
        })
    }
}

/* one interpreter of an actor whichever runtime the pool runs on */
pub(crate) type PoolVm = Box<dyn Pooled>;

/* what the scheduler asks of an actor's interpreter beyond what any engine answers */
pub(crate) trait Pooled: Engine {
    /* injects the completions already in without blocking */
    fn poll(&mut self) -> Result<usize>;

    /* everything `send()` queued since the last take */
    fn take_sends(&mut self) -> Vec<(String, String)>;

    /* the group a send of this instance went to that no scheduler took */
    fn unsent(&self) -> Option<String>;

    /* begins the import walk of `src` which `walk_on` steps where its caller chooses */
    fn walk(&mut self, src: &str) -> Result<Walk, String>;

    /* steps `walk` with what a fetch gave until it waits on another file or ends */
    fn walk_on(&mut self, walk: &mut Walk, fetched: Option<Fetched>) -> Result<Option<Remote>, String>;
}
