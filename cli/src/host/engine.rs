use super::{ChannelSink, Fault};
use anyhow::{anyhow, Result};
use edge::abi::WireValue;
use edge::embed::Limits;
use std::time::Duration;

/* what a run step left the interpreter waiting on */
pub enum Status {
    Done,
    PendingEvent,
    PendingHostCall,
    Error(String),
    Exit(u8),
    Preempted,
}

pub enum Completion {
    Value { id: u32, value: WireValue },
    Error { id: u32, fault: Fault },
}

/* one interpreter of the kit's runtime whichever engine its composition runs it on */
pub trait Engine {
    /* resolves imports and starts a fresh run with resolution failures read as compile errors */
    fn start(&mut self, src: &str, input: Option<&str>) -> Result<Status>;

    /* runs one more input on the persistent interpreter without running the history again */
    fn repl_eval(&mut self, src: &str, input: Option<&str>) -> Result<Status>;

    fn resume(&mut self) -> Result<Status>;

    /* runs the next input as `entry` whose directory resolves its relative imports */
    fn set_entry(&mut self, entry: &str);

    fn save_state(&mut self) -> Result<Option<Vec<u8>>>;

    /* boots from the source the blob embeds whose imports must resolve again */
    fn restore_state(&mut self, blob: &[u8]) -> Result<Status>;

    /* caps the next boot with the limits of a group or the sandbox profile */
    fn set_limits(&mut self, limits: &Limits) -> Result<()>;

    fn set_preempt_interval(&mut self, n: usize) -> Result<()>;

    /* drops every module registration so the next input starts in a fresh namespace */
    fn reset(&mut self) -> Result<()>;

    /* wakes a parked `receive()` or queues the message until the run parks on one */
    fn push_event(&mut self, message: &str) -> bool;

    /* moves the parked calls into the in-flight count whose answers arrive as completions */
    fn dispatch(&mut self);

    fn inflight(&self) -> usize;

    /* blocks for the next completion then injects all that arrived and returns the count */
    fn wait(&mut self, timeout: Option<Duration>) -> Result<usize>;

    /* hands queued messages to a run parked on an event and returns how many arrived */
    fn drain_buffered(&mut self) -> usize;

    /* whether a failure left the interpreter unfit to run again */
    fn poisoned(&self) -> bool;

    /* hands every line a run sends to `channel` the moment the program sends it */
    fn send_to(&mut self, channel: ChannelSink);

    /* why a channel of the run failed to take a line since the last ask */
    fn channel_error(&mut self) -> Option<String>;
}

/* the source a snapshot blob embeds read by the engine that wrote its header */
pub fn snapshot_source(blob: &[u8]) -> Result<String> {
    edge::embed::source_of(blob).map(String::from).map_err(|e| anyhow!(e))
}
