use super::access::Gate;
use super::{Access, Calls, ChannelSink, Closed, Completion, Fault, Held, HostModule, Project, Registered, Reply, Senders, Sink, WireValue, FILE};
use crate::kit::Descriptor;
use anyhow::Result;
use edge::embed::Limits;
use std::collections::VecDeque;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// the calls one package opened of a host module shared by each of its natives
pub type Opened = Arc<Mutex<Box<dyn Calls>>>;

/* a host module call by its index among the names it was served under */
#[derive(Clone)]
pub struct Hosted {
    pub calls: Opened,
    pub index: usize,
    pub name: Arc<str>,
}

/* what the composition wired which every run of its runtime serves whatever engine runs it */
pub struct Wiring {
    pub descriptor: &'static Descriptor,
    // the host modules the composition wired which every run of this runtime may import
    modules: Vec<Arc<dyn HostModule>>,
    // what trusted runs hold of them and the holder names each manifest field reserves
    access: Arc<dyn Access>,
    holders: Vec<(&'static str, &'static str)>,
    // what a run holds where its command line names no limit
    pub limits: Limits,
}

impl Wiring {
    pub fn new(descriptor: &'static Descriptor, modules: Vec<Arc<dyn HostModule>>, access: Arc<dyn Access>, holders: Vec<(&'static str, &'static str)>, limits: Limits) -> Wiring {
        Wiring { descriptor, modules, access, holders, limits }
    }

    pub fn modules(&self) -> &[Arc<dyn HostModule>] {
        &self.modules
    }

    /* the access of a run where an untrusted run with none reaches nothing */
    pub fn access(&self, project: &Project) -> Arc<dyn Access> {
        match (&project.access, project.untrusted) {
            (Some(access), _) => access.clone(),
            (None, true) => Arc::new(Closed),
            (None, false) => self.access.clone(),
        }
    }

    /* the field a walk carries for `access` then the holders it reserves one per line */
    pub(super) fn carried(&self, access: &dyn Access) -> String {
        let Some(field) = access.field() else { return String::new() };
        let holders = self.holders.iter().filter(|(_, by)| *by == field).map(|(holder, _)| *holder);
        std::iter::once(field).chain(holders).collect::<Vec<_>>().join("\n")
    }

    /* the module names the walk keeps any import from shadowing joined by newlines */
    pub(super) fn reserved(&self) -> String {
        self.modules.iter().map(|module| module.name()).collect::<Vec<_>>().join("\n")
    }
}

/* the host side of a run that its prints sends and host module calls reach on either engine */
pub struct Hosting {
    pub print: Sink,
    pub registered: Registered,
    // ids of host calls parked since the last dispatch whose answers arrive as completions
    pub deferred: Vec<u32>,
    // messages `send()` handed over or `None` outside an actor pool where no scheduler drains them
    pub outbox: Option<Vec<(String, String)>>,
    // the bytes the outbox holds and the most a run may leave there between steps
    pub outbox_bytes: usize,
    pub outbox_cap: usize,
    // where a run's channels take each line the moment the program sends it
    pub channel: Option<ChannelSink>,
    // the first line a channel could not take which ends the run after its step
    pub channel_error: Option<String>,
    // the packages the last walk met which decide who writes a file channel
    pub senders: Option<Senders>,
    // the group the first send went to that no outbox took
    pub unsent: Option<String>,
    // the channel the completions and events of the interpreter running now reach it through
    pub events: Option<Sender<Completion>>,
    // the host modules this run opened which close with it
    pub opened: Vec<Opened>,
    // the runtime that names the classes a failed host call raises
    pub descriptor: &'static Descriptor,
    // the module being opened with what its package holds which gates every call it serves
    pub opening: Option<(Arc<dyn HostModule>, Held)>,
}

impl Hosting {
    pub fn new(print: Sink, descriptor: &'static Descriptor, outbox_cap: usize) -> Hosting {
        Hosting { print, registered: Registered::new(), deferred: Vec::new(), outbox: None, outbox_bytes: 0, outbox_cap, channel: None, channel_error: None, senders: None, unsent: None, events: None, opened: Vec::new(), descriptor, opening: None }
    }

    /* where `spec` starts in the native table and its names with the natives `calls` adds from `base` on */
    pub fn open(&mut self, spec: &str, module: &str, names: &[&str], calls: Box<dyn Calls>, base: usize) -> (usize, Vec<String>, Vec<Hosted>) {
        // a spec this run already serves keeps the calls its package opened first
        if let Some((base, names)) = self.registered.get(spec).cloned() {
            return (base, names, Vec::new());
        }
        // the kit checks each call against the grant so no module can skip it
        let calls = match self.opening.clone() {
            Some((module, held)) => Box::new(Gate { module, held, names: names.iter().map(|name| name.to_string()).collect(), calls }),
            None => calls,
        };
        let calls: Opened = Arc::new(Mutex::new(calls));
        self.opened.push(calls.clone());
        let hosted = names.iter().enumerate().map(|(index, name)| Hosted { calls: calls.clone(), index, name: format!("{module}.{name}").into() }).collect();
        let names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        self.registered.insert(spec.to_string(), (base, names.clone()));
        (base, names, hosted)
    }

    /* runs one host module call as `call_id` with its answer now or none once the run parks for it */
    pub fn call(&mut self, hosted: &Hosted, call_id: u32, args: Vec<WireValue>) -> Result<Option<WireValue>, Fault> {
        let Some(events) = self.events.clone() else { return Err(Fault::runtime(format!("{} has no interpreter to answer", hosted.name))) };
        let (reply, settle) = Reply::new(call_id, events.clone(), hosted.name.clone());
        hosted.calls.lock().unwrap_or_else(|e| e.into_inner()).call(hosted.index, args, reply);
        let answer = settle.settle(call_id, &events).transpose()?;
        if answer.is_none() {
            self.deferred.push(call_id);
        }
        Ok(answer)
    }

    /* whether a send to `group` must name the package sending it before the host takes it */
    pub fn asks_sender(&self, group: &str) -> bool {
        group.starts_with(FILE) && self.senders.as_ref().is_some_and(|senders| !senders.alone())
    }

    /* hands a send of the package at `dir` to the channels or the outbox and is false if neither can */
    pub fn deliver(&mut self, group: String, body: String, dir: Option<String>) -> Result<bool, Fault> {
        if let Some(path) = group.strip_prefix(FILE)
            && let Some(dir) = dir
            && let Some(senders) = self.senders.as_mut()
        {
            senders.writes(&dir, path)?;
        }
        if let Some(channel) = self.channel.as_mut() {
            // a channel that failed takes nothing more and the run ends after its step
            if self.channel_error.is_none()
                && let Err(e) = channel(&group, &body)
            {
                self.channel_error = Some(e);
            }
            return Ok(true);
        }
        let Some(outbox) = self.outbox.as_mut() else {
            self.unsent.get_or_insert(group);
            return Ok(false);
        };
        let bytes = group.len() + body.len();
        if self.outbox_bytes.saturating_add(bytes) > self.outbox_cap {
            return Err(Fault::runtime(format!("the messages sent and not yet delivered would pass the {} MB the run may hold", self.outbox_cap >> 20)));
        }
        self.outbox_bytes += bytes;
        outbox.push((group, body));
        Ok(true)
    }

    /* everything `send()` queued since the last take */
    pub fn take_sends(&mut self) -> Vec<(String, String)> {
        self.outbox_bytes = 0;
        self.outbox.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /* forgets every module the run served and aborts what each opened one left running */
    pub fn forget(&mut self) {
        self.registered.clear();
        self.deferred.clear();
        self.close_opened();
    }

    /* aborts what every opened host module left running */
    pub(crate) fn close_opened(&mut self) {
        for calls in self.opened.drain(..) {
            calls.lock().unwrap_or_else(|e| e.into_inner()).close();
        }
    }
}

impl Drop for Hosting {
    fn drop(&mut self) {
        self.close_opened();
    }
}

/* the calls an interpreter parked and the messages it holds until its run parks on one */
pub struct Waits {
    rx: Receiver<Completion>,
    // calls the interpreter parked on moved out of its host side after every step
    pub parked: Vec<u32>,
    pub inflight: usize,
    buffered: VecDeque<String>,
}

impl Waits {
    /* the waits of an interpreter and the sender its host calls answer through */
    pub fn new() -> (Sender<Completion>, Waits) {
        let (events, rx) = channel();
        (events, Waits { rx, parked: Vec::new(), inflight: 0, buffered: VecDeque::new() })
    }

    /* moves the parked calls into the in-flight count whose answers arrive as completions */
    pub fn dispatch(&mut self) {
        self.inflight += std::mem::take(&mut self.parked).len();
    }

    /* blocks for the next completion then hands `deliver` all that arrived and returns the count */
    pub fn wait(&mut self, timeout: Option<Duration>, mut deliver: impl FnMut(Completion) -> Result<usize>) -> Result<usize> {
        let first = match timeout {
            Some(t) => self.rx.recv_timeout(t).ok(),
            None => self.rx.recv().ok(),
        };
        let Some(completion) = first else { return Ok(0) };
        self.settle(completion, &mut deliver)?;
        Ok(1 + self.poll(deliver)?)
    }

    /* hands `deliver` the completions already in without blocking */
    pub fn poll(&mut self, mut deliver: impl FnMut(Completion) -> Result<usize>) -> Result<usize> {
        let mut delivered = 0;
        while let Ok(completion) = self.rx.try_recv() {
            self.settle(completion, &mut deliver)?;
            delivered += 1;
        }
        Ok(delivered)
    }

    // a resumed plugin that waits again is already in flight on the same id
    fn settle(&mut self, completion: Completion, deliver: &mut impl FnMut(Completion) -> Result<usize>) -> Result<()> {
        self.inflight = self.inflight.saturating_sub(1);
        self.inflight += deliver(completion)?;
        Ok(())
    }

    /* wakes a parked `receive()` through `inject` or queues the message until the run parks on one */
    pub fn push(&mut self, message: &str, inject: impl FnOnce(&str) -> bool) -> bool {
        if inject(message) {
            return true;
        }
        self.buffered.push_back(message.to_string());
        false
    }

    /* hands queued messages to a run parked on an event and returns how many arrived */
    pub fn drain(&mut self, mut inject: impl FnMut(&str) -> bool) -> usize {
        let mut delivered = 0;
        while let Some(message) = self.buffered.front().cloned() {
            if !inject(&message) {
                break;
            }
            self.buffered.pop_front();
            delivered += 1;
        }
        delivered
    }
}
